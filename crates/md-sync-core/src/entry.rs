//! What a backend reports about one file.

use crate::path::RelPath;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// One file (directories are implicit: a path's parents exist if the file does).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub path: RelPath,
    pub size: u64,
    /// Seconds since the Unix epoch, plus sub-second nanos.
    #[serde(default)]
    pub mtime_secs: i64,
    #[serde(default)]
    pub mtime_nanos: u32,
    /// SHA-256 hex of the content, when the backend can provide or compute it.
    /// `None` does not mean "unknown content": the engine falls back to
    /// size + mtime + the sync manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Backend-native checksum (S3 ETag, Drive md5, WebDAV getetag, ...).
    /// Only comparable between entries from the *same* backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Content type as stored (e.g. `text/markdown`, or a tombstone
    /// marker). Used by sync layers that classify objects by type
    /// without downloading them. `None` = the backend did not report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl Entry {
    pub fn new(path: RelPath, size: u64, modified: SystemTime) -> Self {
        let (secs, nanos) = system_time_parts(modified);
        Entry {
            path,
            size,
            mtime_secs: secs,
            mtime_nanos: nanos,
            hash: None,
            etag: None,
            content_type: None,
        }
    }

    pub fn modified(&self) -> SystemTime {
        if self.mtime_secs >= 0 {
            SystemTime::UNIX_EPOCH
                + std::time::Duration::new(self.mtime_secs as u64, self.mtime_nanos)
        } else {
            SystemTime::UNIX_EPOCH
        }
    }
}

pub(crate) fn system_time_parts(t: SystemTime) -> (i64, u32) {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}
