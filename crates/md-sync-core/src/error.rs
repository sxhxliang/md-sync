//! Error type for the whole workspace. One error, no `anyhow` in the library:
//! callers decide how to display it, the library only classifies it.

use thiserror::Error;

/// All failures a backend or the sync engine can report.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("unauthorized (check credentials/token): {0}")]
    Unauthorized(String),

    #[error("conflict: {0}")]
    Conflict(String),

    /// A conditional write/read precondition failed (HTTP 412, or the
    /// backend's equivalent): the object changed since the caller last
    /// saw it. Callers that implement compare-and-swap treat this as
    /// "someone else won", not as a transport error.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),

    /// The backend cannot do what was asked (e.g. conditional writes on
    /// a store without etags). Refuse rather than silently degrade: a
    /// CAS caller must never mistake "not checked" for "checked".
    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("serialization error: {0}")]
    Serde(String),
}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => SyncError::NotFound(e.to_string()),
            _ => SyncError::Io(e.to_string()),
        }
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(e: serde_json::Error) -> Self {
        SyncError::Serde(e.to_string())
    }
}

/// Shorthand used across the workspace.
pub type Result<T> = std::result::Result<T, SyncError>;
