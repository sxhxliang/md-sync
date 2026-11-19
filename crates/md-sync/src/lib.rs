//! `md-sync`: the facade crate. One import, feature-gated backends.
//!
//! ```ignore
//! use md_sync::{Engine, Operator, SyncMode};
//! use md_sync::services;
//!
//! // opendal-style: pick a builder, get an operator.
//! let op = Operator::new(services::Fs::default().root("/home/u/notes"))?;
//! op.write("hello.md", b"# Hello").await?;
//!
//! // Or run a sync between any two backends:
//! let engine = Engine::default();
//! let report = engine
//!     .sync(op_a.backend(), op_b.backend(), SyncMode::TwoWay)
//!     .await?;
//! ```
//!
//! Features (see `Cargo.toml`): `fs` (default), `s3`, `webdav`,
//! `gdrive`, `onedrive`, `full`. A backend that is not enabled does not
//! exist in the binary at all — no code, no dependencies, no size.

pub use md_sync_core::*;

/// Backend builders, in the opendal style. Only the builders for enabled
/// features exist here.
pub mod services {
    #[cfg(feature = "dropbox")]
    pub use md_sync_dropbox::DropboxBuilder as Dropbox;
    #[cfg(feature = "fs")]
    pub use md_sync_fs::FsBuilder as Fs;
    #[cfg(feature = "gdrive")]
    pub use md_sync_gdrive::GoogleDriveBuilder as GoogleDrive;
    #[cfg(feature = "managed-gdrive")]
    pub use md_sync_managed_gdrive::ManagedGDriveBuilder as ManagedGDrive;
    #[cfg(feature = "onedrive")]
    pub use md_sync_onedrive::OneDriveBuilder as OneDrive;
    #[cfg(feature = "s3")]
    pub use md_sync_s3::S3Builder as S3;
    #[cfg(feature = "webdav")]
    pub use md_sync_webdav::WebDavBuilder as WebDav;
}

/// Concrete backend types (same feature gates as `services`).
#[cfg(feature = "dropbox")]
pub use md_sync_dropbox::DropboxBackend;
#[cfg(feature = "fs")]
pub use md_sync_fs::FsBackend;
#[cfg(feature = "gdrive")]
pub use md_sync_gdrive::GoogleDriveBackend;
#[cfg(feature = "managed-gdrive")]
pub use md_sync_managed_gdrive::ManagedGDriveBackend;
#[cfg(feature = "managed-gdrive")]
pub use md_sync_managed_gdrive::ManagedNamespace;
#[cfg(feature = "onedrive")]
pub use md_sync_onedrive::OneDriveBackend;
#[cfg(feature = "s3")]
pub use md_sync_s3::S3Backend;
#[cfg(feature = "webdav")]
pub use md_sync_webdav::WebDavBackend;
