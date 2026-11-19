//! md-sync-core: traits, engine and manifest for markdown sync.
//!
//! This crate is intentionally small: `serde`, `serde_json`, `sha2`,
//! `thiserror` and `async-trait` — no tokio, no HTTP client. A backend
//! brings whatever runtime/transport it needs; the engine only speaks the
//! [`StorageBackend`] trait.
//!
//! Start here:
//! * [`StorageBackend`] — implement this to add a backend.
//! * [`Engine`] / [`SyncMode`] / [`SyncOptions`] — run a sync.
//! * [`Operator`] — the opendal-style handle over one backend.
//! * [`MemBackend`] — in-memory backend for tests and embedding.

pub mod backend;
pub mod engine;
pub mod entry;
pub mod error;
pub mod hash;
pub mod manifest;
pub mod memory;
pub mod operator;
pub mod path;

pub use backend::{
    default_content_type, Builder, Precondition, StaticToken, StorageBackend, TokenProvider,
    WriteOptions,
};
pub use engine::{
    ConflictInfo, ConflictPolicy, Engine, Op, Plan, Report, Side, SyncMode, SyncOptions,
};
pub use entry::Entry;
pub use error::{Result, SyncError};
pub use hash::hash_bytes;
pub use manifest::{FileState, Manifest, MANIFEST_PATH};
pub use memory::MemBackend;
pub use operator::Operator;
pub use path::RelPath;
