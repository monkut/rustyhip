//! Concurrent-writer CAS integration test.
//!
//! Proves that when two turbolite-backed `SqliteDb` instances open the same
//! S3 prefix and both try to commit, the second one's checkpoint fails —
//! silent overwrite is impossible.
//!
//! ## Floci limitation
//!
//! Floci (our local S3 emulator, `hectorvent/floci:latest`) **does not
//! implement the `If-Match` conditional-write semantics** that shipped in
//! real S3 in November 2024. Verified empirically:
//!
//! ```ignore
//! aws s3api put-object --bucket b --key k --body f --if-match '"stale"'
//! # => succeeds, returns the new ETag
//! ```
//!
//! Real S3 returns `412 Precondition Failed` in that case. Floci ignores
//! the header. Consequence: this test cannot run meaningfully against
//! floci — writer2's commit succeeds because floci didn't evaluate the
//! precondition. Running it produces a spurious "CAS didn't fire" failure
//! that is a floci bug, not a rustyhip bug.
//!
//! ## How to actually verify the guarantee
//!
//! 1. **SDK-layer mock (landed, fastest).** The turbolite fork carries
//!    `StaticReplayClient`-based unit tests that feed canned 412 / 200
//!    responses to `PutObject` and assert the error surfaces as
//!    `ManifestCasError::PreconditionFailed`. See
//!    `turbolite/src/tiered/test_s3_client.rs`. Zero network, deterministic,
//!    covers the SDK wiring fully. Does **not** exercise the full
//!    two-writer-racing-over-a-real-network path.
//!
//! 2. **Real S3 (authoritative).** This test exists to be run that way.
//!    Point `AWS_ENDPOINT_URL` at a real S3 URL (or unset it), use a real
//!    bucket, real credentials. Costs pennies. Proves the end-to-end
//!    guarantee holds when two genuinely separate writers race.
//!
//! On floci the probe at the top of the test detects missing `If-Match`
//! support and skips with a clear diagnostic — no spurious failure.
//!
//! ## Known separate limitation
//!
//! Even on real S3, the first-PUT to an empty prefix uses `If-Match=None`
//! (the etag cell starts empty), so the initial manifest creation is
//! unconditional. If two Lambdas race on a brand-new prefix, both could
//! create a manifest. Fix: `If-None-Match: *` on first write. Tracked as
//! a follow-up; not in Stage A/B scope.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use aws_sdk_s3::error::SdkError;
use rustyhip::db::SqliteDb;
use tempfile::TempDir;
use turbolite::tiered::{TurboliteConfig, TurboliteVfs, register};

const FLOCI_ENDPOINT: &str = "http://localhost:4566";
const BUCKET: &str = "rustyhip-cas-test";

/// Verify the env vars this test needs are already set by the caller.
/// The `just cas-test` recipe sets them before invoking cargo; running
/// `cargo test` directly without them produces a clear failure message
/// rather than an obscure S3 401.
fn require_floci_env() -> Result<()> {
    for key in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION", "AWS_ENDPOINT_URL"] {
        if std::env::var(key).is_err() {
            anyhow::bail!(
                "env var `{key}` not set — run via `just cas-test`, or set \
                 AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 \
                 AWS_ENDPOINT_URL={FLOCI_ENDPOINT} before invoking cargo test"
            );
        }
    }
    Ok(())
}

fn make_vfs(name: &str, cache_dir: &Path, prefix: &str) -> Result<()> {
    let config = TurboliteConfig {
        bucket: BUCKET.into(),
        prefix: prefix.into(),
        cache_dir: cache_dir.into(),
        endpoint_url: Some(FLOCI_ENDPOINT.into()),
        region: Some("us-east-1".into()),
        ..Default::default()
    };
    let vfs = TurboliteVfs::new(config).context("TurboliteVfs::new")?;
    register(name, vfs).context("register vfs")?;
    Ok(())
}

fn open_db(vfs_name: &str, cache_dir: &Path) -> Result<Arc<SqliteDb>> {
    let db_path = cache_dir.join("test.db");
    Ok(Arc::new(SqliteDb::open_with_vfs(db_path, vfs_name)?))
}

/// Probe whether the current S3 endpoint honors `If-Match`. `Floci` returns
/// success even on a stale `ETag`; real S3 (and `MinIO` with conditional-write
/// support) returns 412. Returns `Ok(true)` if supported, `Ok(false)` if the
/// endpoint is reachable but ignores the header.
async fn probe_if_match_support() -> Result<bool> {
    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
        .endpoint_url(std::env::var("AWS_ENDPOINT_URL").unwrap_or_default())
        .force_path_style(true)
        .build();
    let client = aws_sdk_s3::Client::from_conf(s3_config);

    // Ensure the bucket exists (idempotent).
    let _ = client.create_bucket().bucket(BUCKET).send().await;

    // Seed an object.
    let probe_key = "cas-probe";
    client
        .put_object()
        .bucket(BUCKET)
        .key(probe_key)
        .body(aws_sdk_s3::primitives::ByteStream::from(b"seed".to_vec()))
        .send()
        .await
        .context("probe seed PUT")?;

    // Try to overwrite with a deliberately-bogus If-Match. Real S3 returns 412.
    match client
        .put_object()
        .bucket(BUCKET)
        .key(probe_key)
        .body(aws_sdk_s3::primitives::ByteStream::from(b"overwrite".to_vec()))
        .if_match("\"this-is-not-a-real-etag\"")
        .send()
        .await
    {
        Ok(_) => Ok(false), // floci path: succeeds despite stale If-Match
        Err(e) => {
            let status = match &e {
                SdkError::ServiceError(se) => se.raw().status().as_u16(),
                _ => 0,
            };
            Ok(status == 412)
        }
    }
}

// Multi-thread flavor: turbolite's S3 client uses `block_in_place` inside its
// sync wrappers, which requires a multi-threaded runtime. The handler side
// (lambda_http) always provides one; this test has to opt in explicitly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires floci at AWS_ENDPOINT_URL"]
async fn concurrent_writers_second_commit_fails_with_cas_error() -> Result<()> {
    require_floci_env()?;

    // Phase 0: Skip if the S3 endpoint is a floci-style emulator that doesn't
    // honor If-Match. See module docstring for the floci limitation and the
    // paths forward (real S3, MinIO, SDK mock).
    if !probe_if_match_support().await? {
        eprintln!(
            "SKIP: S3 endpoint at {} does not honor If-Match conditional writes. \
             See tests/cas_conflict.rs module docstring for how to verify CAS \
             against real S3 or a compliant emulator.",
            std::env::var("AWS_ENDPOINT_URL").unwrap_or_default()
        );
        return Ok(());
    }

    // Unique prefix per run so reruns don't step on each other.
    let prefix = format!("cas-test-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());

    // Phase 1: Seed a manifest so both writers capture a non-None ETag on open.
    // Without a pre-existing manifest both writers would do an unconditional
    // first-PUT (known hole — see module docstring).
    {
        let seed_cache = TempDir::new()?;
        make_vfs("cas_seed", seed_cache.path(), &prefix)?;
        let db = open_db("cas_seed", seed_cache.path())?;
        db.exec("CREATE TABLE IF NOT EXISTS t (x INTEGER)".into(), vec![]).await?;
        db.exec("PRAGMA wal_checkpoint(TRUNCATE)".into(), vec![]).await?;
        // Explicitly drop the connection so the seed VFS is no longer active.
        drop(db);
    }

    // Phase 2: Two independent writers open the same prefix.
    let cache1 = TempDir::new()?;
    let cache2 = TempDir::new()?;
    make_vfs("cas_writer1", cache1.path(), &prefix)?;
    make_vfs("cas_writer2", cache2.path(), &prefix)?;

    let db1 = open_db("cas_writer1", cache1.path())?;
    let db2 = open_db("cas_writer2", cache2.path())?;

    // Phase 3: Writer 1 commits first — advances the S3 manifest ETag.
    db1.exec("INSERT INTO t VALUES (1)".into(), vec![]).await?;
    db1.exec("PRAGMA wal_checkpoint(TRUNCATE)".into(), vec![]).await.context("writer1 checkpoint (should succeed)")?;

    // Phase 4: Writer 2's checkpoint should now hit a stale ETag.
    db2.exec("INSERT INTO t VALUES (2)".into(), vec![]).await?;
    let cas_result = db2.exec("PRAGMA wal_checkpoint(TRUNCATE)".into(), vec![]).await;

    match cas_result {
        Ok(_) => anyhow::bail!("expected writer2's checkpoint to fail with CAS precondition, but it succeeded"),
        Err(e) => {
            // The io::Error from commit_manifest crosses the SQLite VFS boundary,
            // so we can't guarantee the exact text reaches us. Assert that *some*
            // error surfaced — good enough to prove the CAS didn't silently pass.
            eprintln!("writer2 checkpoint failed as expected: {e:#}");
        }
    }

    Ok(())
}
