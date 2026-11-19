//! Live conditional-write test against the user's RustFS S3 service.
//! Opt-in: only runs when `MD_SYNC_LIVE_S3=1`.
//! Uses ONLY the `md-sync-test/` prefix in bucket `museai` and cleans up.
//! Credentials come from `~/workspace/rustfs-s3-config.json` via
//! `demo/s3tool.py env` (never printed).
//!
//! Run: `MD_SYNC_LIVE_S3=1 cargo test --offline -p md-sync-s3 --test live_conditional`

use md_sync_core::backend::Builder;
use md_sync_core::{Precondition, RelPath, SyncError, WriteOptions};
use md_sync_s3::S3Builder;
use std::process::Command;

fn live_config() -> Option<(String, String, String, String)> {
    if std::env::var("MD_SYNC_LIVE_S3").ok().as_deref() != Some("1") {
        return None;
    }
    let out = Command::new("python3")
        .args(["demo/s3tool.py", "env"])
        .current_dir(env!("CARGO_MANIFEST_DIR").to_string() + "/../..")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("export {k}=")))
            .map(|l| {
                l.trim_start_matches(&format!("export {k}="))
                    .trim_matches('\'')
                    .to_string()
            })
    };
    Some((
        get("AWS_ACCESS_KEY_ID")?,
        get("AWS_SECRET_ACCESS_KEY")?,
        get("AWS_REGION").unwrap_or_else(|| "us-east-1".into()),
        get("AWS_ENDPOINT_URL")?,
    ))
}

#[tokio::test]
async fn conditional_writes_against_live_s3() {
    let Some((key, secret, region, endpoint)) = live_config() else {
        println!("skipped: set MD_SYNC_LIVE_S3=1 to run against live S3");
        return;
    };
    let backend = S3Builder::new()
        .bucket("museai")
        .region(region)
        .endpoint(endpoint)
        .access_key(key)
        .secret_key(secret)
        .prefix("md-sync-test/live-cas/")
        .build()
        .unwrap();
    let p = RelPath::new("cas.md").unwrap();
    let _ = backend.delete(&p).await;

    // IfNoneMatch creates, then refuses a second create.
    let e1 = backend
        .write_with_options(
            &p,
            b"v1",
            WriteOptions::default().with_precondition(Precondition::IfNoneMatch),
        )
        .await
        .unwrap();
    let etag = e1.etag.clone().expect("PUT returns an etag");
    let err = backend
        .write_with_options(
            &p,
            b"v1-again",
            WriteOptions::default().with_precondition(Precondition::IfNoneMatch),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, SyncError::PreconditionFailed(_)),
        "second IfNoneMatch create must fail, got {err:?}"
    );

    // IfMatch with the fresh etag succeeds; with a stale one it fails.
    let e2 = backend
        .write_with_options(
            &p,
            b"v2",
            WriteOptions::default().with_precondition(Precondition::IfMatch(etag.clone())),
        )
        .await
        .unwrap();
    assert_ne!(e2.etag.as_deref(), Some(etag.as_str()));
    let err = backend
        .write_with_options(
            &p,
            b"v3-stale",
            WriteOptions::default().with_precondition(Precondition::IfMatch(etag)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, SyncError::PreconditionFailed(_)),
        "stale IfMatch must fail, got {err:?}"
    );

    // read_if_match + read_range against the live object.
    let current = backend.stat(&p).await.unwrap().unwrap();
    let cur_etag = current.etag.clone().unwrap();
    assert_eq!(backend.read_if_match(&p, &cur_etag).await.unwrap(), b"v2");
    let err = backend.read_if_match(&p, "deadbeef").await.unwrap_err();
    assert!(
        matches!(err, SyncError::PreconditionFailed(_)),
        "stale read_if_match must fail, got {err:?}"
    );
    assert_eq!(backend.read_range(&p, 0, 1).await.unwrap(), b"v");
    // content-type override is stored and reported back.
    let e3 = backend
        .write_with_options(
            &p,
            b"{}",
            WriteOptions::default()
                .with_content_type("application/x-opendal-sync-tombstone")
                .with_precondition(Precondition::IfMatch(cur_etag)),
        )
        .await
        .unwrap();
    assert_eq!(
        e3.content_type.as_deref(),
        Some("application/x-opendal-sync-tombstone")
    );
    let stat = backend.stat(&p).await.unwrap().unwrap();
    assert_eq!(
        stat.content_type.as_deref(),
        Some("application/x-opendal-sync-tombstone"),
        "stat must report the stored content-type"
    );

    backend.delete(&p).await.unwrap();
}
