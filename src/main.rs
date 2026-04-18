//! Lambda bootstrap.
//!
//! Cold start:
//!   1. init tracing
//!   2. register the turbolite "tiered" VFS pointed at `s3://{BUCKET}/{DB_NAME}`
//!   3. hand each HTTP request to [`rustyhip::handler::handle`]
//!
//! All S3 I/O is handled by the VFS — this binary has no direct `aws-sdk-s3`
//! use. `AWS_ENDPOINT_URL` and `AWS_REGION` flow through into turbolite's
//! config so floci, `LocalStack`, and `MinIO` work unchanged.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use lambda_http::{Error, run, service_fn};
use rustyhip::logging::elapsed_ms;
use rustyhip::state::AppState;
use rustyhip::{VERSION, db::SqliteDb, handler, settings};
use tracing::info;
use turbolite::tiered::{TurboliteConfig, TurboliteVfs, register};

const VFS_NAME: &str = "tiered";

#[tokio::main]
async fn main() -> Result<(), Error> {
    settings::init_logging();
    let started = Instant::now();
    info!(
        op = "bootstrap",
        phase = "start",
        version = VERSION,
        environment = %settings::environment(),
        "START bootstrap"
    );

    let bucket = settings::bucket()?;
    let prefix = settings::db_name()?;
    let cache_dir = settings::cache_dir();
    std::fs::create_dir_all(&cache_dir).with_context(|| format!("create cache_dir {}", cache_dir.display()))?;

    let config = TurboliteConfig {
        bucket,
        prefix,
        cache_dir: cache_dir.clone(),
        endpoint_url: std::env::var("AWS_ENDPOINT_URL").ok(),
        region: Some(settings::region()),
        // NOTE: turbolite's `wal` feature (wal_replication + wal_sync_interval_ms)
        // was the preferred durability mechanism, but it fails to compile against
        // upstream walrust 0.5.1 today (API mismatch on TurboliteStorage::list_objects).
        // Until that's fixed upstream, we force a checkpoint after every non-readonly
        // /sql call from the handler — see src/handler.rs::sql. Multi-writer
        // serialization is still enforced by ReservedConcurrentExecutions=1 — see
        // github.com/monkut/rustyhip/issues/1.
        ..Default::default()
    };
    let vfs = TurboliteVfs::new(config).context("build turbolite VFS")?;
    register(VFS_NAME, vfs).context("register turbolite VFS")?;

    let db_path = cache_dir.join("rustyhip.db");
    let db = Arc::new(SqliteDb::open_with_vfs(db_path, VFS_NAME).context("open db via turbolite VFS")?);
    let state = Arc::new(AppState::new(db));

    info!(op = "bootstrap", phase = "end", duration_ms = elapsed_ms(started), "END bootstrap");

    run(service_fn(move |req| {
        let state = state.clone();
        async move { handler::handle(state, req).await }
    }))
    .await
}
