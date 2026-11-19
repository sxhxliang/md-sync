//! Shared HTTP plumbing for the remote backends.
//!
//! One `reqwest` client (rustls only — no OpenSSL, no native-tls), one
//! error mapping, and the small date/encoding helpers every remote backend
//! needs. Nothing here knows about S3, WebDAV or Drive: it is transport.

pub mod date;
pub mod encode;
pub mod oauth;

use md_sync_core::{Result, SyncError};
use reqwest::{Method, StatusCode};

/// How a request authenticates. Tokens are `String`s owned by the caller;
/// this crate never logs them.
#[derive(Debug, Clone)]
pub enum Auth {
    None,
    Basic { user: String, pass: String },
    Bearer(String),
}

/// A thin wrapper over `reqwest::Client` that maps failures to [`SyncError`].
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    pub fn new() -> Result<Self> {
        let inner = reqwest::Client::builder()
            .user_agent(concat!("md-sync/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        Ok(HttpClient { inner })
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Send a request and return `(status, body bytes)`.
    /// 401/403 -> `Unauthorized`, 404 -> `NotFound`, other 4xx/5xx ->
    /// `Backend` with the status (body is not included: it may contain
    /// provider internals, and it is rarely useful at sync granularity).
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        auth: &Auth,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let owned: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let (status, _h, body) = self.request_full(method, url, auth, &owned, body).await?;
        Ok((status, body))
    }

    /// Like [`HttpClient::request`], but headers are owned (callers that
    /// compute signatures need that) and response headers are returned,
    /// so callers can read `ETag` / `Content-Length` / `Last-Modified`.
    pub async fn request_full(
        &self,
        method: Method,
        url: &str,
        auth: &Auth,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        let mut req = self.inner.request(method, url);
        match auth {
            Auth::None => {}
            Auth::Basic { user, pass } => {
                req = req.basic_auth(user, Some(pass));
            }
            Auth::Bearer(t) => {
                req = req.bearer_auth(t);
            }
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?
            .to_vec();
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(SyncError::Unauthorized(format!("{status} for {url}")))
            }
            StatusCode::NOT_FOUND => Err(SyncError::NotFound(url.to_string())),
            StatusCode::PRECONDITION_FAILED => {
                Err(SyncError::PreconditionFailed(format!("HTTP 412 for {url}")))
            }
            s if s.is_success() => Ok((status, resp_headers, bytes)),
            s => Err(SyncError::Backend(format!("HTTP {s} for {url}"))),
        }
    }

    /// Send a request and return `(status, headers, body)` without mapping
    /// HTTP error statuses to [`SyncError`]. For calls where the caller
    /// must read response headers on non-2xx statuses (e.g. 308 resume,
    /// 401 token retry, `Dropbox-API-Result` on download).
    pub async fn request_unchecked_full(
        &self,
        method: Method,
        url: &str,
        auth: &Auth,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        let mut req = self.inner.request(method, url);
        match auth {
            Auth::None => {}
            Auth::Basic { user, pass } => {
                req = req.basic_auth(user, Some(pass));
            }
            Auth::Bearer(t) => {
                req = req.bearer_auth(t);
            }
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?
            .to_vec();
        Ok((status, resp_headers, bytes))
    }

    pub async fn get(&self, url: &str, auth: &Auth) -> Result<Vec<u8>> {
        Ok(self.request(Method::GET, url, auth, &[], None).await?.1)
    }

    /// Send a request and return `(status, body)` without mapping HTTP
    /// error statuses to [`SyncError`]. For calls where the caller must
    /// distinguish statuses itself (e.g. MKCOL: 201 created vs 405 exists).
    pub async fn request_unchecked(
        &self,
        method: Method,
        url: &str,
        auth: &Auth,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let mut req = self.inner.request(method, url);
        match auth {
            Auth::None => {}
            Auth::Basic { user, pass } => {
                req = req.basic_auth(user, Some(pass));
            }
            Auth::Bearer(t) => {
                req = req.bearer_auth(t);
            }
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SyncError::Backend(e.to_string()))?
            .to_vec();
        Ok((status, bytes))
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new().expect("reqwest client builds with default settings")
    }
}

/// Parse a JSON body, mapping the error to `SyncError::Serde`.
pub fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(SyncError::from)
}
