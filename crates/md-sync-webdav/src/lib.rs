//! WebDAV backend (Nextcloud, ownCloud, nginx `dav`, Apache mod_dav, ...).
//!
//! Listing is a recursive `PROPFIND Depth: 1`; writes are `PUT` with
//! `MKCOL` for missing parents. `getlastmodified` / `getetag` feed the
//! engine's comparison; the sync manifest absorbs the fact that WebDAV
//! cannot set a file's mtime on upload.

use async_trait::async_trait;
use md_sync_core::backend::Builder;
use md_sync_core::{
    default_content_type, Entry, Precondition, RelPath, Result, StorageBackend, SyncError,
    WriteOptions,
};
use md_sync_http::date::parse_rfc1123;
use md_sync_http::encode::encode_path;
use md_sync_http::{Auth, HttpClient};
use reqwest::{Method, StatusCode};
use std::sync::Arc;
use std::time::SystemTime;

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:getcontentlength/><d:getlastmodified/><d:getetag/><d:getcontenttype/><d:resourcetype/></d:prop></d:propfind>"#;

/// One WebDAV collection (the sync root) as a backend.
pub struct WebDavBackend {
    http: HttpClient,
    /// Base URL of the root collection, no trailing slash.
    base_url: String,
    auth: Auth,
}

#[derive(Debug, Default, Clone)]
pub struct WebDavBuilder {
    base_url: String,
    username: String,
    password: String,
    bearer_token: Option<String>,
}

impl WebDavBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Root collection URL, e.g. `https://cloud.example.com/remote.php/dav/files/alice/notes`.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn username(mut self, user: impl Into<String>) -> Self {
        self.username = user.into();
        self
    }

    pub fn password(mut self, pass: impl Into<String>) -> Self {
        self.password = pass.into();
        self
    }

    /// Bearer token instead of username/password (some providers).
    pub fn bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }
}

impl Builder for WebDavBuilder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        if self.base_url.is_empty() {
            return Err(SyncError::InvalidConfig(
                "webdav backend: base_url is required".into(),
            ));
        }
        let auth = if let Some(t) = self.bearer_token {
            Auth::Bearer(t)
        } else if !self.username.is_empty() {
            Auth::Basic {
                user: self.username,
                pass: self.password,
            }
        } else {
            Auth::None
        };
        Ok(Arc::new(WebDavBackend {
            http: HttpClient::new()?,
            base_url: self.base_url.trim_end_matches('/').to_string(),
            auth,
        }))
    }
}

impl WebDavBackend {
    fn url(&self, path: &RelPath) -> String {
        format!("{}/{}", self.base_url, encode_path(path.as_str()))
    }

    fn url_for(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.base_url.clone()
        } else {
            format!("{}/{}", self.base_url, encode_path(rel))
        }
    }

    async fn propfind(&self, url: &str, depth: &str) -> Result<Vec<DavEntry>> {
        let (_, body) = self
            .http
            .request(
                Method::from_bytes(b"PROPFIND").unwrap(),
                url,
                &self.auth,
                &[("Depth", depth), ("Content-Type", "application/xml")],
                Some(PROPFIND_BODY.as_bytes().to_vec()),
            )
            .await?;
        parse_multistatus(&body, &self.base_url)
    }

    /// Recursive listing starting at a collection URL.
    async fn list_recursive(
        &self,
        url: &str,
        rel_prefix: &str,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        let found = self.propfind(url, "1").await?;
        for item in found {
            // The collection itself comes back as the first response; skip it.
            if item.rel.is_empty() {
                continue;
            }
            let rel = if rel_prefix.is_empty() {
                item.rel.clone()
            } else {
                format!("{}/{}", rel_prefix, item.rel)
            };
            if item.is_collection {
                let child_url = self.url_for(&rel);
                Box::pin(self.list_recursive(&child_url, &rel, out)).await?;
            } else if let Ok(rp) = RelPath::new(&rel) {
                let mut e = Entry::new(rp, item.size, item.mtime);
                e.etag = item.etag;
                e.content_type = item.content_type;
                out.push(e);
            }
        }
        Ok(())
    }

    /// Create parent collections for `path` (MKCOL per segment; 405 =
    /// already exists is fine).
    async fn ensure_parents(&self, path: &RelPath) -> Result<()> {
        let mut prefix = String::new();
        let parent = match path.parent() {
            Some(p) => p,
            None => return Ok(()),
        };
        for seg in parent.as_str().split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let url = self.url_for(&prefix);
            let (status, _) = self
                .http
                .request_unchecked(
                    Method::from_bytes(b"MKCOL").unwrap(),
                    &url,
                    &self.auth,
                    &[],
                    None,
                )
                .await?;
            match status {
                StatusCode::CREATED | StatusCode::OK | StatusCode::METHOD_NOT_ALLOWED => {}
                s => {
                    return Err(SyncError::Backend(format!(
                        "MKCOL {url} failed with HTTP {s}"
                    )))
                }
            }
        }
        Ok(())
    }
}

/// One `<response>` from a PROPFIND multistatus.
struct DavEntry {
    /// Path relative to the collection that was listed (decoded).
    rel: String,
    is_collection: bool,
    size: u64,
    mtime: SystemTime,
    etag: Option<String>,
    content_type: Option<String>,
}

/// Parse a `<multistatus>` document. `base_url` is used to turn absolute
/// `<href>`s into paths relative to the sync root.
fn parse_multistatus(body: &[u8], base_url: &str) -> Result<Vec<DavEntry>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut out: Vec<DavEntry> = Vec::new();
    let mut cur: Option<DavEntry> = None;
    let mut field = String::new();
    // The path part of base_url, e.g. `/remote.php/dav/files/alice/notes`.
    let base_path = base_url
        .split("://")
        .nth(1)
        .and_then(|s| s.find('/').map(|i| &s[i..]))
        .unwrap_or("");

    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) => {
                // Self-closing tags, e.g. `<d:collection/>`: no Start event.
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                if local == "collection" {
                    if let Some(c) = cur.as_mut() {
                        c.is_collection = true;
                    }
                }
            }
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = name.rsplit(':').next().unwrap_or(&name).to_string();
                if local == "response" {
                    cur = Some(DavEntry {
                        rel: String::new(),
                        is_collection: false,
                        size: 0,
                        mtime: SystemTime::UNIX_EPOCH,
                        etag: None,
                        content_type: None,
                    });
                }
                if local == "collection" {
                    if let Some(c) = cur.as_mut() {
                        c.is_collection = true;
                    }
                }
                field = local;
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .xml_content()
                    .map_err(|e| SyncError::Backend(e.to_string()))?;
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                if let Some(c) = cur.as_mut() {
                    match field.as_str() {
                        "href" => {
                            // href is an absolute path (or full URL); make
                            // it relative to the base collection.
                            let p = text
                                .split("://")
                                .nth(1)
                                .and_then(|s| s.find('/').map(|i| &s[i..]))
                                .unwrap_or(text.as_str());
                            let decoded = percent_decode(p);
                            let rel = decoded
                                .strip_prefix(base_path)
                                .unwrap_or(&decoded)
                                .trim_matches('/')
                                .to_string();
                            c.rel = rel;
                        }
                        "getcontentlength" => c.size = text.parse().unwrap_or(0),
                        "getlastmodified" => {
                            c.mtime = parse_rfc1123(&text).unwrap_or(SystemTime::UNIX_EPOCH)
                        }
                        "getetag" => c.etag = Some(text.trim_matches('"').to_string()),
                        "getcontenttype" => c.content_type = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.rsplit(':').next() == Some("response") {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                }
                field.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SyncError::Backend(format!("WebDAV PROPFIND XML: {e}"))),
            _ => {}
        }
    }
    Ok(out)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[async_trait]
impl StorageBackend for WebDavBackend {
    fn name(&self) -> &'static str {
        "webdav"
    }

    fn describe(&self) -> String {
        format!("webdav:{}", self.base_url)
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.list_recursive(&self.base_url, "", &mut out).await?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let url = self.url(path);
        Ok(self.http.get(&url, &self.auth).await?)
    }

    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let url = self.url(path);
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        Ok(self
            .http
            .request(
                Method::GET,
                &url,
                &self.auth,
                &[("Range", range.as_str())],
                None,
            )
            .await?
            .1)
    }

    async fn read_if_match(&self, path: &RelPath, etag: &str) -> Result<Vec<u8>> {
        let url = self.url(path);
        Ok(self
            .http
            .request(Method::GET, &url, &self.auth, &[("If-Match", etag)], None)
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
        self.ensure_parents(path).await?;
        let url = self.url(path);
        let content_type = opts
            .content_type
            .unwrap_or_else(|| default_content_type(path).to_string());
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", content_type.as_str())];
        let if_match;
        match &opts.precondition {
            Precondition::Any => {}
            Precondition::IfMatch(etag) => {
                if_match = etag.clone();
                headers.push(("If-Match", if_match.as_str()));
            }
            Precondition::IfNoneMatch => {
                headers.push(("If-None-Match", "*"));
            }
        }
        self.http
            .request(Method::PUT, &url, &self.auth, &headers, Some(data.to_vec()))
            .await?;
        // PUT answers no etag: re-stat for the authoritative entry.
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
        let url = self.url(path);
        match self
            .http
            .request(Method::DELETE, &url, &self.auth, &[], None)
            .await
        {
            Ok(_) => Ok(()),
            Err(SyncError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let url = self.url(path);
        match self.propfind(&url, "0").await {
            Ok(items) => {
                let item = items.into_iter().find(|i| !i.is_collection);
                Ok(item.map(|i| {
                    let mut e = Entry::new(path.clone(), i.size, i.mtime);
                    e.etag = i.etag;
                    e.content_type = i.content_type;
                    e
                }))
            }
            Err(SyncError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn mkdir(&self, path: &RelPath) -> Result<()> {
        let url = self.url_for(path.as_str());
        let (status, _) = self
            .http
            .request_unchecked(
                Method::from_bytes(b"MKCOL").unwrap(),
                &url,
                &self.auth,
                &[],
                None,
            )
            .await?;
        match status {
            StatusCode::CREATED | StatusCode::OK | StatusCode::METHOD_NOT_ALLOWED => Ok(()),
            s => Err(SyncError::Backend(format!(
                "MKCOL {url} failed with HTTP {s}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus_parses() {
        let xml = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response><d:href>/remote.php/dav/files/alice/notes/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
  <d:response><d:href>/remote.php/dav/files/alice/notes/todo.md</d:href>
    <d:propstat><d:prop><d:getcontentlength>128</d:getcontentlength>
    <d:getlastmodified>Wed, 18 Nov 2026 16:28:52 GMT</d:getlastmodified>
    <d:getetag>"abc-123"</d:getetag><d:resourcetype/></d:prop></d:propstat></d:response>
  <d:response><d:href>/remote.php/dav/files/alice/notes/sub%20dir/a.md</d:href>
    <d:propstat><d:prop><d:getcontentlength>7</d:getcontentlength>
    <d:getlastmodified>Wed, 18 Nov 2026 16:28:52 GMT</d:getlastmodified>
    <d:getetag>"x"</d:getetag><d:resourcetype/></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let items = parse_multistatus(
            xml,
            "https://cloud.example.com/remote.php/dav/files/alice/notes",
        )
        .unwrap();
        assert_eq!(items.len(), 3);
        assert!(items[0].is_collection);
        assert_eq!(items[0].rel, "");
        assert_eq!(items[1].rel, "todo.md");
        assert_eq!(items[1].size, 128);
        assert_eq!(items[1].etag.as_deref(), Some("abc-123"));
        assert_eq!(items[2].rel, "sub dir/a.md", "percent-decoded href");
    }
}
