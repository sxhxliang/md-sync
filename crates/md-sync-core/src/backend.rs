//! The one trait every storage backend implements.
//!
//! Design notes (Occam's razor applied to the interface itself):
//! * Five methods + two metadata methods form the core. No streaming, no
//!   locking: a markdown document fits in memory, and sync needs whole
//!   files. Three more methods grow on top of that core for sync layers
//!   that need compare-and-swap (`write_with_options`, `read_range`,
//!   `read_if_match`); each has a safe default, so a backend that cannot
//!   do the real thing refuses instead of silently degrading.
//! * `async-trait` (one tiny proc-macro) buys `dyn StorageBackend`, so the
//!   engine and the CLI can hold *any* backend behind one pointer. The
//!   alternative — hand-boxed futures in every method — would tax every
//!   backend author instead.
//! * The core crate has **no** tokio / HTTP dependency: `async fn` here is
//!   only syntax. Backends bring their own runtime pieces.

use crate::entry::Entry;
use crate::error::{Result, SyncError};
use crate::path::RelPath;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;

/// A precondition for a conditional write or read: compare-and-swap
/// against the object's current revision token (etag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// No condition: create or replace unconditionally.
    Any,
    /// Write/read only if the object currently has this etag.
    IfMatch(String),
    /// Write only if the object does not exist.
    IfNoneMatch,
}

/// Options for [`StorageBackend::write_with_options`].
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Content type to store. `None` means the backend's default for the
    /// extension (markdown → `text/markdown`, otherwise octet-stream).
    /// Sync layers use this to mark tombstones and other typed objects.
    pub content_type: Option<String>,
    /// Revision precondition. Default [`Precondition::Any`].
    pub precondition: Precondition,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            content_type: None,
            precondition: Precondition::Any,
        }
    }
}

impl WriteOptions {
    pub fn with_precondition(mut self, precondition: Precondition) -> Self {
        self.precondition = precondition;
        self
    }

    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
}

/// The content type a backend stores when the caller does not override
/// it: markdown gets `text/markdown`, everything else octet-stream.
pub fn default_content_type(path: &RelPath) -> &'static str {
    match path.extension().as_deref() {
        Some("md") | Some("markdown") => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A storage backend: a flat namespace of files addressed by [`RelPath`].
///
/// `list` returns files only, recursively, sorted by path. Directories are
/// implicit. The sync manifest file (`.md-sync.json`) is stored through the
/// same methods but is never returned by `list` — backends must filter it,
/// or the engine filters it (the engine does, so backends need not care).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Short stable name, e.g. `fs`, `s3`, `webdav`. Used in reports only.
    fn name(&self) -> &'static str;

    /// Human-readable location without secrets, e.g. `fs:/home/u/notes`.
    fn describe(&self) -> String;

    /// All files, recursively, sorted by path.
    async fn list(&self) -> Result<Vec<Entry>>;

    /// Whole file content. `NotFound` if the path does not exist.
    async fn read(&self, path: &RelPath) -> Result<Vec<u8>>;

    /// Write (create or replace) a whole file, creating parents as needed.
    /// Returns the entry as stored (hash/etag may be filled in).
    async fn write(&self, path: &RelPath, data: &[u8]) -> Result<Entry>;

    /// Write with a content-type override and/or a revision precondition.
    /// Default: if a precondition is asked for, refuse with
    /// [`SyncError::Unsupported`] (a CAS caller must never mistake
    /// "not checked" for "checked"); otherwise delegate to [`Self::write`].
    async fn write_with_options(
        &self,
        path: &RelPath,
        data: &[u8],
        opts: WriteOptions,
    ) -> Result<Entry> {
        if opts.precondition != Precondition::Any {
            return Err(SyncError::Unsupported(format!(
                "{} backend does not support conditional writes",
                self.name()
            )));
        }
        let _ = opts.content_type;
        self.write(path, data).await
    }

    /// Read a byte range `[offset, offset + length)`. Default: read the
    /// whole file and slice it (correct, but wasteful for large files;
    /// backends with native range support override this).
    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let data = self.read(path).await?;
        let start = offset as usize;
        if start >= data.len() {
            return Ok(Vec::new());
        }
        let end = (start + length as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }

    /// Read only if the object currently has this etag; otherwise
    /// [`SyncError::PreconditionFailed`]. Default: plain [`Self::read`]
    /// (the caller still verifies content afterwards, so a stale read is
    /// caught — it just is not fenced at the server).
    async fn read_if_match(&self, path: &RelPath, _etag: &str) -> Result<Vec<u8>> {
        self.read(path).await
    }

    /// Delete a file. Succeeds silently if it is already gone (idempotent).
    async fn delete(&self, path: &RelPath) -> Result<()>;

    /// Metadata for one file, or `None` if it does not exist.
    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>>;

    /// Create a directory (and parents). Default: no-op — object stores
    /// (S3, Drive, OneDrive) have no real directories.
    async fn mkdir(&self, _path: &RelPath) -> Result<()> {
        Ok(())
    }

    /// Best-effort: set a file's modification time to the source's, so a
    /// copied document keeps its original date. Default: no-op — most
    /// remote stores cannot set it (S3 sets LastModified on upload).
    /// The engine ignores failures here.
    async fn set_modified(&self, _path: &RelPath, _time: SystemTime) -> Result<()> {
        Ok(())
    }
}

/// A builder in the opendal style: configure, then `build()`.
///
/// `Operator::new(builder)` takes any `Builder`, so user code looks like:
///
/// ```ignore
/// let op = Operator::new(services::S3::default().bucket("notes"))?;
/// ```
pub trait Builder: Send {
    fn build(self) -> Result<Arc<dyn StorageBackend>>;
}

/// Supplies an access token for backends that speak OAuth2 / Bearer auth
/// (Google Drive, OneDrive). The library never does the OAuth dance itself:
/// the application owns login/refresh and hands the library a token, or a
/// callback that fetches a fresh one. That keeps OAuth, its HTTP calls and
/// its token storage out of this small library.
pub trait TokenProvider: Send + Sync {
    fn token(&self) -> Result<String>;
}

/// The simplest provider: a token the application already holds.
pub struct StaticToken(pub String);

impl TokenProvider for StaticToken {
    fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}
