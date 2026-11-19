//! Microsoft OneDrive backend (Microsoft Graph API).
//!
//! Path-based addressing (`/drives/{id}/root:/path:/content`) is used for
//! read/write/delete/stat, so no ID resolution is needed for single files;
//! listing walks `children` by item ID. Like Drive, authentication is a
//! [`TokenProvider`] supplied by the application — no OAuth in this crate.
//!
//! Limits: simple upload only (files < 4 MB — the Graph simple-upload
//! ceiling; markdown documents are far below it); delete is permanent
//! (Graph has no trash call for a single item delete — the item goes to
//! the recycle bin server-side, restorable from OneDrive web).

use async_trait::async_trait;
use md_sync_core::backend::{Builder, TokenProvider};
use md_sync_core::{
    default_content_type, Entry, Precondition, RelPath, Result, StorageBackend, SyncError,
    WriteOptions,
};
use md_sync_http::date::{format_rfc3339, parse_rfc3339};
use md_sync_http::encode::encode_path;
use md_sync_http::{parse_json, Auth, HttpClient};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::SystemTime;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Upload-session chunk size: a multiple of Graph's required 320 KiB.
const CHUNK: usize = 8 * 1024 * 1024;
const SELECT: &str = "id,name,size,lastModifiedDateTime,eTag,file,folder";

pub struct OneDriveBackend {
    http: HttpClient,
    tokens: Arc<dyn TokenProvider>,
    /// `/me/drive` by default, or `/drives/{drive-id}`.
    drive: String,
}

#[derive(Default)]
pub struct OneDriveBuilder {
    tokens: Option<Arc<dyn TokenProvider>>,
    drive_id: Option<String>,
}

impl OneDriveBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn token_provider(mut self, p: Arc<dyn TokenProvider>) -> Self {
        self.tokens = Some(p);
        self
    }

    pub fn access_token(mut self, token: impl Into<String>) -> Self {
        self.tokens = Some(Arc::new(md_sync_core::StaticToken(token.into())));
        self
    }

    /// A specific drive ID. Without it, the signed-in user's default drive.
    pub fn drive_id(mut self, id: impl Into<String>) -> Self {
        self.drive_id = Some(id.into());
        self
    }
}

impl Builder for OneDriveBuilder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        let tokens = self.tokens.ok_or_else(|| {
            SyncError::InvalidConfig(
                "onedrive backend: access token (token provider) is required".into(),
            )
        })?;
        let drive = match self.drive_id {
            Some(id) => format!("/drives/{id}"),
            None => "/me/drive".to_string(),
        };
        Ok(Arc::new(OneDriveBackend {
            http: HttpClient::new()?,
            tokens,
            drive,
        }))
    }
}

fn item_to_entry(item: &Value, rel: &RelPath) -> Option<Entry> {
    if item.get("folder").is_some() {
        return None;
    }
    let size = item["size"].as_u64().unwrap_or(0);
    let mtime = item["lastModifiedDateTime"]
        .as_str()
        .and_then(parse_rfc3339)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut e = Entry::new(rel.clone(), size, mtime);
    e.etag = item["eTag"].as_str().map(|s| s.to_string());
    e.content_type = item["file"]["mimeType"].as_str().map(|s| s.to_string());
    Some(e)
}

impl OneDriveBackend {
    fn auth(&self) -> Result<Auth> {
        Ok(Auth::Bearer(self.tokens.token()?))
    }

    /// `GET {drive}/items/{id}/children`, following `@odata.nextLink`.
    async fn children(&self, item_id: &str) -> Result<Vec<Value>> {
        let mut url = format!(
            "{GRAPH}{}/items/{item_id}/children?$select={SELECT}&$top=999",
            self.drive
        );
        let mut all = Vec::new();
        loop {
            let body = self.http.get(&url, &self.auth()?).await?;
            let v: Value = parse_json(&body)?;
            if let Some(arr) = v["value"].as_array() {
                all.extend(arr.iter().cloned());
            }
            match v["@odata.nextLink"].as_str() {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(all)
    }

    async fn list_recursive(
        &self,
        item_id: &str,
        rel_prefix: &str,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        for item in self.children(item_id).await? {
            let name = item["name"].as_str().unwrap_or("");
            let rel = if rel_prefix.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", rel_prefix, name)
            };
            if item.get("folder").is_some() {
                let id = item["id"].as_str().unwrap_or("");
                Box::pin(self.list_recursive(id, &rel, out)).await?;
            } else if let Ok(rp) = RelPath::new(&rel) {
                if let Some(e) = item_to_entry(&item, &rp) {
                    out.push(e);
                }
            }
        }
        Ok(())
    }

    /// Path-based item URL: `{drive}/root:/notes/a.md` (no trailing `:`).
    fn item_url(&self, path: &RelPath) -> String {
        format!("{GRAPH}{}/root:/{}", self.drive, encode_path(path.as_str()))
    }

    /// Path-based URL with a suffix after the colon, e.g. `:/content`.
    fn item_url_suffix(&self, path: &RelPath, suffix: &str) -> String {
        format!("{}:{suffix}", self.item_url(path))
    }

    /// `(item id, eTag)` for a path, or `None` when it does not exist.
    async fn resolve_item(&self, path: &RelPath) -> Result<Option<(String, String)>> {
        let url = format!("{}?$select=id,eTag", self.item_url(path));
        match self.http.get(&url, &self.auth()?).await {
            Ok(body) => {
                let v: Value = parse_json(&body)?;
                let id = v["id"].as_str().unwrap_or("").to_string();
                let etag = v["eTag"].as_str().unwrap_or("").to_string();
                if id.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some((id, etag)))
                }
            }
            Err(SyncError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Upload via a Graph upload session (files >= 4 MiB). Chunk size is
    /// 8 MiB, a multiple of Graph's required 320 KiB. The session URL is
    /// preauthenticated: the bearer token is never forwarded to it.
    /// Conditional writes are pre-checked against the current eTag (the
    /// session itself carries no fence); `fail` makes session creation
    /// refuse an existing name for `IfNoneMatch`.
    async fn upload_session(
        &self,
        path: &RelPath,
        data: &[u8],
        opts: &WriteOptions,
    ) -> Result<Entry> {
        if let Some(parent) = path.parent() {
            self.mkdir(&parent).await?;
        }
        let session_url = match &opts.precondition {
            Precondition::Any => {
                let url = format!("{}:/createUploadSession", self.item_url(path));
                self.create_session(&url, "replace").await?
            }
            Precondition::IfNoneMatch => {
                let url = format!("{}:/createUploadSession", self.item_url(path));
                self.create_session(&url, "fail").await?
            }
            Precondition::IfMatch(etag) => {
                let (id, current) = self.resolve_item(path).await?.ok_or_else(|| {
                    SyncError::PreconditionFailed(format!("onedrive: {path} does not exist"))
                })?;
                if current != *etag {
                    return Err(SyncError::PreconditionFailed(format!(
                        "onedrive: {path} etag does not match {etag}"
                    )));
                }
                let url = format!("{GRAPH}{}/items/{id}/createUploadSession", self.drive);
                self.create_session(&url, "replace").await?
            }
        };
        let mut offset: usize = 0;
        let mut last: Option<Value> = None;
        let mut stalls = 0;
        while offset < data.len() {
            let end = (offset + CHUNK).min(data.len());
            let (status, _, body) = self
                .http
                .request_unchecked_full(
                    Method::PUT,
                    &session_url,
                    &Auth::None,
                    &[
                        (
                            "Content-Range".to_string(),
                            format!("bytes {}-{}/{}", offset, end - 1, data.len()),
                        ),
                        ("Content-Length".to_string(), (end - offset).to_string()),
                    ],
                    Some(data[offset..end].to_vec()),
                )
                .await?;
            match status {
                s if s.is_success() => {
                    last = Some(parse_json(&body)?);
                    offset = end;
                }
                StatusCode::ACCEPTED => {
                    // 202: chunk accepted; nextExpectedRanges tells us
                    // where the server wants us to continue (usually
                    // exactly `end`; earlier means resend from there).
                    let v: Value = parse_json(&body)?;
                    let next = v["nextExpectedRanges"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    let start: usize = next.split('-').next().unwrap_or("").parse().unwrap_or(end);
                    if start <= offset {
                        stalls += 1;
                        if stalls > 2 {
                            return Err(SyncError::Backend(
                                "onedrive: upload session made no progress".into(),
                            ));
                        }
                    } else {
                        stalls = 0;
                    }
                    offset = start;
                }
                StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
                    return Err(SyncError::PreconditionFailed(format!(
                        "onedrive: upload session for {path} failed with HTTP {status}"
                    )));
                }
                s => {
                    return Err(SyncError::Backend(format!(
                        "onedrive: upload session for {path} failed with HTTP {s}"
                    )));
                }
            }
        }
        let item = last.ok_or_else(|| {
            SyncError::Backend("onedrive: upload session returned no item".into())
        })?;
        match item_to_entry(&item, path) {
            Some(e) => Ok(e),
            None => match self.stat(path).await? {
                Some(e) => Ok(e),
                None => Ok(Entry::new(
                    path.clone(),
                    data.len() as u64,
                    SystemTime::now(),
                )),
            },
        }
    }

    /// Create an upload session at `url` (path-based or item-based).
    /// Returns the preauthenticated `uploadUrl` (https only).
    async fn create_session(&self, url: &str, behavior: &str) -> Result<String> {
        let body = serde_json::to_vec(&json!({
            "item": { "@microsoft.graph.conflictBehavior": behavior },
        }))
        .unwrap();
        let (status, _, resp) = self
            .http
            .request_unchecked_full(
                Method::POST,
                url,
                &self.auth()?,
                &[("Content-Type".to_string(), "application/json".to_string())],
                Some(body),
            )
            .await?;
        match status {
            s if s.is_success() => {
                let v: Value = parse_json(&resp)?;
                let upload_url = v["uploadUrl"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| SyncError::Backend("onedrive: missing uploadUrl".into()))?
                    .to_string();
                if !upload_url.starts_with("https://") {
                    return Err(SyncError::Backend("onedrive: insecure uploadUrl".into()));
                }
                Ok(upload_url)
            }
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
                Err(SyncError::PreconditionFailed(format!(
                    "onedrive: upload session refused with HTTP {status}"
                )))
            }
            s => Err(SyncError::Backend(format!(
                "onedrive: createUploadSession failed with HTTP {s}"
            ))),
        }
    }
}

#[async_trait]
impl StorageBackend for OneDriveBackend {
    fn name(&self) -> &'static str {
        "onedrive"
    }

    fn describe(&self) -> String {
        format!("onedrive:{}", self.drive)
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.list_recursive("root", "", &mut out).await?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let url = self.item_url_suffix(path, "/content");
        self.http.get(&url, &self.auth()?).await
    }

    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let url = self.item_url_suffix(path, "/content");
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        Ok(self
            .http
            .request(
                Method::GET,
                &url,
                &self.auth()?,
                &[("Range", range.as_str())],
                None,
            )
            .await?
            .1)
    }

    /// Graph's path-based content endpoint does not support If-Match, so
    /// this mirrors the native drive adapter: download, then verify the
    /// item's eTag is still the expected one before returning bytes.
    async fn read_if_match(&self, path: &RelPath, etag: &str) -> Result<Vec<u8>> {
        let data = self.read(path).await?;
        match self.resolve_item(path).await? {
            Some((_, current)) if current == etag => Ok(data),
            Some((_, current)) => Err(SyncError::PreconditionFailed(format!(
                "onedrive: {path} etag changed ({current} != {etag})"
            ))),
            None => Err(SyncError::NotFound(path.to_string())),
        }
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
        if data.len() >= 4 * 1024 * 1024 {
            return self.upload_session(path, data, &opts).await;
        }
        // Graph simple upload does not create parent folders.
        if let Some(parent) = path.parent() {
            self.mkdir(&parent).await?;
        }
        let content_type = opts
            .content_type
            .unwrap_or_else(|| default_content_type(path).to_string());
        // Conditional PUTs need status-level handling: Graph answers 409
        // for conflictBehavior=fail and 404 when an If-Match item vanished.
        let put = async |url: &str, headers: &[(&str, &str)]| -> Result<()> {
            let (status, _) = self
                .http
                .request_unchecked(
                    Method::PUT,
                    url,
                    &self.auth()?,
                    &headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect::<Vec<_>>(),
                    Some(data.to_vec()),
                )
                .await?;
            match status {
                s if s.is_success() => Ok(()),
                s @ (StatusCode::PRECONDITION_FAILED
                | StatusCode::CONFLICT
                | StatusCode::NOT_FOUND) => Err(SyncError::PreconditionFailed(format!(
                    "onedrive: conditional PUT to {path} failed with HTTP {s}"
                ))),
                s => Err(SyncError::Backend(format!(
                    "onedrive: PUT to {path} failed with HTTP {s}"
                ))),
            }
        };
        match &opts.precondition {
            Precondition::Any => {
                let url = self.item_url_suffix(path, "/content");
                put(&url, &[("Content-Type", content_type.as_str())]).await?;
            }
            Precondition::IfNoneMatch => {
                let url = format!(
                    "{}?@microsoft.graph.conflictBehavior=fail",
                    self.item_url_suffix(path, "/content")
                );
                put(
                    &url,
                    &[
                        ("Content-Type", content_type.as_str()),
                        ("If-None-Match", "*"),
                    ],
                )
                .await?;
            }
            Precondition::IfMatch(etag) => {
                let (id, current) = self.resolve_item(path).await?.ok_or_else(|| {
                    SyncError::PreconditionFailed(format!("onedrive: {path} does not exist"))
                })?;
                if current != *etag {
                    return Err(SyncError::PreconditionFailed(format!(
                        "onedrive: {path} etag does not match {etag}"
                    )));
                }
                let url = format!("{GRAPH}{}/items/{id}/content", self.drive);
                put(
                    &url,
                    &[
                        ("Content-Type", content_type.as_str()),
                        ("If-Match", etag.as_str()),
                    ],
                )
                .await?;
            }
        }
        // Re-stat for the authoritative entry (size/etag/mtime).
        match self.stat(path).await? {
            Some(e) => Ok(e),
            None => Ok(Entry::new(
                path.clone(),
                data.len() as u64,
                SystemTime::now(),
            )),
        }
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        let url = self.item_url(path);
        match self
            .http
            .request(Method::DELETE, &url, &self.auth()?, &[], None)
            .await
        {
            Ok(_) => Ok(()),
            Err(SyncError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let url = format!("{}?$select={SELECT}", self.item_url(path));
        match self.http.get(&url, &self.auth()?).await {
            Ok(body) => {
                let v: Value = parse_json(&body)?;
                Ok(item_to_entry(&v, path))
            }
            Err(SyncError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn mkdir(&self, path: &RelPath) -> Result<()> {
        // Walk the segments, creating each missing folder. A 409 means the
        // name already exists: resolve its ID by listing and continue.
        enum Parent {
            Root,
            Id(String),
        }
        let mut parent = Parent::Root;
        for seg in path.as_str().split('/') {
            let url = match &parent {
                Parent::Root => format!("{GRAPH}{}/root:/children", self.drive),
                Parent::Id(id) => format!("{GRAPH}{}/items/{id}/children", self.drive),
            };
            let resp = self
                .http
                .request(
                    Method::POST,
                    &url,
                    &self.auth()?,
                    &[("Content-Type", "application/json")],
                    Some(serde_json::to_vec(&json!({ "name": seg, "folder": {} })).unwrap()),
                )
                .await;
            match resp {
                Ok((_, body)) => {
                    let v: Value = parse_json(&body)?;
                    parent = Parent::Id(v["id"].as_str().unwrap_or("").to_string());
                }
                Err(SyncError::Backend(msg)) if msg.contains("409") => {
                    let item_id = match &parent {
                        Parent::Root => "root".to_string(),
                        Parent::Id(id) => id.clone(),
                    };
                    let found = self.children(&item_id).await?.into_iter().find(|it| {
                        it.get("folder").is_some()
                            && it["name"].as_str().unwrap_or("").eq_ignore_ascii_case(seg)
                    });
                    match found {
                        Some(it) => {
                            parent = Parent::Id(it["id"].as_str().unwrap_or("").to_string());
                        }
                        None => return Err(SyncError::Backend(msg)),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn set_modified(&self, path: &RelPath, time: SystemTime) -> Result<()> {
        let url = self.item_url(path);
        let _ = self
            .http
            .request(
                Method::PATCH,
                &url,
                &self.auth()?,
                &[("Content-Type", "application/json")],
                Some(
                    serde_json::to_vec(&json!({
                        "fileSystemInfo": { "lastModifiedDateTime": format_rfc3339(time) }
                    }))
                    .unwrap(),
                ),
            )
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_json_parses() {
        let body = br#"{"value":[
          {"id":"a","name":"todo.md","size":42,"lastModifiedDateTime":"2026-11-18T10:00:00Z","eTag":"\"{ABC}\"","file":{}},
          {"id":"b","name":"sub","size":0,"folder":{}}
        ],"@odata.nextLink":null}"#;
        let v: Value = parse_json(body).unwrap();
        let items = v["value"].as_array().unwrap();
        let e = item_to_entry(&items[0], &RelPath::new("todo.md").unwrap()).unwrap();
        assert_eq!(e.size, 42);
        assert_eq!(e.etag.as_deref(), Some("\"{ABC}\""));
        assert!(item_to_entry(&items[1], &RelPath::new("sub").unwrap()).is_none());
    }
}
