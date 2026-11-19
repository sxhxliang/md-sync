//! Local filesystem backend.
//!
//! A root directory on disk; every [`RelPath`] is a file under it, and the
//! path validation in `RelPath` guarantees nothing escapes the root.
//! File hashes are computed on `list`/`stat` (local reads are cheap), so
//! the engine can compare content exactly even against remote backends.

use async_trait::async_trait;
use md_sync_core::backend::Builder;
use md_sync_core::{
    default_content_type, hash::hash_bytes, Entry, Precondition, RelPath, Result, StorageBackend,
    SyncError, WriteOptions,
};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// A local directory used as a sync backend.
pub struct FsBackend {
    root: PathBuf,
}

impl FsBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FsBackend { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &RelPath) -> PathBuf {
        // RelPath is slash-separated and validated (no `..`, no absolute),
        // so joining it under root cannot escape.
        path.as_str()
            .split('/')
            .fold(self.root.clone(), |p, seg| p.join(seg))
    }

    async fn entry_for(&self, disk: &Path, rel: &RelPath) -> Result<Entry> {
        let meta = tokio::fs::metadata(disk).await?;
        let data = tokio::fs::read(disk).await?;
        let mut e = Entry::new(
            rel.clone(),
            meta.len(),
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        );
        e.hash = Some(hash_bytes(&data));
        e.content_type = Some(default_content_type(rel).to_string());
        Ok(e)
    }

    async fn list_dir(&self, dir: &Path, prefix: &str, out: &mut Vec<Entry>) -> Result<()> {
        let mut rd = match tokio::fs::read_dir(dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(SyncError::from(e)),
        };
        while let Some(ent) = rd.next_entry().await? {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let ft = ent.file_type().await?;
            if ft.is_dir() {
                if name == ".md-sync" {
                    continue;
                }
                // Box the recursion: async fn recursion needs indirection.
                Box::pin(self.list_dir(&ent.path(), &rel, out)).await?;
            } else if ft.is_file() {
                if let Ok(rp) = RelPath::new(&rel) {
                    out.push(self.entry_for(&ent.path(), &rp).await?);
                }
            }
            // Symlinks and other types are ignored: a sync tool must not
            // follow a link out of its root.
        }
        Ok(())
    }
}

/// Builder in the opendal style: `Fs::default().root("/path").build()`.
#[derive(Debug, Default)]
pub struct FsBuilder {
    root: Option<PathBuf>,
}

impl FsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }
}

impl Builder for FsBuilder {
    fn build(self) -> Result<Arc<dyn StorageBackend>> {
        let root = self
            .root
            .ok_or_else(|| SyncError::InvalidConfig("fs backend: root is required".into()))?;
        std::fs::create_dir_all(&root)?;
        Ok(Arc::new(FsBackend::new(root)))
    }
}

#[async_trait]
impl StorageBackend for FsBackend {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn describe(&self) -> String {
        format!("fs:{}", self.root.display())
    }

    async fn list(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.list_dir(&self.root, "", &mut out).await?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        Ok(tokio::fs::read(self.resolve(path)).await?)
    }

    async fn read_range(&self, path: &RelPath, offset: u64, length: u64) -> Result<Vec<u8>> {
        let mut f = tokio::fs::File::open(self.resolve(path)).await?;
        f.seek(SeekFrom::Start(offset)).await?;
        let mut buf = Vec::new();
        f.take(length).read_to_end(&mut buf).await?;
        Ok(buf)
    }

    async fn write(&self, path: &RelPath, data: &[u8]) -> Result<Entry> {
        let disk = self.resolve(path);
        if let Some(parent) = disk.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&disk, data).await?;
        self.entry_for(&disk, path).await
    }

    /// Weak conditional semantics, documented: the fs backend has no
    /// etags, so `IfMatch` only checks the file exists (the etag is
    /// ignored) and `IfNoneMatch` only checks it does not. Do not use
    /// the fs backend where a real compare-and-swap matters.
    async fn write_with_options(
        &self,
        path: &RelPath,
        data: &[u8],
        opts: WriteOptions,
    ) -> Result<Entry> {
        match opts.precondition {
            Precondition::Any => {}
            Precondition::IfMatch(_) => {
                if self.stat(path).await?.is_none() {
                    return Err(SyncError::PreconditionFailed(format!(
                        "fs: {path} does not exist"
                    )));
                }
            }
            Precondition::IfNoneMatch => {
                if self.stat(path).await?.is_some() {
                    return Err(SyncError::PreconditionFailed(format!(
                        "fs: {path} already exists"
                    )));
                }
            }
        }
        let mut entry = self.write(path, data).await?;
        if let Some(ct) = opts.content_type {
            entry.content_type = Some(ct);
        }
        Ok(entry)
    }

    async fn delete(&self, path: &RelPath) -> Result<()> {
        match tokio::fs::remove_file(self.resolve(path)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SyncError::from(e)),
        }
    }

    async fn stat(&self, path: &RelPath) -> Result<Option<Entry>> {
        let disk = self.resolve(path);
        match tokio::fs::metadata(&disk).await {
            Ok(_) => Ok(Some(self.entry_for(&disk, path).await?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SyncError::from(e)),
        }
    }

    async fn mkdir(&self, path: &RelPath) -> Result<()> {
        tokio::fs::create_dir_all(self.resolve(path)).await?;
        Ok(())
    }

    async fn set_modified(&self, path: &RelPath, time: SystemTime) -> Result<()> {
        let f = tokio::fs::File::open(self.resolve(path)).await?;
        f.set_times(tokio::fs::FileTimes::new().set_modified(time))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn tmp_root() -> PathBuf {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("md-sync-fs-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[tokio::test]
    async fn roundtrip_and_list() {
        let root = tmp_root();
        let b = FsBuilder::new().root(&root).build().unwrap();
        b.write(&RelPath::new("notes/a.md").unwrap(), b"# a")
            .await
            .unwrap();
        b.write(&RelPath::new("b.md").unwrap(), b"# b")
            .await
            .unwrap();
        let list = b.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].hash.is_some());
        assert_eq!(
            b.read(&RelPath::new("notes/a.md").unwrap()).await.unwrap(),
            b"# a"
        );
        b.delete(&RelPath::new("b.md").unwrap()).await.unwrap();
        assert!(b
            .stat(&RelPath::new("b.md").unwrap())
            .await
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn conditional_write_and_range_read() {
        let root = tmp_root();
        let b = FsBuilder::new().root(&root).build().unwrap();
        let p = RelPath::new("a.md").unwrap();
        // IfNoneMatch succeeds on a missing file, fails once it exists.
        b.write_with_options(
            &p,
            b"0123456789",
            WriteOptions::default().with_precondition(Precondition::IfNoneMatch),
        )
        .await
        .unwrap();
        let err = b
            .write_with_options(
                &p,
                b"x",
                WriteOptions::default().with_precondition(Precondition::IfNoneMatch),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::PreconditionFailed(_)), "{err:?}");
        // IfMatch succeeds on an existing file, fails on a missing one.
        b.write_with_options(
            &p,
            b"abcdef",
            WriteOptions::default().with_precondition(Precondition::IfMatch("ignored".into())),
        )
        .await
        .unwrap();
        let missing = RelPath::new("nope.md").unwrap();
        let err = b
            .write_with_options(
                &missing,
                b"x",
                WriteOptions::default().with_precondition(Precondition::IfMatch("e".into())),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::PreconditionFailed(_)), "{err:?}");
        assert_eq!(b.read_range(&p, 2, 3).await.unwrap(), b"cde");
        assert!(b.read_range(&p, 99, 3).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
