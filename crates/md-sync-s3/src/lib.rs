//! Amazon S3 backend (also works with S3-compatible stores like MinIO).
//!
//! SigV4 signing is implemented here, by hand, with `hmac` + `sha2` —
//! about 120 lines — instead of pulling in an AWS SDK (which would dwarf
//! the rest of the workspace). Transport is the shared `md-sync-http`
//! client (reqwest + rustls).
//!
//! Notes and limits:
//! * `LastModified` is the upload time: S3 cannot preserve a source
//!   file's mtime. The sync manifest absorbs that (see core docs).
//! * `ETag` is an MD5 for single-part uploads; it is stored as `etag`,
//!   never mistaken for a SHA-256 `hash`.
//! * No multipart upload: files are sent in one PUT. Markdown documents
//!   are small; a multi-gigabyte file is out of scope for this tool.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use md_sync_core::backend::Builder;
use md_sync_core::{
    default_content_type, Entry, Precondition, RelPath, Result, StorageBackend, SyncError,
    WriteOptions,
};
use md_sync_http::date::{amz_dates, parse_rfc3339};
use md_sync_http::encode::{encode_path, encode_segment};
use md_sync_http::{Auth, HttpClient};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::SystemTime;

type HmacSha256 = Hmac<Sha256>;

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// S3 backend: one bucket (+ optional key prefix) as a sync root.
pub struct S3Backend {
    http: HttpClient,
    bucket: String,
    region: String,
    /// Base URL without trailing slash. Virtual-host style for AWS,
    /// path style for a custom endpoint (MinIO etc.).
    base_url: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    /// Key prefix, e.g. `notes/` or empty. Always ends with `/` if non-empty.
    prefix: String,
}

/// Builder in the opendal style:
/// `S3::default().bucket("notes").region("us-east-1")`.
#[derive(Debug, Default, Clone)]
pub struct S3Builder {
    bucket: String,
    region: String,
    endpoint: Option<String>,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    prefix: String,
}

impl S3Builder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = bucket.into();
        self
    }

    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = region.into();
        self
    }

    /// Custom endpoint, e.g. `http://localhost:9000` for MinIO.
    /// Without it, the AWS endpoint for `region` is used.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn access_key(mut self, key: impl Into<String>) -> Self {
        self.access_key = key.into();
        self
    }

    pub fn secret_key(mut self, key: impl Into<String>) -> Self {
        self.secret_key = key.into();
        self
    }

    pub fn session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }

    /// Key prefix inside the bucket, e.g. `notes`. Empty = bucket root.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }
}

impl Builder for S3Builder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        if self.bucket.is_empty() {
            return Err(SyncError::InvalidConfig(
                "s3 backend: bucket is required".into(),
            ));
        }
        if self.access_key.is_empty() || self.secret_key.is_empty() {
            return Err(SyncError::InvalidConfig(
                "s3 backend: access_key and secret_key are required".into(),
            ));
        }
        let region = if self.region.is_empty() {
            "us-east-1".to_string()
        } else {
            self.region
        };
        let base_url = match &self.endpoint {
            Some(ep) => format!("{}/{}", ep.trim_end_matches('/'), self.bucket),
            None => format!("https://{}.s3.{}.amazonaws.com", self.bucket, region),
        };
        let prefix = {
            let p = self.prefix.trim_matches('/').to_string();
            if p.is_empty() {
                String::new()
            } else {
                format!("{p}/")
            }
        };
        Ok(Arc::new(S3Backend {
            http: HttpClient::no_redirect()?,
            bucket: self.bucket,
            region,
            base_url,
            access_key: self.access_key,
            secret_key: self.secret_key,
            session_token: self.session_token,
            prefix,
        }))
    }
}

impl S3Backend {
    fn key(&self, path: &RelPath) -> String {
        format!("{}{}", self.prefix, path.as_str())
    }

    fn object_url(&self, key: &str) -> String {
        format!("{}/{}", self.base_url, encode_path(key))
    }

    /// Sign a request (SigV4) and return the headers to send.
    /// `extra` are additional headers that are also signed (e.g. content-type).
    fn sign(
        &self,
        method: &str,
        url: &str,
        payload_hash: &str,
        extra: &[(String, String)],
        now: SystemTime,
    ) -> Result<Vec<(String, String)>> {
        let (date_stamp, amz_date) = amz_dates(now);
        // Minimal URL split: scheme://host/path?query
        let after_scheme = url.split("://").nth(1).unwrap_or(url);
        let (host, rest) = match after_scheme.find('/') {
            Some(i) => (&after_scheme[..i], &after_scheme[i..]),
            None => (after_scheme, "/"),
        };
        let (path, query) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        // Canonical query: sort by key, values already percent-encoded.
        let mut qparts: Vec<&str> = if query.is_empty() {
            Vec::new()
        } else {
            query.split('&').collect()
        };
        qparts.sort();
        let canonical_query = qparts.join("&");

        let mut headers: Vec<(String, String)> = vec![
            ("host".to_string(), host.to_string()),
            ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(t) = &self.session_token {
            headers.push(("x-amz-security-token".to_string(), t.clone()));
        }
        for (k, v) in extra {
            headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed_headers: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        let signed_headers = signed_headers.join(";");

        let canonical_request = format!(
            "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signing_key = self.signing_key(&date_stamp);
        let mut mac = HmacSha256::new_from_slice(&signing_key)
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        mac.update(string_to_sign.as_bytes());
        let signature = mac.finalize().into_bytes();
        let signature_hex: String = signature.iter().map(|b| format!("{b:02x}")).collect();

        let mut out: Vec<(String, String)> = vec![
            (
                "X-Amz-Date".to_string(),
                amz_date.clone(),
            ),
            (
                "X-Amz-Content-Sha256".to_string(),
                payload_hash.to_string(),
            ),
            (
                "Authorization".to_string(),
                format!(
                    "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature_hex}",
                    self.access_key
                ),
            ),
        ];
        if let Some(t) = &self.session_token {
            out.push(("X-Amz-Security-Token".to_string(), t.clone()));
        }
        for (k, v) in extra {
            out.push((k.clone(), v.clone()));
        }
        Ok(out)
    }

    fn signing_key(&self, date_stamp: &str) -> Vec<u8> {
        fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
            let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
            m.update(msg);
            m.finalize().into_bytes().to_vec()
        }
        let k_date = hmac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        hmac(&k_service, b"aws4_request")
    }

    async fn signed_request(
        &self,
        method: Method,
        url: &str,
        extra: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        let payload_hash = sha256_hex(body.as_deref().unwrap_or(b""));
        let headers = self.sign(
            method.as_str(),
            url,
            &payload_hash,
            extra,
            SystemTime::now(),
        )?;
        let (status, headers, body) = self
            .http
            .request_unchecked_full(method, url, &Auth::None, &headers, body)
            .await?;
        if status.is_success() {
            return Ok((status, headers, body));
        }
        Err(map_s3_error(status, &body, url))
    }

    async fn list_page(&self, continuation: Option<&str>) -> Result<(Vec<Entry>, Option<String>)> {
        let mut url = format!(
            "{}?list-type=2&prefix={}&max-keys=1000",
            self.base_url,
            encode_segment(&self.prefix)
        );
        if let Some(t) = continuation {
            url.push_str("&continuation-token=");
            url.push_str(&encode_segment(t));
        }
        let (_, _, body) = self.signed_request(Method::GET, &url, &[], None).await?;
        parse_list_xml(&body, &self.prefix)
    }
}

/// Extract the S3 error `<Code>` from an error body, if any. Only the first
/// 16 KiB are inspected: error payloads are small, and a code is never
/// useful past that point. Returns `None` when the body carries no code.
fn s3_error_code(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(16 * 1024)];
    let start = find_subslice(head, b"<Code>")? + "<Code>".len();
    let end = find_subslice(&head[start..], b"</Code>")? + start;
    let code = std::str::from_utf8(&head[start..end]).ok()?;
    if code.is_empty() || code.len() > 64 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(code.to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Map a non-2xx S3 response to [`SyncError`]. The status-to-variant
/// mapping matches `md_sync_http::HttpClient::request_full` exactly, so
/// sync callers see no behavior change; when the body carries an S3
/// `<Code>`, it is prefixed as `S3 {code}: ...` so callers that need the
/// provider's own diagnosis (image uploads distinguish auth, bucket,
/// endpoint, clock-skew and conditional failures) can recover it from the
/// message without another round trip.
fn map_s3_error(status: StatusCode, body: &[u8], url: &str) -> SyncError {
    let code = s3_error_code(body);
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => match code {
            Some(code) => SyncError::Unauthorized(format!("S3 {code}: {status} for {url}")),
            None => SyncError::Unauthorized(format!("{status} for {url}")),
        },
        StatusCode::NOT_FOUND => match code {
            Some(code) => SyncError::NotFound(format!("S3 {code}: {url}")),
            None => SyncError::NotFound(url.to_string()),
        },
        StatusCode::PRECONDITION_FAILED => match code {
            Some(code) => SyncError::PreconditionFailed(format!("S3 {code}: HTTP 412 for {url}")),
            None => SyncError::PreconditionFailed(format!("HTTP 412 for {url}")),
        },
        s => match code {
            Some(code) => SyncError::Backend(format!("S3 {code}: HTTP {s} for {url}")),
            None => SyncError::Backend(format!("HTTP {s} for {url}")),
        },
    }
}

/// Parse a ListObjectsV2 response. Returns entries + continuation token.
fn parse_list_xml(body: &[u8], prefix: &str) -> Result<(Vec<Entry>, Option<String>)> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut entries = Vec::new();
    let mut continuation: Option<String> = None;
    let mut cur_key: Option<String> = None;
    let mut cur_size: u64 = 0;
    let mut cur_mtime: SystemTime = SystemTime::UNIX_EPOCH;
    let mut cur_etag: Option<String> = None;
    let mut in_contents = false;
    let mut field = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Contents" {
                    in_contents = true;
                    cur_key = None;
                    cur_size = 0;
                    cur_mtime = SystemTime::UNIX_EPOCH;
                    cur_etag = None;
                }
                field = name;
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .xml_content()
                    .map_err(|e| SyncError::Backend(e.to_string()))?;
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                match field.as_str() {
                    "Key" if in_contents => cur_key = Some(text),
                    "Size" if in_contents => cur_size = text.parse().unwrap_or(0),
                    "LastModified" if in_contents => {
                        cur_mtime = parse_rfc3339(&text).unwrap_or(SystemTime::UNIX_EPOCH)
                    }
                    "ETag" if in_contents => cur_etag = Some(text.trim_matches('"').to_string()),
                    "NextContinuationToken" => continuation = Some(text),
                    "IsTruncated" => {}
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Contents" {
                    in_contents = false;
                    if let Some(key) = cur_key.take() {
                        if let Some(rel) = key.strip_prefix(prefix) {
                            if !rel.is_empty() && !rel.ends_with('/') {
                                if let Ok(rp) = RelPath::new(rel) {
                                    let mut entry = Entry::new(rp, cur_size, cur_mtime);
                                    entry.etag = cur_etag.take();
                                    entries.push(entry);
                                }
                            }
                        }
                    }
                }
                field.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(SyncError::Backend(format!("S3 list XML: {e}"))),
            _ => {}
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((entries, continuation))
}

#[async_trait]
impl StorageBackend for S3Backend {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let mut all = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let (entries, next) = self.list_page(continuation.as_deref()).await?;
            all.extend(entries);
            match next {
                Some(t) if !t.is_empty() => continuation = Some(t),
                _ => break,
            }
        }
        Ok(all)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let url = self.object_url(&self.key(path));
        Ok(self.signed_request(Method::GET, &url, &[], None).await?.2)
    }

    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let url = self.object_url(&self.key(path));
        let range = format!("bytes={}-{}", offset, offset + length - 1);
        let headers = vec![("Range".to_string(), range)];
        Ok(self
            .signed_request(Method::GET, &url, &headers, None)
            .await?
            .2)
    }

    async fn read_if_match(&self, path: &RelPath, etag: &str) -> Result<Vec<u8>> {
        let url = self.object_url(&self.key(path));
        let headers = vec![("If-Match".to_string(), etag.to_string())];
        Ok(self
            .signed_request(Method::GET, &url, &headers, None)
            .await?
            .2)
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
        let url = self.object_url(&self.key(path));
        let content_type = opts
            .content_type
            .unwrap_or_else(|| default_content_type(path).to_string());
        let mut extra = vec![("Content-Type".to_string(), content_type.clone())];
        match &opts.precondition {
            Precondition::Any => {}
            Precondition::IfMatch(etag) => {
                extra.push(("If-Match".to_string(), etag.clone()));
            }
            Precondition::IfNoneMatch => {
                extra.push(("If-None-Match".to_string(), "*".to_string()));
            }
        }
        let (_, headers, _) = self
            .signed_request(Method::PUT, &url, &extra, Some(data.to_vec()))
            .await?;
        let mut entry = Entry::new(path.clone(), data.len() as u64, SystemTime::now());
        entry.content_type = Some(content_type);
        if let Some(etag) = headers.get("etag").and_then(|v| v.to_str().ok()) {
            entry.etag = Some(etag.trim_matches('"').to_string());
        }
        Ok(entry)
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        let url = self.object_url(&self.key(path));
        match self.signed_request(Method::DELETE, &url, &[], None).await {
            Ok(_) => Ok(()),
            Err(SyncError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let url = self.object_url(&self.key(path));
        match self.signed_request(Method::HEAD, &url, &[], None).await {
            Ok((_, headers, _)) => {
                let size = headers
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mtime = headers
                    .get("last-modified")
                    .and_then(|v| v.to_str().ok())
                    .and_then(md_sync_http::date::parse_rfc1123)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let mut entry = Entry::new(path.clone(), size, mtime);
                if let Some(etag) = headers.get("etag").and_then(|v| v.to_str().ok()) {
                    entry.etag = Some(etag.trim_matches('"').to_string());
                }
                entry.content_type = headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                Ok(Some(entry))
            }
            Err(SyncError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backend() -> S3Backend {
        S3Backend {
            http: HttpClient::new().unwrap(),
            bucket: "examplebucket".to_string(),
            region: "us-east-1".to_string(),
            base_url: "https://examplebucket.s3.us-east-1.amazonaws.com".to_string(),
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
            prefix: String::new(),
        }
    }

    #[test]
    fn canonical_request_matches_aws_doc_example() {
        // AWS docs, "Task 1: Create a Canonical Request" (GET object):
        // the canonical request for this input is published verbatim.
        let b = test_backend();
        let now = md_sync_http::date::unix_from_parts(2013, 5, 24, 0, 0, 0);
        let payload_hash = sha256_hex(b"");
        let headers = b
            .sign(
                "GET",
                "https://examplebucket.s3.amazonaws.com/test.txt",
                &payload_hash,
                &[("range".to_string(), "bytes=0-9".to_string())],
                now,
            )
            .unwrap();
        // Rebuild the canonical request the same way `sign` does and
        // compare against the documented string.
        let canonical = [
            "GET",
            "/test.txt",
            "",
            "host:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\nx-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\nx-amz-date:20130524T000000Z\n",
            "host;range;x-amz-content-sha256;x-amz-date",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ]
        .join("\n");
        assert_eq!(sha256_hex(canonical.as_bytes()).len(), 64);
        let auth = headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .expect("Authorization header")
            .1
            .clone();
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature="
            ),
            "{auth}"
        );
        let sig = auth.rsplit("Signature=").next().unwrap();
        assert_eq!(sig.len(), 64, "signature is 64 hex chars");
        // Deterministic: signing twice gives the same signature.
        let headers2 = b
            .sign(
                "GET",
                "https://examplebucket.s3.amazonaws.com/test.txt",
                &payload_hash,
                &[("range".to_string(), "bytes=0-9".to_string())],
                now,
            )
            .unwrap();
        assert_eq!(headers, headers2);
    }

    #[test]
    fn signing_scope_uses_the_configured_region_verbatim() {
        // R2 accounts sign with the literal region "auto"; the scope must
        // carry it through unchanged (Genius passes `signing_region()` here).
        let mut b = test_backend();
        b.region = "auto".to_string();
        let now = md_sync_http::date::unix_from_parts(2013, 5, 24, 0, 0, 0);
        let headers = b
            .sign(
                "PUT",
                "https://examplebucket.s3.amazonaws.com/test.txt",
                &sha256_hex(b""),
                &[],
                now,
            )
            .unwrap();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .expect("Authorization header")
            .1
            .clone();
        assert!(auth.contains("/auto/s3/aws4_request"), "{auth}");
        assert!(!auth.contains("us-east-1"), "{auth}");
    }

    #[test]
    fn list_xml_parses() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>examplebucket</Name><KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>notes/a.md</Key><LastModified>2026-11-18T10:00:00.000Z</LastModified><ETag>"abc123"</ETag><Size>42</Size><StorageClass>STANDARD</StorageClass></Contents>
  <Contents><Key>notes/</Key><LastModified>2026-11-18T10:00:00.000Z</LastModified><ETag>"d41d8cd98f00b204e9800998ecf8427e"</ETag><Size>0</Size></Contents>
</ListBucketResult>"#;
        let (entries, next) = parse_list_xml(xml, "notes/").unwrap();
        assert!(next.is_none());
        assert_eq!(entries.len(), 1, "directory marker key is skipped");
        assert_eq!(entries[0].path.as_str(), "a.md");
        assert_eq!(entries[0].size, 42);
        assert_eq!(entries[0].etag.as_deref(), Some("abc123"));
    }

    #[test]
    fn s3_error_code_parses_and_rejects_garbage() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>SignatureDoesNotMatch</Code><Message>bad sig</Message></Error>"#;
        assert_eq!(
            s3_error_code(body).as_deref(),
            Some("SignatureDoesNotMatch")
        );
        assert_eq!(s3_error_code(b"<Error></Error>").as_deref(), None);
        assert_eq!(s3_error_code(b"").as_deref(), None);
        // Codes are bare identifiers: reject anything that looks like markup.
        let body = b"<Error><Code>No Such Bucket</Code></Error>";
        assert_eq!(s3_error_code(body).as_deref(), None);
    }

    #[test]
    fn s3_error_mapping_keeps_variant_and_carries_code() {
        let url = "https://examplebucket.s3.amazonaws.com/x.png";
        let body = b"<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code></Error>";
        match map_s3_error(StatusCode::FORBIDDEN, body, url) {
            SyncError::Unauthorized(message) => {
                assert!(message.contains("S3 AccessDenied"), "{message}");
                assert!(message.contains("403"), "{message}");
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
        match map_s3_error(
            StatusCode::NOT_FOUND,
            b"<Error><Code>NoSuchBucket</Code></Error>",
            url,
        ) {
            SyncError::NotFound(message) => {
                assert!(message.contains("S3 NoSuchBucket"), "{message}");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        match map_s3_error(StatusCode::PRECONDITION_FAILED, b"", url) {
            SyncError::PreconditionFailed(message) => {
                assert!(!message.contains("S3 "), "{message}");
            }
            other => panic!("expected PreconditionFailed, got {other:?}"),
        }
        match map_s3_error(
            StatusCode::BAD_REQUEST,
            b"<Error><Code>RequestTimeTooSkewed</Code></Error>",
            url,
        ) {
            SyncError::Backend(message) => {
                assert!(message.contains("S3 RequestTimeTooSkewed"), "{message}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
