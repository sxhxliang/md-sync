//! The sync manifest: what the last successful sync looked like.
//!
//! Stored as `.md-sync.json` at the root of each backend, one JSON object:
//! `{ "version": 1, "files": { "notes/todo.md": { "hash": "...", ... } } }`.
//!
//! Why a manifest instead of just comparing the two sides?
//! Comparing sides alone cannot tell *deletion* from *creation*
//! (a file on A only: new on A, or deleted on B?), and it cannot tell a
//! *conflict* (both changed) from a one-sided change. The manifest is the
//! shared baseline that makes all three decidable. It also stops
//! cross-backend mtime flapping: after a copy, S3's LastModified is the
//! upload time, not the source's — the manifest records the state as
//! actually stored, so the next run sees "unchanged".

use crate::entry::Entry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// File name of the manifest inside a backend. Never synced itself.
pub const MANIFEST_PATH: &str = ".md-sync.json";

/// One file's state at the last successful sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub size: u64,
    pub mtime_secs: i64,
    #[serde(default)]
    pub mtime_nanos: u32,
}

impl From<&Entry> for FileState {
    fn from(e: &Entry) -> Self {
        FileState {
            hash: e.hash.clone(),
            etag: e.etag.clone(),
            size: e.size,
            mtime_secs: e.mtime_secs,
            mtime_nanos: e.mtime_nanos,
        }
    }
}

/// The whole manifest. `BTreeMap` keeps the JSON output deterministic.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, FileState>,
}

impl Manifest {
    pub fn new() -> Self {
        Manifest {
            version: 1,
            files: BTreeMap::new(),
        }
    }

    /// Parse; a corrupt manifest is treated as empty by the caller (see
    /// `Engine::load_manifest`), never as a hard failure.
    pub fn parse(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    pub fn get(&self, path: &str) -> Option<&FileState> {
        self.files.get(path)
    }

    pub fn insert(&mut self, entry: &Entry) {
        self.files
            .insert(entry.path.as_str().to_string(), FileState::from(entry));
    }

    /// Build a manifest from a fresh listing.
    pub fn from_entries(entries: &[Entry]) -> Self {
        let mut m = Manifest::new();
        for e in entries {
            m.insert(e);
        }
        m
    }
}
