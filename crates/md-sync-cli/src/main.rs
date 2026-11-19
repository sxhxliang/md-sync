//! Example CLI for md-sync — and the verification harness for the library.
//!
//! Backend specs (credentials always come from the environment, never from
//! argv, so they don't leak into shell history):
//!
//! ```text
//! fs:/home/u/notes            local directory (a bare path works too)
//! s3://bucket/prefix          AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
//!                             AWS_REGION (default us-east-1),
//!                             AWS_ENDPOINT_URL (MinIO etc.), AWS_SESSION_TOKEN
//! webdav:https://host/path    WEBDAV_USERNAME, WEBDAV_PASSWORD
//!                             (or WEBDAV_TOKEN for bearer auth)
//! gdrive:<folder-id>          GDRIVE_ACCESS_TOKEN (folder id, or "root")
//! onedrive:                   ONEDRIVE_ACCESS_TOKEN
//!                             (onedrive:<drive-id> for a specific drive)
//! ```

use clap::{Parser, Subcommand};
use md_sync::{Builder, ConflictPolicy, Engine, Op, Report, StorageBackend, SyncMode, SyncOptions};
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "md-sync",
    version,
    about = "Sync and back up markdown documents"
)]
struct Cli {
    /// Print the report as JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,

    /// Show every planned/applied operation.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Two-way sync: changes flow both ways, deletions propagate.
    Sync {
        a: String,
        b: String,
        /// Mode: two-way (default), bidirectional, mirror, one-way, backup.
        #[arg(long, default_value = "two-way")]
        mode: String,
        /// Conflict policy: newer (default), keep-a, keep-b, both, skip.
        #[arg(long)]
        conflict: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Extra extensions to sync, e.g. `--ext mdx` (default: md, markdown).
        #[arg(long)]
        ext: Vec<String>,
    },
    /// One-way mirror: `from` wins, extra files in `to` are deleted.
    Push {
        from: String,
        to: String,
        /// Keep files in `to` that are missing from `from`.
        #[arg(long)]
        no_delete: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        ext: Vec<String>,
    },
    /// One-way mirror in the other direction: `from` wins.
    Pull {
        from: String,
        to: String,
        #[arg(long)]
        no_delete: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        ext: Vec<String>,
    },
    /// Copy-only backup: never deletes, never overwrites a newer file in `to`.
    Backup {
        from: String,
        to: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        ext: Vec<String>,
    },
    /// Show what a sync would do, without changing anything.
    Plan {
        a: String,
        b: String,
        /// two-way (default), bidirectional, mirror, one-way, backup.
        #[arg(long, default_value = "two-way")]
        mode: String,
        #[arg(long)]
        conflict: Option<String>,
        #[arg(long)]
        ext: Vec<String>,
    },
    /// List the markdown files in a backend.
    List { backend: String },
}

#[cfg(any(
    feature = "s3",
    feature = "webdav",
    feature = "gdrive",
    feature = "onedrive"
))]
fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))
}

#[cfg(any(
    feature = "s3",
    feature = "webdav",
    feature = "gdrive",
    feature = "onedrive"
))]
fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Build a backend from a spec string. Feature-gated: a backend that was
/// not compiled in reports exactly that, instead of failing mysteriously.
fn parse_backend(spec: &str) -> Result<Arc<dyn StorageBackend>, String> {
    #[cfg(feature = "fs")]
    {
        if let Some(path) = spec.strip_prefix("fs:") {
            return md_sync::services::Fs::default()
                .root(path)
                .build()
                .map_err(|e| e.to_string());
        }
    }
    #[cfg(feature = "s3")]
    {
        if let Some(rest) = spec.strip_prefix("s3://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((b, p)) => (b, p),
                None => (rest, ""),
            };
            let mut b = md_sync::services::S3::default()
                .bucket(bucket)
                .region(
                    env_opt("AWS_REGION")
                        .or_else(|| env_opt("AWS_DEFAULT_REGION"))
                        .unwrap_or_else(|| "us-east-1".to_string()),
                )
                .access_key(env("AWS_ACCESS_KEY_ID")?)
                .secret_key(env("AWS_SECRET_ACCESS_KEY")?)
                .prefix(prefix);
            if let Some(ep) = env_opt("AWS_ENDPOINT_URL").or_else(|| env_opt("S3_ENDPOINT")) {
                b = b.endpoint(ep);
            }
            if let Some(t) = env_opt("AWS_SESSION_TOKEN") {
                b = b.session_token(t);
            }
            return b.build().map_err(|e| e.to_string());
        }
    }
    #[cfg(feature = "webdav")]
    {
        if let Some(url) = spec.strip_prefix("webdav:") {
            let mut b = md_sync::services::WebDav::default().base_url(url);
            if let Some(t) = env_opt("WEBDAV_TOKEN") {
                b = b.bearer_token(t);
            } else {
                b = b
                    .username(env_opt("WEBDAV_USERNAME").unwrap_or_default())
                    .password(env_opt("WEBDAV_PASSWORD").unwrap_or_default());
            }
            return b.build().map_err(|e| e.to_string());
        }
    }
    #[cfg(feature = "gdrive")]
    {
        if let Some(folder) = spec.strip_prefix("gdrive:") {
            let folder = if folder.is_empty() { "root" } else { folder };
            return md_sync::services::GoogleDrive::default()
                .access_token(env("GDRIVE_ACCESS_TOKEN").or_else(|_| env("GOOGLE_ACCESS_TOKEN"))?)
                .root_folder_id(folder)
                .build()
                .map_err(|e| e.to_string());
        }
    }
    #[cfg(feature = "onedrive")]
    {
        if let Some(drive) = spec.strip_prefix("onedrive:") {
            let mut b =
                md_sync::services::OneDrive::default().access_token(env("ONEDRIVE_ACCESS_TOKEN")?);
            if !drive.is_empty() {
                b = b.drive_id(drive);
            }
            return b.build().map_err(|e| e.to_string());
        }
    }
    #[cfg(feature = "fs")]
    {
        if !spec.contains("://") {
            // A bare path is a local directory.
            return md_sync::services::Fs::default()
                .root(spec)
                .build()
                .map_err(|e| e.to_string());
        }
    }
    Err(format!(
        "unknown or not-enabled backend spec: {spec} (rebuild with the matching feature: fs, s3, webdav, gdrive, onedrive)"
    ))
}

fn parse_mode(s: &str) -> Result<SyncMode, String> {
    match s {
        "two-way" => Ok(SyncMode::TwoWay),
        "bidirectional" => Ok(SyncMode::Bidirectional),
        "one-way" => Ok(SyncMode::OneWay),
        "mirror" => Ok(SyncMode::Mirror),
        "backup" => Ok(SyncMode::Backup),
        other => Err(format!(
            "unknown mode: {other} (two-way|bidirectional|one-way|mirror|backup)"
        )),
    }
}

fn parse_conflict(s: Option<&str>) -> Result<ConflictPolicy, String> {
    match s.unwrap_or("newer") {
        "newer" => Ok(ConflictPolicy::KeepNewer),
        "keep-a" => Ok(ConflictPolicy::KeepA),
        "keep-b" => Ok(ConflictPolicy::KeepB),
        "both" => Ok(ConflictPolicy::KeepBoth),
        "skip" => Ok(ConflictPolicy::Skip),
        other => Err(format!(
            "unknown conflict policy: {other} (newer|keep-a|keep-b|both|skip)"
        )),
    }
}

fn options(ext: &[String], dry_run: bool, delete: bool, conflict: ConflictPolicy) -> SyncOptions {
    let mut o = SyncOptions {
        dry_run,
        delete_extraneous: delete,
        conflict,
        ..Default::default()
    };
    if !ext.is_empty() {
        o.include_exts = ext
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect();
    }
    o
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / 1024.0 / 1024.0)
    }
}

fn print_report(cli: &Cli, a_desc: &str, b_desc: &str, report: &Report) -> ExitCode {
    if cli.json {
        println!("{}", serde_json::to_string_pretty(report).unwrap());
    } else {
        let mode = match report.mode {
            Some(SyncMode::TwoWay) => "two-way",
            Some(SyncMode::Bidirectional) => "bidirectional",
            Some(SyncMode::OneWay) => "one-way",
            Some(SyncMode::Mirror) => "mirror",
            Some(SyncMode::Backup) => "backup",
            None => "?",
        };
        println!("{mode}: {a_desc} <-> {b_desc}");
        if report.dry_run {
            println!("(dry run — nothing was changed)");
        }
        if cli.verbose {
            for op in &report.planned {
                match op {
                    Op::Copy { from, to, path } => {
                        println!("  {from} -> {to}  {path}");
                    }
                    Op::CopyAs { from, to, src, dst } => {
                        println!("  {from} -> {to}  {src} as {dst}");
                    }
                    Op::Delete { side, path } => {
                        println!("  delete on {side}  {path}");
                    }
                }
            }
        }
        println!(
            "applied {} ops, copied {}, deleted {}, conflicts {}, errors {}",
            report.ops_applied,
            human_bytes(report.copied_bytes),
            report.deleted,
            report.conflicts.len(),
            report.errors.len()
        );
        for c in &report.conflicts {
            println!("  conflict {}: {}", c.path, c.resolution);
        }
        for s in &report.skipped {
            println!("  skipped: {s}");
        }
        for w in &report.warnings {
            println!("  warning: {w}");
        }
        for e in &report.errors {
            println!("  error: {e}");
        }
    }
    if !report.is_ok() {
        ExitCode::from(1)
    } else if !report.conflicts.is_empty()
        && report
            .conflicts
            .iter()
            .any(|c| c.resolution.contains("skipped"))
    {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    match &cli.cmd {
        Cmd::Sync {
            a,
            b,
            mode,
            conflict,
            dry_run,
            ext,
        } => {
            let ba = parse_backend(a)?;
            let bb = parse_backend(b)?;
            let engine = Engine::new(options(
                ext,
                *dry_run,
                true,
                parse_conflict(conflict.as_deref())?,
            ));
            let report = engine
                .sync(ba.as_ref(), bb.as_ref(), parse_mode(mode)?)
                .await
                .map_err(|e| e.to_string())?;
            Ok(print_report(&cli, &ba.describe(), &bb.describe(), &report))
        }
        Cmd::Push {
            from,
            to,
            no_delete,
            dry_run,
            ext,
        } => {
            let bs = parse_backend(from)?;
            let bd = parse_backend(to)?;
            let engine = Engine::new(options(
                ext,
                *dry_run,
                !no_delete,
                ConflictPolicy::KeepNewer,
            ));
            let report = engine
                .sync(bs.as_ref(), bd.as_ref(), SyncMode::OneWay)
                .await
                .map_err(|e| e.to_string())?;
            Ok(print_report(&cli, &bs.describe(), &bd.describe(), &report))
        }
        Cmd::Pull {
            from,
            to,
            no_delete,
            dry_run,
            ext,
        } => {
            let bs = parse_backend(from)?;
            let bd = parse_backend(to)?;
            let engine = Engine::new(options(
                ext,
                *dry_run,
                !no_delete,
                ConflictPolicy::KeepNewer,
            ));
            let report = engine
                .sync(bs.as_ref(), bd.as_ref(), SyncMode::OneWay)
                .await
                .map_err(|e| e.to_string())?;
            Ok(print_report(&cli, &bs.describe(), &bd.describe(), &report))
        }
        Cmd::Backup {
            from,
            to,
            dry_run,
            ext,
        } => {
            let bs = parse_backend(from)?;
            let bd = parse_backend(to)?;
            let engine = Engine::new(options(ext, *dry_run, false, ConflictPolicy::KeepNewer));
            let report = engine
                .sync(bs.as_ref(), bd.as_ref(), SyncMode::Backup)
                .await
                .map_err(|e| e.to_string())?;
            Ok(print_report(&cli, &bs.describe(), &bd.describe(), &report))
        }
        Cmd::Plan {
            a,
            b,
            mode,
            conflict,
            ext,
        } => {
            let ba = parse_backend(a)?;
            let bb = parse_backend(b)?;
            let m = parse_mode(mode)?;
            let engine = Engine::new(options(
                ext,
                true,
                true,
                parse_conflict(conflict.as_deref())?,
            ));
            let report = engine
                .sync(ba.as_ref(), bb.as_ref(), m)
                .await
                .map_err(|e| e.to_string())?;
            Ok(print_report(&cli, &ba.describe(), &bb.describe(), &report))
        }
        Cmd::List { backend } => {
            let b = parse_backend(backend)?;
            let entries = b.list().await.map_err(|e| e.to_string())?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&entries).unwrap());
            } else {
                println!("{} ({} files)", b.describe(), entries.len());
                for e in entries {
                    println!(
                        "  {:>8}  {}  {}",
                        human_bytes(e.size),
                        e.path,
                        e.hash.as_deref().map(|h| &h[..8]).unwrap_or("--------")
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
