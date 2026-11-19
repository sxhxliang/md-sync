//! OAuth2 refresh-token flow, shared by the drive backends (Dropbox,
//! OneDrive, Google Drive).
//!
//! The application hands over `client_id` / `client_secret` /
//! `refresh_token`; this helper fetches short-lived access tokens on
//! demand and refreshes them when they expire (or when a request comes
//! back 401 with that token). Rotating refresh tokens are kept inside
//! the helper, mirroring the contract of the native drive adapters it
//! replaces: the saved refresh token may still be reused according to
//! the provider's refresh contract.

use crate::encode::encode_form;
use crate::{Auth, HttpClient};
use md_sync_core::{Result, SyncError};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Static configuration for one OAuth account.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// Token endpoint, e.g. `https://api.dropboxapi.com/oauth2/token`.
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// Extra scope for the refresh grant (OneDrive needs
    /// `offline_access Files.ReadWrite`); `None` for the others.
    pub scope: Option<String>,
}

struct State {
    refresh_token: String,
    token: String,
    expires: Instant,
}

/// A refresh-token holder with an expiry cache and 401 retry.
///
/// One instance per backend. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct OAuthRefresher {
    config: OAuthConfig,
    state: Arc<Mutex<State>>,
}

impl OAuthRefresher {
    pub fn new(config: OAuthConfig) -> Self {
        let refresh_token = config.refresh_token.clone();
        OAuthRefresher {
            config,
            state: Arc::new(Mutex::new(State {
                refresh_token,
                token: String::new(),
                expires: Instant::now(),
            })),
        }
    }

    /// A valid access token, refreshing first when the cached one is
    /// missing, expired, or was just rejected (`rejected`).
    pub async fn token(&self, http: &HttpClient, rejected: Option<&str>) -> Result<String> {
        let mut state = self.state.lock().await;
        if !state.token.is_empty()
            && state.expires > Instant::now()
            && rejected != Some(state.token.as_str())
        {
            return Ok(state.token.clone());
        }
        let mut pairs: Vec<(String, String)> = vec![
            ("grant_type".into(), "refresh_token".into()),
            ("client_id".into(), self.config.client_id.clone()),
            ("refresh_token".into(), state.refresh_token.clone()),
        ];
        if !self.config.client_secret.is_empty() {
            pairs.push(("client_secret".into(), self.config.client_secret.clone()));
        }
        if let Some(scope) = &self.config.scope {
            pairs.push(("scope".into(), scope.clone()));
        }
        let body = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", encode_form(k), encode_form(v)))
            .collect::<Vec<_>>()
            .join("&");
        let (status, _, bytes) = http
            .request_unchecked_full(
                Method::POST,
                &self.config.token_url,
                &Auth::None,
                &[(
                    "Content-Type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                )],
                Some(body.into_bytes()),
            )
            .await?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(SyncError::Unauthorized(format!(
                "OAuth refresh failed with HTTP {status}"
            )));
        }
        if !status.is_success() {
            return Err(SyncError::Backend(format!(
                "OAuth refresh failed with HTTP {status}"
            )));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(SyncError::from)?;
        let token = value["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SyncError::Backend("OAuth refresh: missing access_token".into()))?
            .to_owned();
        // Rotating refresh tokens stay inside this helper.
        if let Some(refresh) = value["refresh_token"].as_str() {
            state.refresh_token = refresh.to_string();
        }
        let seconds = value["expires_in"]
            .as_u64()
            .unwrap_or(3600)
            .saturating_sub(60)
            .max(1);
        state.expires = Instant::now() + Duration::from_secs(seconds);
        state.token = token.clone();
        Ok(token)
    }

    /// Send an authenticated request, retrying once with a fresh token
    /// when the first attempt comes back 401. Returns the raw
    /// `(status, headers, body)` so callers can interpret 409/412/308
    /// and provider-specific headers themselves.
    pub async fn request(
        &self,
        http: &HttpClient,
        method: Method,
        url: &str,
        headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        let token = self.token(http, None).await?;
        let (status, resp_headers, bytes) = http
            .request_unchecked_full(
                method.clone(),
                url,
                &Auth::Bearer(token.clone()),
                headers,
                body.clone(),
            )
            .await?;
        if status != StatusCode::UNAUTHORIZED {
            return Ok((status, resp_headers, bytes));
        }
        // The cached token was rejected: refresh past it and retry once.
        let token = self.token(http, Some(&token)).await?;
        http.request_unchecked_full(method, url, &Auth::Bearer(token), headers, body)
            .await
    }
}
