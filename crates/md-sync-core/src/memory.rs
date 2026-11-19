//! An in-memory backend. Useful for tests, for embedding, and for trying
//! the engine without touching disk or network. Not a persistence story.

use crate::backend::StorageBackend;
use crate::entry::Entry;
use crate::error::{Result, SyncError};
use crate::hash::hash_bytes;
use crate::path::RelPath;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone)]
struct Stored {
    data: Vec<u8>,
    modified: SystemTime,
}

/// `MemBackend`: a `BTreeMap` behind a mutex.
pub struct MemBackend {
    name: &'static str,
    files: Mutex<BTreeMap<String, Stored>>,
}

impl MemBackend {
    pub fn new() -> Self {
        MemBackend {
            name: "memory",
            files: Mutex::new(BTreeMap::new()),
        }
    }

    fn entry_for(path: &RelPath, s: &Stored) -> Entry {
        let mut e = Entry::new(path.clone(), s.data.len() as u64, s.modified);
        e.hash = Some(hash_bytes(&s.data));
        e
    }
}

impl Default for MemBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StorageBackend for MemBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn describe(&self) -> String {
        "memory:".to_string()
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let files = self
            .files
            .lock()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        Ok(files
            .iter()
            .filter_map(|(p, s)| RelPath::new(p).ok().map(|rp| Self::entry_for(&rp, s)))
            .collect())
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let files = self
            .files
            .lock()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        files
            .get(path.as_str())
            .map(|s| s.data.clone())
            .ok_or_else(|| SyncError::NotFound(path.to_string()))
    }

    async fn write(&self, path: &RelPath, data: &[u8]) -> Result<Entry> {
        let mut files = self
            .files
            .lock()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        let now = SystemTime::now();
        files.insert(
            path.as_str().to_string(),
            Stored {
                data: data.to_vec(),
                modified: now,
            },
        );
        let stored = files.get(path.as_str()).unwrap();
        Ok(Self::entry_for(path, stored))
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        let mut files = self
            .files
            .lock()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        files.remove(path.as_str());
        Ok(())
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let files = self
            .files
            .lock()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        Ok(files.get(path.as_str()).map(|s| Self::entry_for(path, s)))
    }

    async fn set_modified(&self, path: &RelPath, time: SystemTime) -> Result<()> {
        let mut files = self
            .files
            .lock()
            .map_err(|e| SyncError::Backend(e.to_string()))?;
        if let Some(s) = files.get_mut(path.as_str()) {
            s.modified = time;
        }
        Ok(())
    }
}
