//! A validated, slash-separated relative path inside a backend.
//!
//! Rules (kept deliberately strict: a sync tool must never escape its root):
//! * never absolute, never empty, never `.` / `..` as a segment
//! * `/` is the only separator; `\` is rejected, not silently converted
//! * no empty segments (`a//b`), no NUL, no trailing `/`

use crate::error::{Result, SyncError};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Relative path of one document inside a backend, e.g. `notes/todo.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RelPath(String);

impl RelPath {
    /// Validate and normalise a path string.
    pub fn new(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(SyncError::InvalidPath("empty path".into()));
        }
        if s.starts_with('/') || s.contains('\\') || s.contains('\0') {
            return Err(SyncError::InvalidPath(format!("not a relative path: {s}")));
        }
        let mut out: Vec<&str> = Vec::new();
        for seg in s.split('/') {
            match seg {
                "" => return Err(SyncError::InvalidPath(format!("empty segment in: {s}"))),
                "." | ".." => return Err(SyncError::InvalidPath(format!("dot segment in: {s}"))),
                _ => out.push(seg),
            }
        }
        Ok(RelPath(out.join("/")))
    }

    /// Build from path segments, e.g. `["notes", "todo.md"]`.
    pub fn from_segments(segments: &[&str]) -> Result<Self> {
        Self::new(&segments.join("/"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parent directory, `None` for a top-level file.
    pub fn parent(&self) -> Option<RelPath> {
        self.0.rfind('/').map(|i| RelPath(self.0[..i].to_string()))
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Lower-cased extension without the dot, e.g. `md`.
    pub fn extension(&self) -> Option<String> {
        self.file_name()
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
    }

    pub fn file_stem(&self) -> &str {
        let name = self.file_name();
        name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name)
    }

    /// `notes/todo.md` -> `notes/todo.conflict-a-1700000000.md`.
    /// The conflict copy keeps the same extension so editors still open it.
    pub fn conflict_name(&self, side: &str, unix_secs: u64) -> RelPath {
        let stem = self.file_stem();
        let ext = self.extension().unwrap_or_default();
        let name = if ext.is_empty() {
            format!("{stem}.conflict-{side}-{unix_secs}")
        } else {
            format!("{stem}.conflict-{side}-{unix_secs}.{ext}")
        };
        match self.parent() {
            Some(p) => RelPath(format!("{}/{name}", p.as_str())),
            None => RelPath(name),
        }
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RelPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
