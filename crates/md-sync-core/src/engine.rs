//! The sync engine: pure logic over two [`StorageBackend`]s.
//!
//! Four modes, one shared core:
//!
//! * **Two-way** — changes flow both directions, deletions propagate, and a
//!   file changed on *both* sides since the last sync is a conflict. This
//!   is the stateful mode: it consults the manifest baseline.
//! * **Bidirectional** — the stateless two-way mode (opendal-sync
//!   semantics): unique files copy both ways, and a file present on both
//!   sides with different content is a conflict, even if only one side
//!   changed since the last sync. Deletions are never propagated and no
//!   baseline is consulted for decisions.
//! * **Mirror** — destination is made to match the source: new/changed
//!   files copy over, and files missing from the source are deleted from
//!   the destination. `OneWay` is the same mode under its old name.
//! * **Backup** — one-way, copy-only, never deletes, and never overwrites a
//!   destination file that is newer than the source (the backup is
//!   protected; the skip is reported, not silent).
//!
//! Two-way, one-way/mirror and backup read the sync manifest
//! (`.md-sync.json`, see `manifest`) as the baseline of the last successful
//! sync. That is what makes create-vs-delete and conflict decidable, and
//! what stops mtime flapping between backends that cannot preserve
//! modification times (S3, WebDAV). Bidirectional never consults a manifest
//! for decisions; after a successful run it still refreshes the manifests
//! to the merged state, so a later two-way run has a correct baseline.

use crate::backend::StorageBackend;
use crate::entry::Entry;
use crate::error::{Result, SyncError};
use crate::manifest::{FileState, Manifest, MANIFEST_PATH};
use crate::path::RelPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which side of a sync an operation touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    A,
    B,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::A => Side::B,
            Side::B => Side::A,
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::A => "A",
            Side::B => "B",
        })
    }
}

/// How to resolve a file changed on both sides since the last sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ConflictPolicy {
    /// Keep the newer file (by mtime); on a tie, keep A. Deterministic.
    #[default]
    KeepNewer,
    /// A always wins.
    KeepA,
    /// B always wins.
    KeepB,
    /// Keep the winner as the canonical file and preserve the loser as
    /// `name.conflict-<side>-<unix-secs>.md` on both sides.
    KeepBoth,
    /// Copy neither; report the conflict and leave both files alone.
    /// The path is kept out of the new manifest so the next run
    /// re-detects it instead of silently accepting the divergence.
    Skip,
}

/// Sync mode (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    /// Stateful two-way: deletions propagate; only files changed on *both*
    /// sides since the last sync are conflicts.
    TwoWay,
    /// Stateless two-way (opendal-sync `Bidirectional`): unique files copy
    /// both ways; any shared file with different content is a conflict.
    /// Deletions never propagate; no manifest is consulted for decisions.
    Bidirectional,
    /// One-way mirror; the old name for [`SyncMode::Mirror`]. Kept for
    /// compatibility; honors `delete_extraneous`.
    OneWay,
    /// One-way mirror: the destination is made to match the source.
    /// Unlike [`SyncMode::OneWay`], always deletes destination files
    /// missing from the source (`delete_extraneous` does not apply).
    Mirror,
    /// Copy-only backup: never deletes, never overwrites a newer file.
    Backup,
}

/// Options for one sync run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncOptions {
    /// File extensions to sync, lower-case, without the dot.
    /// Default: `["md", "markdown"]`.
    pub include_exts: Vec<String>,
    /// Plan only: report what would happen, change nothing.
    pub dry_run: bool,
    /// One-way mode: delete destination files missing from the source.
    /// Two-way always propagates deletions; backup never deletes.
    /// `Mirror` always deletes; `OneWay` deletes only when this is on.
    pub delete_extraneous: bool,
    /// What to do with a both-sides-changed file.
    pub conflict: ConflictPolicy,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions {
            include_exts: vec!["md".to_string(), "markdown".to_string()],
            dry_run: false,
            delete_extraneous: true,
            conflict: ConflictPolicy::KeepNewer,
        }
    }
}

impl SyncOptions {
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

/// One planned/applied operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    /// Copy `path` from one side to the other.
    Copy { from: Side, to: Side, path: RelPath },
    /// Copy `src` on `from` to `dst` on `to` (used for conflict copies).
    CopyAs {
        from: Side,
        to: Side,
        src: RelPath,
        dst: RelPath,
    },
    /// Delete `path` on `side`.
    Delete { side: Side, path: RelPath },
}

/// What happened to one conflicted file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub path: RelPath,
    pub resolution: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_path: Option<RelPath>,
}

/// A plan: what a sync would do, before it does it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    pub ops: Vec<Op>,
    pub conflicts: Vec<ConflictInfo>,
}

/// The outcome of a sync run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub mode: Option<SyncMode>,
    pub dry_run: bool,
    pub planned: Vec<Op>,
    pub ops_applied: usize,
    pub copied_bytes: u64,
    pub deleted: usize,
    #[serde(default)]
    pub conflicts: Vec<ConflictInfo>,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Report {
    /// True when nothing failed (conflicts resolved by policy are not
    /// failures; skipped conflicts are reported, not errors).
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// The engine. Stateless apart from its options; one `Engine` can run
/// many syncs.
#[derive(Default)]
pub struct Engine {
    pub options: SyncOptions,
}

impl Engine {
    pub fn new(options: SyncOptions) -> Self {
        Engine { options }
    }

    /// Run a full sync in the given mode and return its report.
    pub async fn sync(
        &self,
        a: &dyn StorageBackend,
        b: &dyn StorageBackend,
        mode: SyncMode,
    ) -> Result<Report> {
        match mode {
            SyncMode::TwoWay => self.sync_two_way(a, b).await,
            SyncMode::Bidirectional => self.sync_bidirectional(a, b).await,
            SyncMode::OneWay | SyncMode::Mirror | SyncMode::Backup => {
                self.sync_one_way(a, b, mode).await
            }
        }
    }

    // -- two-way -----------------------------------------------------------

    pub async fn plan_two_way(
        &self,
        a: &dyn StorageBackend,
        b: &dyn StorageBackend,
    ) -> Result<(Plan, Vec<String>, Manifest, BTreeSet<String>)> {
        let snap_a = self.snapshot(a).await?;
        let snap_b = self.snapshot(b).await?;
        let (man_a, w_a) = load_manifest(a).await;
        let (man_b, w_b) = load_manifest(b).await;
        let mut warnings = Vec::new();
        warnings.extend(w_a);
        warnings.extend(w_b);
        // Baseline: A wins if both exist; if they disagree, say so — the
        // next successful sync rewrites both identically and converges.
        if !man_a.files.is_empty() && !man_b.files.is_empty() && man_a != man_b {
            warnings
                .push("manifests on the two sides differ; using A's as the baseline".to_string());
        }
        let baseline = if man_a.files.is_empty() && !man_b.files.is_empty() {
            man_b
        } else {
            man_a
        };

        let same_backend = a.name() == b.name();
        let mut plan = Plan::default();
        let mut held_back: BTreeSet<String> = BTreeSet::new(); // skipped conflicts
        let mut keys: BTreeSet<String> = BTreeSet::new();
        keys.extend(snap_a.keys().cloned());
        keys.extend(snap_b.keys().cloned());

        for key in keys {
            let ea = snap_a.get(&key);
            let eb = snap_b.get(&key);
            match (ea, eb) {
                (Some(ae), Some(be)) => {
                    if same_content(ae, be, same_backend) {
                        continue;
                    }
                    let ca = changed_since(ae, baseline.get(&key));
                    let cb = changed_since(be, baseline.get(&key));
                    if !ca && !cb {
                        // Already synced; the two backends just describe
                        // the same content differently (e.g. S3 mtime).
                        continue;
                    }
                    if ca && cb {
                        self.resolve_conflict(&mut plan, &mut held_back, ae, be, "both changed");
                    } else if ca {
                        plan.ops.push(Op::Copy {
                            from: Side::A,
                            to: Side::B,
                            path: ae.path.clone(),
                        });
                    } else {
                        plan.ops.push(Op::Copy {
                            from: Side::B,
                            to: Side::A,
                            path: be.path.clone(),
                        });
                    }
                }
                (Some(ae), None) => {
                    if baseline.get(&key).is_some() {
                        // Was synced before, now gone on B: deleted on B.
                        plan.ops.push(Op::Delete {
                            side: Side::A,
                            path: ae.path.clone(),
                        });
                    } else {
                        plan.ops.push(Op::Copy {
                            from: Side::A,
                            to: Side::B,
                            path: ae.path.clone(),
                        });
                    }
                }
                (None, Some(be)) => {
                    if baseline.get(&key).is_some() {
                        plan.ops.push(Op::Delete {
                            side: Side::B,
                            path: be.path.clone(),
                        });
                    } else {
                        plan.ops.push(Op::Copy {
                            from: Side::B,
                            to: Side::A,
                            path: be.path.clone(),
                        });
                    }
                }
                (None, None) => {}
            }
        }
        Ok((plan, warnings, baseline, held_back))
    }

    fn resolve_conflict(
        &self,
        plan: &mut Plan,
        held_back: &mut BTreeSet<String>,
        ae: &Entry,
        be: &Entry,
        reason: &str,
    ) {
        let path = ae.path.clone();
        let newer_a = mtime_tuple(ae) >= mtime_tuple(be);
        match self.options.conflict {
            ConflictPolicy::KeepNewer | ConflictPolicy::KeepA | ConflictPolicy::KeepB => {
                let keep_a = match self.options.conflict {
                    ConflictPolicy::KeepA => true,
                    ConflictPolicy::KeepB => false,
                    _ => newer_a,
                };
                let (from, to) = if keep_a {
                    (Side::A, Side::B)
                } else {
                    (Side::B, Side::A)
                };
                plan.conflicts.push(ConflictInfo {
                    path: path.clone(),
                    resolution: format!(
                        "{reason}; kept {} ({})",
                        if keep_a { "A" } else { "B" },
                        match self.options.conflict {
                            ConflictPolicy::KeepNewer => "newer",
                            ConflictPolicy::KeepA => "policy=keep-a",
                            _ => "policy=keep-b",
                        }
                    ),
                    conflict_path: None,
                });
                plan.ops.push(Op::Copy { from, to, path });
            }
            ConflictPolicy::KeepBoth => {
                let keep_a = newer_a;
                let loser = if keep_a { Side::B } else { Side::A };
                let winner = loser.other();
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let conflict_path = path.conflict_name(&loser.to_string().to_lowercase(), secs);
                plan.conflicts.push(ConflictInfo {
                    path: path.clone(),
                    resolution: format!(
                        "{reason}; kept {} as canonical, loser preserved at {}",
                        winner, conflict_path
                    ),
                    conflict_path: Some(conflict_path.clone()),
                });
                // Loser content preserved on both sides FIRST: the
                // canonical copy below overwrites the loser's file, so the
                // conflict copies must be read before that happens.
                for to in [Side::A, Side::B] {
                    plan.ops.push(Op::CopyAs {
                        from: loser,
                        to,
                        src: path.clone(),
                        dst: conflict_path.clone(),
                    });
                }
                // Canonical winner to the losing side.
                plan.ops.push(Op::Copy {
                    from: winner,
                    to: loser,
                    path: path.clone(),
                });
            }
            ConflictPolicy::Skip => {
                plan.conflicts.push(ConflictInfo {
                    path: path.clone(),
                    resolution: format!("{reason}; skipped by policy, both files left alone"),
                    conflict_path: None,
                });
                held_back.insert(path.as_str().to_string());
            }
        }
    }

    // -- bidirectional (stateless two-way) ---------------------------------

    /// Plan a stateless bidirectional sync: no manifest is consulted, so a
    /// file present on only one side is always a new copy (never a
    /// deletion), and a shared file with different content is always a
    /// conflict, even if only one side changed since the last sync.
    pub async fn plan_bidirectional(
        &self,
        a: &dyn StorageBackend,
        b: &dyn StorageBackend,
    ) -> Result<(Plan, Vec<String>, BTreeSet<String>)> {
        let snap_a = self.snapshot(a).await?;
        let snap_b = self.snapshot(b).await?;
        let same_backend = a.name() == b.name();
        let mut plan = Plan::default();
        let mut held_back: BTreeSet<String> = BTreeSet::new();
        let mut keys: BTreeSet<String> = BTreeSet::new();
        keys.extend(snap_a.keys().cloned());
        keys.extend(snap_b.keys().cloned());

        for key in keys {
            match (snap_a.get(&key), snap_b.get(&key)) {
                (Some(ae), Some(be)) => {
                    if same_content(ae, be, same_backend) {
                        continue;
                    }
                    self.resolve_conflict(
                        &mut plan,
                        &mut held_back,
                        ae,
                        be,
                        "differ on both sides",
                    );
                }
                (Some(ae), None) => plan.ops.push(Op::Copy {
                    from: Side::A,
                    to: Side::B,
                    path: ae.path.clone(),
                }),
                (None, Some(be)) => plan.ops.push(Op::Copy {
                    from: Side::B,
                    to: Side::A,
                    path: be.path.clone(),
                }),
                (None, None) => {}
            }
        }
        Ok((plan, Vec::new(), held_back))
    }

    async fn sync_bidirectional(
        &self,
        a: &dyn StorageBackend,
        b: &dyn StorageBackend,
    ) -> Result<Report> {
        let (plan, warnings, held_back) = self.plan_bidirectional(a, b).await?;
        let mut report = Report {
            mode: Some(SyncMode::Bidirectional),
            dry_run: self.options.dry_run,
            planned: plan.ops.clone(),
            conflicts: plan.conflicts.clone(),
            warnings,
            ..Default::default()
        };
        if self.options.dry_run {
            return Ok(report);
        }
        self.apply(a, b, &plan, &mut report).await;
        // Decisions never consulted a manifest, but the merged state is
        // still recorded: without it, a later two-way run would compare
        // against a stale baseline and report conflicts for files this
        // run already reconciled. Held-back (skipped-conflict) paths stay
        // out of the new manifest so the next run re-detects them.
        match self
            .write_two_way_manifests(a, b, &Manifest::new(), &held_back)
            .await
        {
            Ok(()) => {}
            Err(e) => report.errors.push(format!("manifest write failed: {e}")),
        }
        Ok(report)
    }

    async fn sync_two_way(&self, a: &dyn StorageBackend, b: &dyn StorageBackend) -> Result<Report> {
        let (plan, warnings, baseline, held_back) = self.plan_two_way(a, b).await?;
        let mut report = Report {
            mode: Some(SyncMode::TwoWay),
            dry_run: self.options.dry_run,
            planned: plan.ops.clone(),
            conflicts: plan.conflicts.clone(),
            warnings,
            ..Default::default()
        };
        if self.options.dry_run {
            return Ok(report);
        }
        self.apply(a, b, &plan, &mut report).await;
        // Manifests from a fresh listing: what is actually there now.
        // Held-back (skipped-conflict) paths keep their old baseline, so
        // the next run re-detects them instead of blessing the divergence.
        match self
            .write_two_way_manifests(a, b, &baseline, &held_back)
            .await
        {
            Ok(()) => {}
            Err(e) => report.errors.push(format!("manifest write failed: {e}")),
        }
        Ok(report)
    }

    // -- one-way / backup --------------------------------------------------

    pub async fn plan_one_way(
        &self,
        src: &dyn StorageBackend,
        dst: &dyn StorageBackend,
        mode: SyncMode,
    ) -> Result<(Plan, Vec<String>, Manifest, BTreeSet<String>)> {
        let snap_s = self.snapshot(src).await?;
        let snap_d = self.snapshot(dst).await?;
        let (man_d, warnings) = load_manifest(dst).await;
        let same_backend = src.name() == dst.name();
        let mut plan = Plan::default();
        let mut held_back: BTreeSet<String> = BTreeSet::new();

        for (key, se) in &snap_s {
            match snap_d.get(key) {
                None => plan.ops.push(Op::Copy {
                    from: Side::A,
                    to: Side::B,
                    path: se.path.clone(),
                }),
                Some(de) => {
                    let cs = changed_since(se, man_d.get(key));
                    let cd = changed_since(de, man_d.get(key));
                    if !cs && !cd {
                        continue;
                    }
                    if same_content(se, de, same_backend) {
                        continue;
                    }
                    if mode == SyncMode::Backup && cd && mtime_tuple(de) > mtime_tuple(se) {
                        // Backup protects a newer destination file.
                        plan.conflicts.push(ConflictInfo {
                            path: se.path.clone(),
                            resolution: "backup: destination is newer, left alone".to_string(),
                            conflict_path: None,
                        });
                        held_back.insert(key.clone());
                        continue;
                    }
                    plan.ops.push(Op::Copy {
                        from: Side::A,
                        to: Side::B,
                        path: se.path.clone(),
                    });
                }
            }
        }
        // Mirror always makes the destination match the source; OneWay
        // deletes only when delete_extraneous is on (push/pull --no-delete).
        if mode == SyncMode::Mirror || (mode == SyncMode::OneWay && self.options.delete_extraneous)
        {
            for (key, de) in &snap_d {
                if !snap_s.contains_key(key) {
                    plan.ops.push(Op::Delete {
                        side: Side::B,
                        path: de.path.clone(),
                    });
                }
            }
        }
        Ok((plan, warnings, man_d, held_back))
    }

    async fn sync_one_way(
        &self,
        src: &dyn StorageBackend,
        dst: &dyn StorageBackend,
        mode: SyncMode,
    ) -> Result<Report> {
        let (plan, warnings, baseline, held_back) = self.plan_one_way(src, dst, mode).await?;
        let mut report = Report {
            mode: Some(mode),
            dry_run: self.options.dry_run,
            planned: plan.ops.clone(),
            conflicts: plan.conflicts.clone(),
            warnings,
            ..Default::default()
        };
        if self.options.dry_run {
            return Ok(report);
        }
        self.apply(src, dst, &plan, &mut report).await;
        // Only the destination gets a manifest: a backup must not modify
        // its source, and a mirror's source needs no baseline.
        match self
            .write_one_way_manifest(dst, &baseline, &held_back)
            .await
        {
            Ok(()) => {}
            Err(e) => report.errors.push(format!("manifest write failed: {e}")),
        }
        Ok(report)
    }

    // -- shared helpers ----------------------------------------------------

    /// Filtered snapshot: markdown files only, manifest excluded, sorted.
    async fn snapshot(&self, backend: &dyn StorageBackend) -> Result<BTreeMap<String, Entry>> {
        let mut map = BTreeMap::new();
        for e in backend.list().await? {
            if e.path.as_str() == MANIFEST_PATH {
                continue;
            }
            match e.path.extension() {
                Some(ext) if self.options.include_exts.iter().any(|x| x == &ext) => {
                    map.insert(e.path.as_str().to_string(), e);
                }
                _ => continue,
            }
        }
        Ok(map)
    }

    async fn apply(
        &self,
        a: &dyn StorageBackend,
        b: &dyn StorageBackend,
        plan: &Plan,
        report: &mut Report,
    ) {
        for op in &plan.ops {
            let r: Result<()> = async {
                match op {
                    Op::Copy { from, to, path } => {
                        let (src, dst) = side_backends(a, b, *from, *to);
                        let data = src.read(path).await?;
                        // Best-effort: stat only feeds set_modified, so a
                        // backend that cannot stat (restricted key, odd
                        // permissions) must not fail the copy itself.
                        let src_entry = src.stat(path).await.ok().flatten();
                        let written = dst.write(path, &data).await?;
                        let _ = written;
                        if let Some(se) = src_entry {
                            let _ = dst.set_modified(path, se.modified()).await;
                        }
                        report.copied_bytes += data.len() as u64;
                        Ok(())
                    }
                    Op::CopyAs { from, to, src, dst } => {
                        let (s, d) = side_backends(a, b, *from, *to);
                        let data = s.read(src).await?;
                        d.write(dst, &data).await?;
                        report.copied_bytes += data.len() as u64;
                        Ok(())
                    }
                    Op::Delete { side, path } => {
                        let backend = if *side == Side::A { a } else { b };
                        backend.delete(path).await?;
                        report.deleted += 1;
                        Ok(())
                    }
                }
            }
            .await;
            match r {
                Ok(()) => report.ops_applied += 1,
                Err(e) => report.errors.push(format!("{op:?}: {e}")),
            }
        }
    }

    async fn write_two_way_manifests(
        &self,
        a: &dyn StorageBackend,
        b: &dyn StorageBackend,
        baseline: &Manifest,
        held_back: &BTreeSet<String>,
    ) -> Result<()> {
        let snap_a = self.snapshot(a).await?;
        let mut manifest = Manifest::from_entries(&snap_a.values().cloned().collect::<Vec<_>>());
        restore_baseline(&mut manifest, baseline, held_back);
        let data = manifest.to_json().map_err(SyncError::from)?;
        let path = RelPath::new(MANIFEST_PATH).unwrap();
        a.write(&path, &data).await?;
        b.write(&path, &data).await?;
        Ok(())
    }

    async fn write_one_way_manifest(
        &self,
        dst: &dyn StorageBackend,
        baseline: &Manifest,
        held_back: &BTreeSet<String>,
    ) -> Result<()> {
        let snap_d = self.snapshot(dst).await?;
        let mut manifest = Manifest::from_entries(&snap_d.values().cloned().collect::<Vec<_>>());
        restore_baseline(&mut manifest, baseline, held_back);
        let data = manifest.to_json().map_err(SyncError::from)?;
        let path = RelPath::new(MANIFEST_PATH).unwrap();
        dst.write(&path, &data).await?;
        Ok(())
    }
}

/// A corrupt or missing manifest is an empty baseline plus a warning —
/// never a failed sync. Losing the manifest only costs precision: the
/// next run treats every differing file as changed on both sides, and the
/// conflict policy decides.
async fn load_manifest(backend: &dyn StorageBackend) -> (Manifest, Vec<String>) {
    match backend.read(&RelPath::new(MANIFEST_PATH).unwrap()).await {
        Ok(data) => match Manifest::parse(&data) {
            Ok(m) => (m, Vec::new()),
            Err(e) => (
                Manifest::new(),
                vec![format!(
                    "corrupt manifest on {} ({}); treating as empty",
                    backend.describe(),
                    e
                )],
            ),
        },
        Err(SyncError::NotFound(_)) => (Manifest::new(), Vec::new()),
        Err(e) => (
            Manifest::new(),
            vec![format!(
                "could not read manifest on {} ({}); treating as empty",
                backend.describe(),
                e
            )],
        ),
    }
}

/// Has this entry changed since the baseline state recorded at last sync?
/// `None` baseline = never synced = changed.
fn changed_since(entry: &Entry, baseline: Option<&FileState>) -> bool {
    let Some(base) = baseline else {
        return true;
    };
    if let (Some(h), Some(bh)) = (&entry.hash, &base.hash) {
        if h != bh {
            return true;
        }
    } else if let (Some(e), Some(be)) = (&entry.etag, &base.etag) {
        if e != be {
            return true;
        }
    }
    if entry.size != base.size {
        return true;
    }
    // mtime alone is weak evidence (a copy updates it), so require > 2s.
    (entry.mtime_secs - base.mtime_secs).abs() > 2
}

/// Do two entries hold the same content? Hash wins when both sides have
/// one; same-backend etag next; otherwise size + mtime within a second.
fn same_content(a: &Entry, b: &Entry, same_backend: bool) -> bool {
    if let (Some(ha), Some(hb)) = (&a.hash, &b.hash) {
        return ha == hb;
    }
    if same_backend {
        if let (Some(ea), Some(eb)) = (&a.etag, &b.etag) {
            return ea == eb;
        }
    }
    if a.size != b.size {
        return false;
    }
    (a.mtime_secs - b.mtime_secs).abs() <= 1
}

fn mtime_tuple(e: &Entry) -> (i64, u32) {
    (e.mtime_secs, e.mtime_nanos)
}

fn side_backends<'x>(
    a: &'x dyn StorageBackend,
    b: &'x dyn StorageBackend,
    from: Side,
    to: Side,
) -> (&'x dyn StorageBackend, &'x dyn StorageBackend) {
    // `from == to` is legitimate for CopyAs: a conflict copy is written
    // to both sides, including the side the loser content already lives on.
    let src = match from {
        Side::A => a,
        Side::B => b,
    };
    let dst = match to {
        Side::A => a,
        Side::B => b,
    };
    (src, dst)
}

/// Held-back paths keep their old baseline entry (or stay absent), so the
/// next run re-detects the divergence instead of blessing it.
fn restore_baseline(manifest: &mut Manifest, baseline: &Manifest, held_back: &BTreeSet<String>) {
    for key in held_back {
        match baseline.get(key) {
            Some(state) => {
                manifest.files.insert(key.clone(), state.clone());
            }
            None => {
                manifest.files.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemBackend;
    use std::sync::Arc;

    fn entry(backend: &MemBackend, path: &str, data: &[u8]) -> Entry {
        // Helper for tests: write then stat, so hash/mtime are real.
        let rt = tokio_test_runtime();
        rt.block_on(async {
            backend
                .write(&RelPath::new(path).unwrap(), data)
                .await
                .unwrap()
        });
        rt.block_on(async {
            backend
                .stat(&RelPath::new(path).unwrap())
                .await
                .unwrap()
                .unwrap()
        })
    }

    // The core crate has no tokio dependency, so tests spin a tiny
    // single-thread runtime on demand via a minimal executor shim:
    // we reuse std threads + a oneshot instead — simpler: tests below are
    // async and run under `tokio` only where the dev-dependency exists.
    fn tokio_test_runtime() -> TokioRt {
        TokioRt
    }

    struct TokioRt;
    impl TokioRt {
        fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
            // Minimal single-threaded executor for test helpers.
            use std::sync::Arc as StdArc;
            use std::task::{Context, Poll, Wake};
            struct NoopWake;
            impl Wake for NoopWake {
                fn wake(self: StdArc<Self>) {}
            }
            let waker = std::task::Waker::from(StdArc::new(NoopWake));
            let mut cx = Context::from_waker(&waker);
            let mut f = Box::pin(f);
            loop {
                match f.as_mut().poll(&mut cx) {
                    Poll::Ready(v) => return v,
                    Poll::Pending => std::thread::yield_now(),
                }
            }
        }
    }

    #[test]
    fn rel_path_rejects_escape() {
        assert!(RelPath::new("../x.md").is_err());
        assert!(RelPath::new("/abs.md").is_err());
        assert!(RelPath::new("a//b.md").is_err());
        assert!(RelPath::new("a\\b.md").is_err());
        assert_eq!(RelPath::new("a/b.md").unwrap().as_str(), "a/b.md");
        assert_eq!(
            RelPath::new("a/b.md")
                .unwrap()
                .conflict_name("b", 42)
                .as_str(),
            "a/b.conflict-b-42.md"
        );
    }

    #[test]
    fn manifest_roundtrip() {
        let mut m = Manifest::new();
        let e = Entry::new(RelPath::new("a.md").unwrap(), 3, SystemTime::now());
        m.insert(&e);
        let json = m.to_json().unwrap();
        assert_eq!(Manifest::parse(&json).unwrap(), m);
    }

    #[test]
    fn engine_two_way_copies_new_file() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let _ = entry(&a, "hello.md", b"# hi");
        let engine = Engine::default();
        let rt = TokioRt;
        let report = rt.block_on(engine.sync(a.as_ref(), b.as_ref(), SyncMode::TwoWay));
        let report = report.unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert_eq!(report.ops_applied, 1);
        let got = rt
            .block_on(b.read(&RelPath::new("hello.md").unwrap()))
            .unwrap();
        assert_eq!(got, b"# hi");
    }

    #[test]
    fn engine_two_way_conflict_keep_newer() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        // First sync establishes the baseline.
        let _ = entry(&a, "note.md", b"v1");
        rt.block_on(engine_sync(&a, &b)).unwrap();
        // Both sides change independently.
        let _ = entry(&a, "note.md", b"v2-a");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = entry(&b, "note.md", b"v2-b-longer");
        let report = rt.block_on(engine_sync(&a, &b));
        let report = report.unwrap();
        assert_eq!(report.conflicts.len(), 1, "{report:?}");
        // B is newer, so B wins and flows to A.
        let got = rt
            .block_on(a.read(&RelPath::new("note.md").unwrap()))
            .unwrap();
        assert_eq!(got, b"v2-b-longer");
    }

    async fn engine_sync(a: &Arc<MemBackend>, b: &Arc<MemBackend>) -> Result<Report> {
        Engine::default()
            .sync(a.as_ref(), b.as_ref(), SyncMode::TwoWay)
            .await
    }

    #[test]
    fn engine_one_way_deletes_extraneous() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        let _ = entry(&a, "keep.md", b"k");
        let _ = entry(&b, "stale.md", b"s");
        let report = rt
            .block_on(Engine::default().sync(a.as_ref(), b.as_ref(), SyncMode::OneWay))
            .unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(rt
            .block_on(b.stat(&RelPath::new("stale.md").unwrap()))
            .unwrap()
            .is_none());
        assert!(rt
            .block_on(b.stat(&RelPath::new("keep.md").unwrap()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn engine_backup_never_deletes_and_protects_newer() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        let _ = entry(&a, "note.md", b"old");
        let _ = entry(&b, "only-in-backup.md", b"keep me");
        // Establish a backup baseline, then make the backup newer.
        rt.block_on(Engine::default().sync(a.as_ref(), b.as_ref(), SyncMode::Backup))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = entry(&b, "note.md", b"newer backup edit");
        let report = rt
            .block_on(Engine::default().sync(a.as_ref(), b.as_ref(), SyncMode::Backup))
            .unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        // Stale file still there, newer backup file untouched.
        assert!(rt
            .block_on(b.stat(&RelPath::new("only-in-backup.md").unwrap()))
            .unwrap()
            .is_some());
        let got = rt
            .block_on(b.read(&RelPath::new("note.md").unwrap()))
            .unwrap();
        assert_eq!(got, b"newer backup edit");
    }

    #[test]
    fn engine_bidirectional_copies_unique_both_ways() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        let _ = entry(&a, "only-a.md", b"a");
        let _ = entry(&b, "only-b.md", b"b");
        let report = rt
            .block_on(Engine::default().sync(a.as_ref(), b.as_ref(), SyncMode::Bidirectional))
            .unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.ops_applied, 2);
        assert!(rt
            .block_on(b.stat(&RelPath::new("only-a.md").unwrap()))
            .unwrap()
            .is_some());
        assert!(rt
            .block_on(a.stat(&RelPath::new("only-b.md").unwrap()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn engine_bidirectional_never_deletes() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        // Establish a baseline with two-way, then delete on B only.
        let _ = entry(&a, "note.md", b"v1");
        rt.block_on(engine_sync(&a, &b)).unwrap();
        rt.block_on(b.delete(&RelPath::new("note.md").unwrap()))
            .unwrap();
        let report = rt
            .block_on(Engine::default().sync(a.as_ref(), b.as_ref(), SyncMode::Bidirectional))
            .unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        // Stateless: a one-sided file is a new copy, never a deletion.
        assert_eq!(report.deleted, 0);
        assert!(rt
            .block_on(b.stat(&RelPath::new("note.md").unwrap()))
            .unwrap()
            .is_some());
        assert!(rt
            .block_on(a.stat(&RelPath::new("note.md").unwrap()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn engine_bidirectional_one_side_change_is_conflict() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        let _ = entry(&a, "note.md", b"v1");
        rt.block_on(engine_sync(&a, &b)).unwrap();
        // Only A changes: two-way would copy it silently, but the
        // stateless mode has no baseline to tell that from a both-change.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = entry(&a, "note.md", b"v2-a");
        let report = rt
            .block_on(Engine::default().sync(a.as_ref(), b.as_ref(), SyncMode::Bidirectional))
            .unwrap();
        assert_eq!(report.conflicts.len(), 1, "{report:?}");
        assert!(
            report.conflicts[0]
                .resolution
                .contains("differ on both sides"),
            "{report:?}"
        );
        // Default policy keeps the newer (A) version, which flows to B.
        let got = rt
            .block_on(b.read(&RelPath::new("note.md").unwrap()))
            .unwrap();
        assert_eq!(got, b"v2-a");
    }

    #[test]
    fn engine_mirror_always_deletes() {
        let a = Arc::new(MemBackend::new());
        let b = Arc::new(MemBackend::new());
        let rt = TokioRt;
        let _ = entry(&a, "keep.md", b"k");
        let _ = entry(&b, "stale.md", b"s");
        // Mirror ignores delete_extraneous: destination must match source.
        let no_delete = Engine::new(SyncOptions {
            delete_extraneous: false,
            ..Default::default()
        });
        let report = rt
            .block_on(no_delete.sync(a.as_ref(), b.as_ref(), SyncMode::Mirror))
            .unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(rt
            .block_on(b.stat(&RelPath::new("stale.md").unwrap()))
            .unwrap()
            .is_none());
        // OneWay with the same options keeps the extra file.
        let _ = entry(&b, "stale.md", b"s");
        let report = rt
            .block_on(no_delete.sync(a.as_ref(), b.as_ref(), SyncMode::OneWay))
            .unwrap();
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(rt
            .block_on(b.stat(&RelPath::new("stale.md").unwrap()))
            .unwrap()
            .is_some());
    }
}
