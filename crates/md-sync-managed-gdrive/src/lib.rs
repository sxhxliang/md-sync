//! Managed Google Drive backend: content-addressed storage behind a
//! CAS manifest (Drive v2 API).
//!
//! Google Drive permits duplicate names. A public property on the
//! stable My Drive root elects one manifest by `ETag` CAS; all logical
//! paths then commit by CAS of that manifest. Payloads have
//! pre-generated IDs and are never modified. Local notes stay ordinary
//! files. Cloud objects are exclusively app-managed: this backend does
//! not import or expose ordinary existing Drive files.
//!
//! The on-disk layout (`genius.sync.manifest.v1` pointer,
//! `Genius Sync Data` folder, manifest format 1) is identical to the
//! native drive adapter this replaces, so existing managed namespaces
//! keep working after the migration.

use async_trait::async_trait;
use md_sync_core::backend::Builder;
use md_sync_core::{Entry, Precondition, RelPath, Result, StorageBackend, SyncError, WriteOptions};
use md_sync_http::oauth::{OAuthConfig, OAuthRefresher};
use md_sync_http::HttpClient;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

const API: &str = "https://www.googleapis.com/drive/v2/files";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v2/files";
const FORMAT: u32 = 1;
const CHUNK: usize = 8 * 1024 * 1024;

/// Persistent account-root registry key and display name for managed
/// storage. Reuse these values across clients and upgrades to retain
/// the same namespace.
#[derive(Clone, Debug)]
pub struct ManagedNamespace {
    pub pointer_key: String,
    pub folder_name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    folder: String,
    entries: BTreeMap<String, Object>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    id: String,
    hash: String,
    size: u64,
    /// A unique revision, even when bytes are unchanged (no ABA).
    revision: String,
}

struct Snapshot {
    id: String,
    etag: String,
    manifest: Manifest,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn file_url(id: &str) -> Result<String> {
    if !valid_id(id) {
        return Err(SyncError::Backend("invalid Google Drive object ID".into()));
    }
    Ok(format!("{API}/{id}"))
}

fn invalid(message: &str) -> SyncError {
    SyncError::Backend(message.to_string())
}

fn conflict() -> SyncError {
    SyncError::PreconditionFailed("Google Drive revision changed".into())
}

fn missing() -> SyncError {
    SyncError::NotFound("Google Drive file missing".into())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("incomplete Google Drive response"))
}

/// Map a Drive response status to an error (or pass the body through).
fn checked(status: StatusCode, body: Vec<u8>) -> Result<Vec<u8>> {
    if status.is_success() {
        return Ok(body);
    }
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(SyncError::Unauthorized(format!(
            "Google Drive request failed with HTTP {status}"
        ))),
        StatusCode::NOT_FOUND => Err(missing()),
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => Err(conflict()),
        s => Err(SyncError::Backend(format!(
            "Google Drive request failed with HTTP {s}"
        ))),
    }
}

/// Managed Drive backend: one elected manifest + immutable objects.
pub struct ManagedGDriveBackend {
    http: HttpClient,
    auth: OAuthRefresher,
    /// Logical root prefix inside the managed namespace (no slashes).
    root: String,
    namespace: ManagedNamespace,
}

/// Builder: `ManagedGDrive::default().oauth(...).root("notes").namespace(...)`.
#[derive(Default)]
pub struct ManagedGDriveBuilder {
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    root: String,
    namespace: Option<ManagedNamespace>,
}

impl ManagedGDriveBuilder {
    pub fn new() -> Self {
        Self::default()
    }

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

    /// Logical root prefix inside the managed namespace.
    pub fn root(mut self, root: impl Into<String>) -> Self {
        self.root = root.into().trim_matches('/').to_string();
        self
    }

    pub fn namespace(mut self, namespace: ManagedNamespace) -> Self {
        self.namespace = Some(namespace);
        self
    }
}

impl Builder for ManagedGDriveBuilder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        let namespace = self.namespace.ok_or_else(|| {
            SyncError::InvalidConfig("managed-gdrive backend: namespace is required".into())
        })?;
        if namespace.pointer_key.is_empty() || namespace.folder_name.is_empty() {
            return Err(SyncError::InvalidConfig(
                "managed namespace must be specified".into(),
            ));
        }
        let (client_id, refresh_token) = match (self.client_id, self.refresh_token) {
            (Some(c), Some(r)) => (c, r),
            _ => {
                return Err(SyncError::InvalidConfig(
                    "managed-gdrive backend: OAuth refresh credentials are required".into(),
                ));
            }
        };
        Ok(Arc::new(ManagedGDriveBackend {
            http: HttpClient::new()?,
            auth: OAuthRefresher::new(OAuthConfig {
                token_url: "https://oauth2.googleapis.com/token".into(),
                client_id,
                client_secret: self.client_secret.unwrap_or_default(),
                refresh_token,
                scope: None,
            }),
            root: self.root,
            namespace,
        }))
    }
}

impl ManagedGDriveBackend {
    /// Logical path inside the managed namespace (`root` + `path`).
    fn full_path(&self, path: &str) -> String {
        [self.root.as_str(), path.trim_matches('/')]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        self.auth
            .request(&self.http, method, url, headers, body)
            .await
    }

    async fn json(&self, method: Method, url: &str, value: &Value) -> Result<Value> {
        let (status, _, body) = self
            .request(
                method,
                url,
                &[("Content-Type".to_string(), "application/json".to_string())],
                Some(serde_json::to_vec(value).map_err(SyncError::from)?),
            )
            .await?;
        let body = checked(status, body)?;
        serde_json::from_slice(&body).map_err(SyncError::from)
    }

    async fn gdrive_get(&self, id: &str) -> Result<Value> {
        let (status, _, body) = self.request(Method::GET, &file_url(id)?, &[], None).await?;
        let body = checked(status, body)?;
        let value: Value = serde_json::from_slice(&body).map_err(SyncError::from)?;
        if value["labels"]["trashed"] == true {
            return Err(missing());
        }
        Ok(value)
    }

    async fn gdrive_id(&self) -> Result<String> {
        let (status, _, body) = self
            .request(
                Method::GET,
                &format!("{API}/generateIds?maxResults=1&space=drive"),
                &[],
                None,
            )
            .await?;
        let body = checked(status, body)?;
        let value: Value = serde_json::from_slice(&body).map_err(SyncError::from)?;
        let id = value["ids"]
            .as_array()
            .and_then(|ids| ids.first())
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing Google Drive generated ID"))?;
        if !valid_id(id) {
            return Err(invalid("invalid Google Drive generated ID"));
        }
        Ok(id.into())
    }

    async fn gdrive_manifest_id(&self) -> Result<String> {
        let namespace = &self.namespace;
        let pointer_key = namespace.pointer_key.as_str();
        // The root ID is account-stable, unlike a folder found by name.
        // Never resolve duplicate names or infer the elected manifest
        // from a listing.
        for _ in 0..8 {
            let root = self.gdrive_get("root").await?;
            let mut properties = root
                .get("properties")
                .map(|p| {
                    p.as_array()
                        .cloned()
                        .ok_or_else(|| invalid("invalid Google Drive properties"))
                })
                .transpose()?
                .unwrap_or_default();
            let pointers: Vec<_> = properties
                .iter()
                .filter(|p| p["key"] == pointer_key && p["visibility"] == "PUBLIC")
                .collect();
            if pointers.len() > 1 {
                return Err(invalid("ambiguous Google Drive sync registry"));
            }
            if let Some(pointer) = pointers.first() {
                let id = string(pointer, "value")?;
                file_url(id)?;
                return Ok(id.into());
            }
            let folder = self.gdrive_id().await?;
            self.json(
                Method::POST,
                API,
                &json!({"id": folder, "title": namespace.folder_name, "mimeType": "application/vnd.google-apps.folder", "parents": [{"id": string(&root, "id")?}]}),
            )
            .await?;
            let id = self.gdrive_id().await?;
            let manifest = Manifest {
                format: FORMAT,
                folder: folder.clone(),
                entries: BTreeMap::new(),
            };
            self.gdrive_insert(
                &id,
                &folder,
                "manifest.json",
                &serde_json::to_vec(&manifest).map_err(SyncError::from)?,
            )
            .await?;
            properties.push(json!({"key": pointer_key, "visibility": "PUBLIC", "value": id}));
            let (status, _, body) = self
                .request(
                    Method::PATCH,
                    &file_url(string(&root, "id")?)?,
                    &[
                        ("If-Match".to_string(), string(&root, "etag")?.to_string()),
                        ("Content-Type".to_string(), "application/json".to_string()),
                    ],
                    Some(
                        serde_json::to_vec(&json!({"properties": properties}))
                            .map_err(SyncError::from)?,
                    ),
                )
                .await?;
            match checked(status, body) {
                Ok(_) => {
                    // Confirm election even if the server omits properties
                    // from PATCH's response. Never use a candidate not in
                    // the pointer.
                    let current = self.gdrive_get("root").await?;
                    let elected = current["properties"]
                        .as_array()
                        .and_then(|ps| {
                            ps.iter()
                                .find(|p| p["key"] == pointer_key && p["visibility"] == "PUBLIC")
                        })
                        .and_then(|p| p["value"].as_str());
                    if elected == Some(id.as_str()) {
                        return Ok(id);
                    }
                    return Err(conflict());
                }
                Err(SyncError::PreconditionFailed(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(conflict())
    }

    async fn gdrive_snapshot(&self) -> Result<Snapshot> {
        let id = self.gdrive_manifest_id().await?;
        // A missing/corrupt elected manifest is never a new empty space.
        let info = self.gdrive_get(&id).await.map_err(|e| {
            if matches!(e, SyncError::NotFound(_)) {
                invalid("Google Drive sync manifest missing")
            } else {
                e
            }
        })?;
        let etag = string(&info, "etag")?.to_owned();
        let bytes = self.gdrive_media(&id, Some(&etag)).await?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("invalid Google Drive sync manifest"))?;
        if manifest.format != FORMAT
            || !valid_id(&manifest.folder)
            || manifest.entries.iter().any(|(path, object)| {
                RelPath::new(path).is_err()
                    || !valid_id(&object.id)
                    || object.hash.len() != 64
                    || !object.hash.bytes().all(|b| b.is_ascii_hexdigit())
                    || object.revision.len() != 32
                    || !object.revision.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err(invalid("unsupported Google Drive sync manifest"));
        }
        Ok(Snapshot { id, etag, manifest })
    }

    async fn gdrive_media(&self, id: &str, etag: Option<&str>) -> Result<Vec<u8>> {
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(e) = etag {
            headers.push(("If-Match".to_string(), e.to_string()));
        }
        let (status, _, body) = self
            .request(
                Method::GET,
                &format!("{}?alt=media", file_url(id)?),
                &headers,
                None,
            )
            .await?;
        let body = checked(status, body)?;
        // Verify after reading as well; a media response may omit its ETag.
        if let Some(etag) = etag {
            let current = self.gdrive_get(id).await?;
            if string(&current, "etag")? != etag {
                return Err(conflict());
            }
        }
        Ok(body)
    }

    async fn gdrive_insert(
        &self,
        id: &str,
        folder: &str,
        title: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        let info = json!({"id": id, "title": title, "mimeType": "application/octet-stream", "parents": [{"id": folder}]});
        // All uploads use a generated immutable ID. Session completion
        // cannot overwrite any visible logical file; only the later
        // manifest CAS can.
        let (status, headers, _) = self
            .request(
                Method::POST,
                &format!("{UPLOAD}?uploadType=resumable"),
                &[
                    ("Content-Type".to_string(), "application/json".to_string()),
                    (
                        "X-Upload-Content-Type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                    (
                        "X-Upload-Content-Length".to_string(),
                        bytes.len().to_string(),
                    ),
                ],
                Some(serde_json::to_vec(&info).map_err(SyncError::from)?),
            )
            .await?;
        let _ = checked(status, Vec::new())?;
        let session = headers
            .get("location")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| invalid("missing Google Drive upload URL"))?
            .to_owned();
        if !session.starts_with("https://www.googleapis.com/upload/drive/") {
            return Err(invalid("invalid Google Drive upload destination"));
        }
        // 8 MiB is a multiple of Drive's required 256 KiB chunk size.
        let mut offset = 0;
        loop {
            let end = (offset + CHUNK).min(bytes.len());
            let range = if bytes.is_empty() {
                "bytes */0".to_string()
            } else {
                format!("bytes {}-{}/{}", offset, end - 1, bytes.len())
            };
            let (status, resp_headers, body) = self
                .request(
                    Method::PUT,
                    &session,
                    &[
                        (
                            "Content-Type".to_string(),
                            "application/octet-stream".to_string(),
                        ),
                        ("Content-Range".to_string(), range),
                    ],
                    Some(bytes[offset..end].to_vec()),
                )
                .await?;
            if end == bytes.len() {
                let body = checked(status, body)?;
                let value: Value = serde_json::from_slice(&body).map_err(SyncError::from)?;
                if string(&value, "id")? != id {
                    return Err(invalid("Google Drive upload changed object ID"));
                }
                return Ok(value);
            }
            if status.as_u16() != 308
                || resp_headers.get("range").and_then(|h| h.to_str().ok())
                    != Some(format!("bytes=0-{}", end - 1).as_str())
            {
                return Err(invalid("incomplete Google Drive upload chunk"));
            }
            offset = end;
        }
    }

    async fn gdrive_commit(&self, snapshot: &Snapshot) -> Result<()> {
        let (status, _, body) = self
            .request(
                Method::PUT,
                &format!("{UPLOAD}/{}?uploadType=media", snapshot.id),
                &[
                    ("If-Match".to_string(), snapshot.etag.clone()),
                    ("Content-Type".to_string(), "application/json".to_string()),
                ],
                Some(serde_json::to_vec(&snapshot.manifest).map_err(SyncError::from)?),
            )
            .await?;
        let body = checked(status, body)?;
        let value: Value = serde_json::from_slice(&body).map_err(SyncError::from)?;
        if string(&value, "id")? != snapshot.id || string(&value, "etag")? == snapshot.etag {
            return Err(invalid("Google Drive manifest commit omitted new revision"));
        }
        Ok(())
    }

    fn entry_from(&self, rel: &str, object: &Object) -> Result<Entry> {
        let rp = RelPath::new(rel)?;
        let mut e = Entry::new(rp, object.size, SystemTime::UNIX_EPOCH);
        e.etag = Some(object.revision.clone());
        e.content_type = Some("application/octet-stream".to_string());
        Ok(e)
    }
}

#[async_trait]
impl StorageBackend for ManagedGDriveBackend {
    fn name(&self) -> &'static str {
        "managed-gdrive"
    }

    fn describe(&self) -> String {
        format!("managed-gdrive:{}", self.namespace.folder_name)
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let snapshot = self.gdrive_snapshot().await?;
        let root = if self.root.is_empty() {
            String::new()
        } else {
            format!("{}/", self.root)
        };
        let mut out = Vec::new();
        for (path, object) in &snapshot.manifest.entries {
            let Some(rel) = path.strip_prefix(&root) else {
                continue;
            };
            out.push(self.entry_from(rel, object)?);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let snapshot = self.gdrive_snapshot().await?;
        let object = snapshot
            .manifest
            .entries
            .get(&self.full_path(path.as_str()))
            .ok_or_else(missing)?;
        let bytes = self.gdrive_media(&object.id, None).await?;
        if bytes.len() as u64 != object.size
            || blake3::hash(&bytes).to_hex().as_str() != object.hash
        {
            return Err(invalid(
                "Google Drive managed content was modified or truncated",
            ));
        }
        Ok(bytes)
    }

    async fn read_if_match(&self, path: &RelPath, etag: &str) -> Result<Vec<u8>> {
        let snapshot = self.gdrive_snapshot().await?;
        let object = snapshot
            .manifest
            .entries
            .get(&self.full_path(path.as_str()))
            .ok_or_else(missing)?;
        if object.revision != etag {
            return Err(conflict());
        }
        let bytes = self.gdrive_media(&object.id, None).await?;
        if bytes.len() as u64 != object.size
            || blake3::hash(&bytes).to_hex().as_str() != object.hash
        {
            return Err(invalid(
                "Google Drive managed content was modified or truncated",
            ));
        }
        Ok(bytes)
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
        let _ = opts.content_type; // Objects are always octet-stream.
        let mut snapshot = self.gdrive_snapshot().await?;
        let rel = path.clone();
        let path = self.full_path(path.as_str());
        let previous = snapshot.manifest.entries.get(&path);
        let if_match_conflict = match &opts.precondition {
            Precondition::IfMatch(expected) => {
                previous.map_or(true, |object| &object.revision != expected)
            }
            _ => false,
        };
        let if_none_conflict =
            matches!(opts.precondition, Precondition::IfNoneMatch) && previous.is_some();
        if if_match_conflict || if_none_conflict {
            return Err(conflict());
        }
        // A file cannot shadow a logical folder or one of its ancestor files.
        if snapshot
            .manifest
            .entries
            .keys()
            .any(|key| key.starts_with(&format!("{path}/")) || path.starts_with(&format!("{key}/")))
        {
            return Err(conflict());
        }
        let id = self.gdrive_id().await?;
        let object = Object {
            id: id.clone(),
            hash: blake3::hash(data).to_hex().to_string(),
            size: data.len() as u64,
            revision: format!("{:032x}", rand::random::<u128>()),
        };
        self.gdrive_insert(
            &id,
            &snapshot.manifest.folder,
            &format!("content-{id}"),
            data,
        )
        .await?;
        let meta = object.clone();
        let previous = previous.map(|object| object.revision.clone());
        for _ in 0..8 {
            if snapshot.manifest.entries.keys().any(|key| {
                key.starts_with(&format!("{path}/")) || path.starts_with(&format!("{key}/"))
            }) {
                return Err(conflict());
            }
            snapshot
                .manifest
                .entries
                .insert(path.clone(), object.clone());
            match self.gdrive_commit(&snapshot).await {
                Ok(()) => {
                    return self.entry_from(rel.as_str(), &meta);
                }
                Err(SyncError::PreconditionFailed(_)) => {
                    snapshot = self.gdrive_snapshot().await?;
                    // Retry contention on a different logical file, never
                    // an intervening edit/delete/recreate of this
                    // destination.
                    if snapshot.manifest.entries.get(&path).map(|o| &o.revision)
                        != previous.as_ref()
                    {
                        return Err(conflict());
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(conflict())
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        let mut snapshot = self.gdrive_snapshot().await?;
        if snapshot
            .manifest
            .entries
            .remove(&self.full_path(path.as_str()))
            .is_some()
        {
            self.gdrive_commit(&snapshot).await?;
        }
        // Content remains immutable: another device may still be
        // downloading an earlier snapshot. Reclamation needs a separate
        // retention protocol.
        Ok(())
    }

    async fn mkdir(&self, _path: &RelPath) -> Result<()> {
        Ok(())
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let snapshot = self.gdrive_snapshot().await?;
        let full = self.full_path(path.as_str());
        match snapshot.manifest.entries.get(&full) {
            Some(object) => Ok(Some(self.entry_from(path.as_str(), object)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_and_validates() {
        let body = br#"{"format":1,"folder":"abc","entries":{"notes/a.md":{"id":"x1","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":3,"revision":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}"#;
        let m: Manifest = serde_json::from_slice(body).unwrap();
        assert_eq!(m.format, FORMAT);
        assert_eq!(m.folder, "abc");
        assert_eq!(m.entries.len(), 1);
        let obj = &m.entries["notes/a.md"];
        assert_eq!(obj.size, 3);
        assert!(valid_id(&obj.id));
    }

    #[test]
    fn manifest_rejects_bad_hash_and_revision() {
        let body = br#"{"format":1,"folder":"abc","entries":{"a.md":{"id":"x1","hash":"zz","size":3,"revision":"short"}}}"#;
        let m: Manifest = serde_json::from_slice(body).unwrap();
        let bad = m.entries.iter().any(|(path, object)| {
            RelPath::new(path).is_err()
                || !valid_id(&object.id)
                || object.hash.len() != 64
                || !object.hash.bytes().all(|b| b.is_ascii_hexdigit())
                || object.revision.len() != 32
                || !object.revision.bytes().all(|b| b.is_ascii_hexdigit())
        });
        assert!(bad);
    }

    #[test]
    fn valid_id_rules() {
        assert!(valid_id("abc-123_X"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a b"));
    }
}
