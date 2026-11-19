//! Dropbox backend (Dropbox API v2).
//!
//! Ordinary paths under a root; revision tokens are Dropbox `rev`s.
//! Conditional writes use upload `mode: update` (If-Match) and `add`
//! (If-None-Match). Files larger than 8 MiB use upload sessions, with
//! the precondition applied at finish — never at start/append, so a
//! device may edit or delete the target while chunks are in flight.
//!
//! Dropbox stores no content-type, so tombstones are recognised by
//! their body (see the sync layer's `body_tombstones`).

use async_trait::async_trait;
use md_sync_core::backend::Builder;
use md_sync_core::{
    default_content_type, Entry, Precondition, RelPath, Result, StorageBackend, SyncError,
    WriteOptions,
};
use md_sync_http::date::parse_rfc3339;
use md_sync_http::oauth::{OAuthConfig, OAuthRefresher};
use md_sync_http::{Auth, HttpClient};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::SystemTime;

const API: &str = "https://api.dropboxapi.com/2/files";
const CONTENT: &str = "https://content.dropboxapi.com/2/files";
const CHUNK: usize = 8 * 1024 * 1024;

/// How this backend authenticates.
enum AuthSource {
    /// A fixed access token (the application refreshes it).
    Static(String),
    /// Refresh-token flow, refreshed inside the backend on demand.
    OAuth(OAuthRefresher),
}

/// Dropbox backend: one root folder as a sync root.
pub struct DropboxBackend {
    http: HttpClient,
    auth: AuthSource,
    /// Root prefix inside Dropbox, no leading/trailing slash; empty =
    /// the account root.
    root: String,
}

/// Builder in the opendal style: `Dropbox::default().oauth(...).root("notes")`.
#[derive(Default)]
pub struct DropboxBuilder {
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    access_token: Option<String>,
    root: String,
}

impl DropboxBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// OAuth refresh flow: the backend refreshes the access token itself.
    pub fn oauth(
        mut self,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        self.client_id = Some(client_id.into());
        self.client_secret = Some(client_secret.into());
        self.refresh_token = Some(refresh_token.into());
        self
    }

    /// A fixed access token (the application owns refresh).
    pub fn access_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// Root folder inside Dropbox (empty = account root).
    pub fn root(mut self, root: impl Into<String>) -> Self {
        self.root = root.into().trim_matches('/').to_string();
        self
    }
}

impl Builder for DropboxBuilder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        let auth = match (self.refresh_token, self.access_token) {
            (Some(refresh), Some(client_id)) => {
                AuthSource::OAuth(OAuthRefresher::new(OAuthConfig {
                    token_url: "https://api.dropboxapi.com/oauth2/token".into(),
                    client_id,
                    client_secret: self.client_secret.unwrap_or_default(),
                    refresh_token: refresh,
                    scope: None,
                }))
            }
            (None, Some(token)) => AuthSource::Static(token),
            _ => {
                return Err(SyncError::InvalidConfig(
                    "dropbox backend: an access token or OAuth refresh credentials are required"
                        .into(),
                ));
            }
        };
        Ok(Arc::new(DropboxBackend {
            http: HttpClient::new()?,
            auth,
            root: self.root,
        }))
    }
}

/// Dropbox requires non-ASCII header JSON to use \u escapes (including
/// surrogate pairs). JSON escaping alone does not make arbitrary
/// filenames valid headers.
fn ascii_json(value: &Value) -> Result<String> {
    let json = serde_json::to_string(value)
        .map_err(|_| SyncError::Backend("invalid Dropbox argument".into()))?;
    let mut out = String::new();
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    Ok(out)
}

fn invalid(message: &str) -> SyncError {
    SyncError::Backend(message.to_string())
}

fn missing() -> SyncError {
    SyncError::NotFound("Dropbox file missing".into())
}

fn conflict() -> SyncError {
    SyncError::PreconditionFailed("Dropbox revision changed".into())
}

/// Map a Dropbox RPC/content response to an error. 409 carries an
/// `error_summary`: only `path/...not_found` is NotFound, only a summary
/// containing `conflict` is a revision conflict; other 409s (quota,
/// invalid names, locked accounts, session failures) are plain errors.
fn response_error(status: StatusCode, body: &[u8]) -> SyncError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            SyncError::Unauthorized(format!("Dropbox request failed with HTTP {status}"))
        }
        StatusCode::NOT_FOUND => missing(),
        StatusCode::PRECONDITION_FAILED => conflict(),
        StatusCode::CONFLICT => {
            let value: Value = serde_json::from_slice(body).unwrap_or_default();
            let summary = value["error_summary"].as_str().unwrap_or_default();
            if summary.starts_with("path/not_found") || summary.starts_with("path_lookup/not_found")
            {
                missing()
            } else if summary.contains("conflict") {
                conflict()
            } else {
                invalid("Dropbox rejected the file operation")
            }
        }
        s => SyncError::Backend(format!("Dropbox request failed with HTTP {s}")),
    }
}

/// `(size, rev)` for a file entry; folders are `None`.
fn file_meta(value: &Value) -> Result<Option<(u64, String)>> {
    if value[".tag"] == "folder" {
        return Ok(None);
    }
    let size = value["size"]
        .as_u64()
        .ok_or_else(|| invalid("missing Dropbox size"))?;
    let rev = value["rev"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("missing Dropbox rev"))?
        .to_string();
    Ok(Some((size, rev)))
}

impl DropboxBackend {
    /// `/root/path` (leading slash, empty root = account root).
    fn dropbox_path(&self, path: &str) -> String {
        let joined = [self.root.as_str(), path.trim_matches('/')]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        if joined.is_empty() {
            String::new()
        } else {
            format!("/{joined}")
        }
    }

    /// One authenticated request with 401 retry (OAuth only; a static
    /// token gets a single attempt and surfaces 401 as Unauthorized).
    async fn authed(
        &self,
        method: Method,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        match &self.auth {
            AuthSource::Static(token) => {
                self.http
                    .request_unchecked_full(
                        method,
                        url,
                        &Auth::Bearer(token.clone()),
                        headers,
                        body,
                    )
                    .await
            }
            AuthSource::OAuth(refresher) => {
                refresher
                    .request(&self.http, method, url, headers, body)
                    .await
            }
        }
    }

    async fn rpc(&self, method: &str, value: Value) -> Result<Value> {
        let (status, _, body) = self
            .authed(
                Method::POST,
                &format!("{API}/{method}"),
                &[("Content-Type".to_string(), "application/json".to_string())],
                Some(serde_json::to_vec(&value).map_err(SyncError::from)?),
            )
            .await?;
        if !status.is_success() {
            return Err(response_error(status, &body));
        }
        serde_json::from_slice(&body).map_err(SyncError::from)
    }

    async fn mkdir_parents(&self, path: &str) -> Result<()> {
        let mut current = String::new();
        for part in self.dropbox_path(path).split('/') {
            if part.is_empty() {
                continue;
            }
            current.push('/');
            current.push_str(part);
            match self
                .rpc(
                    "create_folder_v2",
                    json!({"path": current, "autorename": false}),
                )
                .await
            {
                Ok(_) => {}
                Err(SyncError::PreconditionFailed(_)) => {
                    let value = self.rpc("get_metadata", json!({"path": current})).await?;
                    if value[".tag"] != "folder" {
                        return Err(SyncError::InvalidConfig("Dropbox parent is a file".into()));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn upload(&self, method: &str, arg: &Value, bytes: &[u8]) -> Result<Value> {
        let (status, _, body) = self
            .authed(
                Method::POST,
                &format!("{CONTENT}/{method}"),
                &[
                    ("Dropbox-API-Arg".to_string(), ascii_json(arg)?),
                    (
                        "Content-Type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                ],
                Some(bytes.to_vec()),
            )
            .await?;
        if !status.is_success() {
            return Err(response_error(status, &body));
        }
        if body.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&body).map_err(SyncError::from)
        }
    }

    /// Download a file, returning `(bytes, rev)`.
    async fn download(&self, path: &RelPath) -> Result<(Vec<u8>, String)> {
        let arg = json!({"path": self.dropbox_path(path.as_str())});
        let (status, headers, body) = self
            .authed(
                Method::POST,
                &format!("{CONTENT}/download"),
                &[("Dropbox-API-Arg".to_string(), ascii_json(&arg)?)],
                None,
            )
            .await?;
        if !status.is_success() {
            return Err(response_error(status, &body));
        }
        let info: Value = serde_json::from_slice(
            headers
                .get("dropbox-api-result")
                .ok_or_else(|| invalid("missing Dropbox download revision"))?
                .as_bytes(),
        )
        .map_err(|_| invalid("invalid Dropbox download metadata"))?;
        let Some((size, rev)) = file_meta(&info)? else {
            return Err(invalid("Dropbox download returned a folder"));
        };
        if body.len() as u64 != size {
            return Err(invalid("incomplete Dropbox download"));
        }
        Ok((body, rev))
    }

    fn entry_from(&self, value: &Value, display: &str) -> Result<Entry> {
        let Some((size, rev)) = file_meta(value)? else {
            return Err(invalid("Dropbox listing returned a folder as a file"));
        };
        let root_prefix = format!("{}/", self.dropbox_path(""));
        let rel = display
            .strip_prefix(root_prefix.as_str())
            .or_else(|| display.strip_prefix('/'))
            .ok_or_else(|| invalid("Dropbox entry outside root"))?;
        let rp = RelPath::new(rel)?;
        let mtime = value["server_modified"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut e = Entry::new(rp, size, mtime);
        e.etag = Some(rev);
        e.content_type = Some(default_content_type(&e.path).to_string());
        Ok(e)
    }
}

#[async_trait]
impl StorageBackend for DropboxBackend {
    fn name(&self) -> &'static str {
        "dropbox"
    }

    fn describe(&self) -> String {
        format!("dropbox:/{}", self.root)
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let mut page = self
            .rpc(
                "list_folder",
                json!({"path": self.dropbox_path(""), "recursive": true, "include_deleted": false, "limit": 2000}),
            )
            .await?;
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        let root = format!("{}/", self.dropbox_path(""));
        loop {
            for item in page["entries"]
                .as_array()
                .ok_or_else(|| invalid("incomplete Dropbox listing"))?
            {
                if item[".tag"] == "folder" {
                    continue;
                }
                let display = item["path_display"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("missing Dropbox path"))?;
                if !display.to_lowercase().starts_with(&root.to_lowercase()) {
                    return Err(invalid("Dropbox entry outside root"));
                }
                if !seen.insert(display.to_lowercase()) {
                    return Err(invalid("duplicate Dropbox listing entry"));
                }
                entries.push(self.entry_from(item, display)?);
            }
            if page["has_more"].as_bool() != Some(true) {
                if page["has_more"].as_bool() != Some(false) {
                    return Err(invalid("missing Dropbox pagination state"));
                }
                break;
            }
            let cursor = page["cursor"]
                .as_str()
                .ok_or_else(|| invalid("missing Dropbox cursor"))?
                .to_owned();
            if !seen.insert(format!("cursor:{cursor}")) {
                return Err(invalid("repeated Dropbox cursor"));
            }
            page = self
                .rpc("list_folder/continue", json!({"cursor": cursor}))
                .await?;
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        Ok(self.download(path).await?.0)
    }

    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let arg = json!({"path": self.dropbox_path(path.as_str())});
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        let (status, _, body) = self
            .authed(
                Method::POST,
                &format!("{CONTENT}/download"),
                &[
                    ("Dropbox-API-Arg".to_string(), ascii_json(&arg)?),
                    ("Range".to_string(), range),
                ],
                None,
            )
            .await?;
        if !status.is_success() {
            return Err(response_error(status, &body));
        }
        Ok(body)
    }

    async fn read_if_match(&self, path: &RelPath, etag: &str) -> Result<Vec<u8>> {
        let (data, rev) = self.download(path).await?;
        if rev == etag {
            Ok(data)
        } else {
            Err(SyncError::PreconditionFailed(format!(
                "dropbox: {path} rev changed"
            )))
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
        if let Some((parent, _)) = path.as_str().rsplit_once('/') {
            self.mkdir_parents(parent).await?;
        }
        let mode = match &opts.precondition {
            Precondition::Any => json!("overwrite"),
            Precondition::IfMatch(rev) => json!({".tag": "update", "update": rev}),
            Precondition::IfNoneMatch => json!("add"),
        };
        let commit = json!({
            "path": self.dropbox_path(path.as_str()),
            "mode": mode,
            "autorename": false,
            "strict_conflict": true,
            "mute": true,
        });
        let value = if data.len() <= CHUNK {
            self.upload("upload", &commit, data).await?
        } else {
            // Sessions: the precondition belongs to finish, never to
            // start/append.
            let session = self
                .upload(
                    "upload_session/start",
                    &json!({"close": false}),
                    &data[..CHUNK],
                )
                .await?;
            let id = session["session_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("missing Dropbox session ID"))?;
            let mut offset = CHUNK;
            while data.len() - offset > CHUNK {
                let arg = json!({"cursor": {"session_id": id, "offset": offset}, "close": false});
                self.upload(
                    "upload_session/append_v2",
                    &arg,
                    &data[offset..offset + CHUNK],
                )
                .await?;
                offset += CHUNK;
            }
            self.upload(
                "upload_session/finish",
                &json!({"cursor": {"session_id": id, "offset": offset}, "commit": commit}),
                &data[offset..],
            )
            .await?
        };
        let Some((size, rev)) = file_meta(&value)? else {
            return Err(invalid("Dropbox upload returned a folder"));
        };
        let mtime = value["server_modified"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(SystemTime::now());
        let mut e = Entry::new(path.clone(), size, mtime);
        e.etag = Some(rev);
        e.content_type = Some(default_content_type(path).to_string());
        Ok(e)
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        match self
            .rpc(
                "delete_v2",
                json!({"path": self.dropbox_path(path.as_str())}),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(SyncError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let value = match self
            .rpc(
                "get_metadata",
                json!({"path": self.dropbox_path(path.as_str())}),
            )
            .await
        {
            Ok(v) => v,
            Err(SyncError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if value[".tag"] == "folder" {
            return Ok(None);
        }
        let display = value["path_display"].as_str().unwrap_or(path.as_str());
        Ok(Some(self.entry_from(&value, display)?))
    }

    async fn mkdir(&self, path: &RelPath) -> Result<()> {
        self.mkdir_parents(path.as_str()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_json_escapes_non_ascii() {
        // U+1F600 needs a surrogate pair; U+00E9 a single \u escape.
        let v = json!({"path": "/notes/😀café.md"});
        let out = ascii_json(&v).unwrap();
        assert!(out.is_ascii(), "{out}");
        assert!(out.contains("\\ud83d\\ude00"), "{out}");
        assert!(out.contains("\\u00e9"), "{out}");
        let back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn conflict_summary_mapping() {
        let body = br#"{"error_summary": "path/conflict/file/..."}"#;
        assert!(matches!(
            response_error(StatusCode::CONFLICT, body),
            SyncError::PreconditionFailed(_)
        ));
        let body = br#"{"error_summary": "path/not_found/"}"#;
        assert!(matches!(
            response_error(StatusCode::CONFLICT, body),
            SyncError::NotFound(_)
        ));
        let body = br#"{"error_summary": "other/quota"}"#;
        assert!(matches!(
            response_error(StatusCode::CONFLICT, body),
            SyncError::Backend(_)
        ));
    }

    #[test]
    fn entry_from_display_strips_root() {
        let b = DropboxBackend {
            http: HttpClient::new().unwrap(),
            auth: AuthSource::Static("x".into()),
            root: "notes".into(),
        };
        let v = json!({".tag": "file", "size": 7, "rev": "abc", "server_modified": "2026-11-18T10:00:00Z"});
        let e = b.entry_from(&v, "/notes/sub/a.md").unwrap();
        assert_eq!(e.path.as_str(), "sub/a.md");
        assert_eq!(e.etag.as_deref(), Some("abc"));
    }
}
