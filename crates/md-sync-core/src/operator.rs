//! `Operator`: the opendal-style handle over one backend.
//!
//! ```ignore
//! use md_sync_core::{Operator, services::Fs};
//!
//! let op = Operator::new(Fs::default().root("/home/u/notes"))?;
//! op.write("hello.md", b"# Hello").await?;
//! let data = op.read("hello.md").await?;
//! ```

use crate::backend::{Builder, StorageBackend};
use crate::entry::Entry;
use crate::error::Result;
use crate::path::RelPath;
use std::sync::Arc;

/// A handle to one configured backend.
pub struct Operator {
    backend: Arc<dyn StorageBackend>,
}

impl Operator {
    /// Build the backend from its builder and wrap it.
    pub fn new<B: Builder>(builder: B) -> Result<Self> {
        Ok(Operator {
            backend: builder.build()?,
        })
    }

    /// Wrap an already-built backend (e.g. one shared by two operators).
    pub fn from_backend(backend: Arc<dyn StorageBackend>) -> Self {
        Operator { backend }
    }

    pub fn backend(&self) -> &Arc<dyn StorageBackend> {
        &self.backend
    }

    pub fn name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn describe(&self) -> String {
        self.backend.describe()
    }

    pub async fn list(&self) -> Result<Vec<Entry>> {
        self.backend.list().await
    }

    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.backend.read(&RelPath::new(path)?).await
    }

    pub async fn write(&self, path: &str, data: &[u8]) -> Result<Entry> {
        self.backend.write(&RelPath::new(path)?, data).await
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.backend.delete(&RelPath::new(path)?).await
    }

    pub async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        self.backend.stat(&RelPath::new(path)?).await
    }
}
