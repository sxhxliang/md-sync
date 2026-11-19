//! Google Drive backend (Drive API v3).
//!
//! Drive is not a path filesystem: files live in folders by ID. This
//! backend resolves paths to IDs on demand (one `files.list` per folder
//! level, cached only within a single call) and keeps the public
//! interface identical to every other backend.
//!
//! Authentication is deliberately *not* in this crate: the application
//! owns OAuth2 and hands the backend a [`TokenProvider`] — a static token
//! or a callback that returns a fresh access token. The provider should
//! cache: it is called once per HTTP request.
//!
//! Limits: simple + multipart upload only (no resumable upload —
//! markdown files are small); delete moves to trash; `modifiedTime` is
//! settable, so `set_modified` is implemented.

use async_trait::async_trait;
use md_sync_core::backend::{Builder, TokenProvider};
use md_sync_core::{
    default_content_type, Entry, Precondition, RelPath, Result, StorageBackend, SyncError,
    WriteOptions,
};
use md_sync_http::date::{format_rfc3339, parse_rfc3339};
use md_sync_http::encode::encode_segment;
use md_sync_http::{parse_json, Auth, HttpClient};
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::SystemTime;

const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FIELDS: &str = "files(id,name,mimeType,size,modifiedTime,md5Checksum,etag),nextPageToken";

pub struct GoogleDriveBackend {
    http: HttpClient,
    tokens: Arc<dyn TokenProvider>,
    root_folder_id: String,
}

#[derive(Default)]
pub struct GoogleDriveBuilder {
    tokens: Option<Arc<dyn TokenProvider>>,
    root_folder_id: String,
}

impl GoogleDriveBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A [`TokenProvider`]: static token or a refresh callback.
    pub fn token_provider(mut self, p: Arc<dyn TokenProvider>) -> Self {
        self.tokens = Some(p);
        self
    }

    /// Convenience: a fixed access token string.
    pub fn access_token(mut self, token: impl Into<String>) -> Self {
        self.tokens = Some(Arc::new(md_sync_core::StaticToken(token.into())));
        self
    }

    /// Folder ID to sync inside. Default `"root"` (the Drive root).
    pub fn root_folder_id(mut self, id: impl Into<String>) -> Self {
        self.root_folder_id = id.into();
        self
    }
}

impl Builder for GoogleDriveBuilder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        let tokens = self.tokens.ok_or_else(|| {
            SyncError::InvalidConfig(
                "gdrive backend: access token (token provider) is required".into(),
            )
        })?;
        Ok(Arc::new(GoogleDriveBackend {
            http: HttpClient::new()?,
            tokens,
            root_folder_id: if self.root_folder_id.is_empty() {
                "root".to_string()
            } else {
                self.root_folder_id
            },
        }))
    }
}

struct DriveFile {
    id: String,
    name: String,
    is_folder: bool,
    size: u64,
    mtime: SystemTime,
    /// Resource etag: the revision token `If-Match` understands.
    /// (The md5Checksum is content-only; the resource etag also moves
    /// on metadata changes, which is what a CAS writer wants.)
    etag: Option<String>,
    mime_type: Option<String>,
}

fn file_from_json(v: &Value) -> DriveFile {
    DriveFile {
        id: v["id"].as_str().unwrap_or("").to_string(),
        name: v["name"].as_str().unwrap_or("").to_string(),
        is_folder: v["mimeType"].as_str().unwrap_or("") == FOLDER_MIME,
        size: v["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
        mtime: v["modifiedTime"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        etag: v["etag"].as_str().map(|s| s.to_string()),
        mime_type: v["mimeType"].as_str().map(|s| s.to_string()),
    }
}

impl GoogleDriveBackend {
    fn auth(&self) -> Result<Auth> {
        Ok(Auth::Bearer(self.tokens.token()?))
    }

    /// `files.list` for one folder. `query` is the full `q` value.
    async fn list_files(
        &self,
        query: &str,
        page_token: Option<&str>,
    ) -> Result<(Vec<DriveFile>, Option<String>)> {
        let mut url = format!(
            "{API}/files?q={}&fields={}&pageSize=1000",
            encode_segment(query),
            encode_segment(FIELDS)
        );
        if let Some(t) = page_token {
            url.push_str("&pageToken=");
            url.push_str(&encode_segment(t));
        }
        let body = self.http.get(&url, &self.auth()?).await?;
        let v: Value = parse_json(&body)?;
        let files = v["files"]
            .as_array()
            .map(|a| a.iter().map(file_from_json).collect())
            .unwrap_or_default();
        let next = v["nextPageToken"].as_str().map(|s| s.to_string());
        Ok((files, next))
    }

    async fn children(&self, folder_id: &str) -> Result<Vec<DriveFile>> {
        let q = format!("'{folder_id}' in parents and trashed = false");
        let mut all = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let (files, next) = self.list_files(&q, page.as_deref()).await?;
            all.extend(files);
            match next {
                Some(t) => page = Some(t),
                None => break,
            }
        }
        Ok(all)
    }

    /// Find a direct child of `folder_id` by name.
    async fn find_child(&self, folder_id: &str, name: &str) -> Result<Option<DriveFile>> {
        let q = format!("'{folder_id}' in parents and name = '{name}' and trashed = false");
        let (files, _) = self.list_files(&q, None).await?;
        Ok(files.into_iter().next())
    }

    /// Resolve a relative path to a file ID (folders included).
    async fn resolve(&self, path: &RelPath) -> Result<Option<DriveFile>> {
        let mut folder = self.root_folder_id.clone();
        let mut segs: Vec<&str> = path.as_str().split('/').collect();
        let last = segs.pop().unwrap_or("");
        for seg in segs {
            match self.find_child(&folder, seg).await? {
                Some(f) if f.is_folder => folder = f.id,
                _ => return Ok(None),
            }
        }
        self.find_child(&folder, last).await
    }

    /// Resolve (creating missing folders) the parent folder ID of `path`.
    async fn resolve_parent(&self, path: &RelPath) -> Result<String> {
        let mut folder = self.root_folder_id.clone();
        if let Some(parent) = path.parent() {
            for seg in parent.as_str().split('/') {
                match self.find_child(&folder, seg).await? {
                    Some(f) if f.is_folder => folder = f.id,
                    Some(_) => {
                        return Err(SyncError::Backend(format!("{seg} is a file, not a folder")))
                    }
                    None => {
                        let body = self
                            .http
                            .request(
                                Method::POST,
                                &format!("{API}/files"),
                                &self.auth()?,
                                &[("Content-Type", "application/json")],
                                Some(
                                    serde_json::to_vec(&json!({
                                        "name": seg,
                                        "mimeType": FOLDER_MIME,
                                        "parents": [folder],
                                    }))
                                    .unwrap(),
                                ),
                            )
                            .await?
                            .1;
                        let v: Value = parse_json(&body)?;
                        folder = v["id"].as_str().unwrap_or("").to_string();
                    }
                }
            }
        }
        Ok(folder)
    }

    async fn list_recursive(
        &self,
        folder_id: &str,
        rel_prefix: &str,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        for f in self.children(folder_id).await? {
            let rel = if rel_prefix.is_empty() {
                f.name.clone()
            } else {
                format!("{}/{}", rel_prefix, f.name)
            };
            if f.is_folder {
                Box::pin(self.list_recursive(&f.id, &rel, out)).await?;
            } else if let Ok(rp) = RelPath::new(&rel) {
                let mut e = Entry::new(rp, f.size, f.mtime);
                e.etag = f.etag;
                e.content_type = f.mime_type;
                out.push(e);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl StorageBackend for GoogleDriveBackend {
    fn name(&self) -> &'static str {
        "gdrive"
    }

    fn describe(&self) -> String {
        format!("gdrive:{}", self.root_folder_id)
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.list_recursive(&self.root_folder_id.clone(), "", &mut out)
            .await?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let file = self
            .resolve(path)
            .await?
            .ok_or_else(|| SyncError::NotFound(path.to_string()))?;
        self.http
            .get(&format!("{API}/files/{}?alt=media", file.id), &self.auth()?)
            .await
    }

    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let file = self
            .resolve(path)
            .await?
            .ok_or_else(|| SyncError::NotFound(path.to_string()))?;
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        Ok(self
            .http
            .request(
                Method::GET,
                &format!("{API}/files/{}?alt=media", file.id),
                &self.auth()?,
                &[("Range", range.as_str())],
                None,
            )
            .await?
            .1)
    }

    async fn read_if_match(&self, path: &RelPath, etag: &str) -> Result<Vec<u8>> {
        let file = self
            .resolve(path)
            .await?
            .ok_or_else(|| SyncError::NotFound(path.to_string()))?;
        Ok(self
            .http
            .request(
                Method::GET,
                &format!("{API}/files/{}?alt=media", file.id),
                &self.auth()?,
                &[("If-Match", etag)],
                None,
            )
            .await?
            .1)
    }

    async fn write(&self, path: &RelPath, data: &[u8]) -> Result<Entry> {
        self.write_with_options(path, data, WriteOptions::default())
            .await
    }

    async fn write_with_options(
        &self,
        path: &RelPath,
        data: &[u8],
        opts: WriteOptions,
    ) -> Result<Entry> {
        let parent = self.resolve_parent(path).await?;
        let existing = self.resolve(path).await?;
        // Precondition check before touching the server: Drive has no
        // atomic "create only if absent", so IfNoneMatch is a resolve
        // check; IfMatch is enforced again by the server header.
        match &opts.precondition {
            Precondition::Any => {}
            Precondition::IfMatch(etag) => match &existing {
                Some(f) if f.etag.as_deref() == Some(etag.as_str()) => {}
                _ => {
                    return Err(SyncError::PreconditionFailed(format!(
                        "gdrive: {path} etag does not match {etag}"
                    )));
                }
            },
            Precondition::IfNoneMatch => {
                if existing.is_some() {
                    return Err(SyncError::PreconditionFailed(format!(
                        "gdrive: {path} already exists"
                    )));
                }
            }
        }
        let content_type = opts
            .content_type
            .unwrap_or_else(|| default_content_type(path).to_string());
        let body: Vec<u8> = match existing {
            Some(f) => {
                // Update media of the existing file.
                let mut headers = vec![("Content-Type".to_string(), content_type.clone())];
                if let Precondition::IfMatch(etag) = &opts.precondition {
                    headers.push(("If-Match".to_string(), etag.clone()));
                }
                let (_, _, resp) = self
                    .http
                    .request_full(
                        Method::PATCH,
                        &format!("{UPLOAD_API}/files/{}?uploadType=media", f.id),
                        &self.auth()?,
                        &headers,
                        Some(data.to_vec()),
                    )
                    .await?;
                resp
            }
            None => {
                // Multipart create: metadata + media in one request.
                let boundary = "md-sync-boundary";
                let meta = serde_json::to_vec(&json!({
                    "name": path.file_name(),
                    "parents": [parent],
                }))
                .unwrap();
                let mut multipart = Vec::new();
                multipart.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                multipart
                    .extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
                multipart.extend_from_slice(&meta);
                multipart.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
                multipart
                    .extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
                multipart.extend_from_slice(data);
                multipart.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
                let (_, _, resp) = self
                    .http
                    .request_full(
                        Method::POST,
                        &format!("{UPLOAD_API}/files?uploadType=multipart"),
                        &self.auth()?,
                        &[(
                            "Content-Type".to_string(),
                            format!("multipart/related; boundary={boundary}"),
                        )],
                        Some(multipart),
                    )
                    .await?;
                resp
            }
        };
        let v: Value = parse_json(&body)?;
        let f = file_from_json(&v);
        let mut e = Entry::new(path.clone(), f.size, f.mtime);
        e.etag = f.etag;
        e.content_type = f.mime_type;
        Ok(e)
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        let Some(f) = self.resolve(path).await? else {
            return Ok(());
        };
        // Trash, don't permanently delete.
        self.http
            .request(
                Method::PATCH,
                &format!("{API}/files/{}", f.id),
                &self.auth()?,
                &[("Content-Type", "application/json")],
                Some(br#"{"trashed": true}"#.to_vec()),
            )
            .await?;
        Ok(())
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        Ok(self.resolve(path).await?.map(|f| {
            let mut e = Entry::new(path.clone(), f.size, f.mtime);
            e.etag = f.etag;
            e.content_type = f.mime_type;
            e
        }))
    }

    async fn mkdir(&self, path: &RelPath) -> Result<()> {
        let mut folder = self.root_folder_id.clone();
        for seg in path.as_str().split('/') {
            match self.find_child(&folder, seg).await? {
                Some(f) if f.is_folder => folder = f.id,
                Some(_) => {
                    return Err(SyncError::Backend(format!("{seg} is a file, not a folder")))
                }
                None => {
                    let body = self
                        .http
                        .request(
                            Method::POST,
                            &format!("{API}/files"),
                            &self.auth()?,
                            &[("Content-Type", "application/json")],
                            Some(
                                serde_json::to_vec(&json!({
                                    "name": seg,
                                    "mimeType": FOLDER_MIME,
                                    "parents": [folder],
                                }))
                                .unwrap(),
                            ),
                        )
                        .await?
                        .1;
                    let v: Value = parse_json(&body)?;
                    folder = v["id"].as_str().unwrap_or("").to_string();
                }
            }
        }
        Ok(())
    }

    async fn set_modified(&self, path: &RelPath, time: SystemTime) -> Result<()> {
        let Some(f) = self.resolve(path).await? else {
            return Ok(());
        };
        self.http
            .request(
                Method::PATCH,
                &format!("{API}/files/{}", f.id),
                &self.auth()?,
                &[("Content-Type", "application/json")],
                Some(serde_json::to_vec(&json!({ "modifiedTime": format_rfc3339(time) })).unwrap()),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_list_json_parses() {
        let body = br#"{"files":[
          {"id":"1","name":"todo.md","mimeType":"text/markdown","size":"42","modifiedTime":"2026-11-18T10:00:00.000Z","md5Checksum":"abc123","etag":"etag-1"},
          {"id":"2","name":"sub","mimeType":"application/vnd.google-apps.folder"}
        ],"nextPageToken":"tok"}"#;
        let v: Value = parse_json(body).unwrap();
        let files: Vec<DriveFile> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(file_from_json)
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "todo.md");
        assert_eq!(files[0].size, 42);
        assert_eq!(files[0].etag.as_deref(), Some("etag-1"));
        assert_eq!(files[0].mime_type.as_deref(), Some("text/markdown"));
        assert!(!files[0].is_folder);
        assert!(files[1].is_folder);
        assert_eq!(v["nextPageToken"].as_str(), Some("tok"));
    }
}
