//! Runtime configuration — environment variables and logging setup.
//!
//! Logging follows the weyucou/sops logging guidelines: JSON structured output
//! by default (production / Lambda), opt into human-readable text locally with
//! `LOG_FORMAT=pretty`. See [`crate::logging`] for the JSON formatter.

use std::path::PathBuf;
use std::sync::Once;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::logging::JsonEventFormatter;

static INIT: Once = Once::new();

/// Default level used when `LOG_LEVEL` / `RUST_LOG` are unset.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default AWS region if neither `AWS_REGION` nor `REGION` is set.
pub const DEFAULT_REGION: &str = "ap-northeast-1";

/// Default cache directory for turbolite's local page cache inside a Lambda.
pub const DEFAULT_CACHE_DIR: &str = "/tmp/rustyhip-cache";

/// Logger `service` field — identifies this application in structured logs.
pub const SERVICE_NAME: &str = "rustyhip";

/// Environment variable naming the S3 bucket holding the database.
pub const ENV_BUCKET: &str = "BUCKET";
/// Environment variable naming the turbolite `prefix` (logical DB name) within the bucket.
pub const ENV_DB_NAME: &str = "DB_NAME";
/// Environment variable overriding the local cache directory for turbolite.
pub const ENV_CACHE_DIR: &str = "DB_CACHE_DIR";
/// Environment variable naming the deployment environment label (production / staging / development).
pub const ENV_ENVIRONMENT: &str = "ENVIRONMENT";
/// Environment variable selecting the log output format (`json` | `pretty`). Defaults to `json`.
pub const ENV_LOG_FORMAT: &str = "LOG_FORMAT";
/// Standard AWS SDK override for the service endpoint. When set, force S3 path-style addressing.
pub const ENV_AWS_ENDPOINT_URL: &str = "AWS_ENDPOINT_URL";
/// Per-service override for the S3 endpoint URL.
pub const ENV_AWS_ENDPOINT_URL_S3: &str = "AWS_ENDPOINT_URL_S3";

/// Default deployment environment when `ENVIRONMENT` is unset.
pub const DEFAULT_ENVIRONMENT: &str = "development";

/// AWS region, honoring `AWS_REGION` then `REGION` then [`DEFAULT_REGION`].
#[must_use]
pub fn region() -> String {
    std::env::var("AWS_REGION").or_else(|_| std::env::var("REGION")).unwrap_or_else(|_| DEFAULT_REGION.to_owned())
}

/// S3 bucket holding the database. Required.
pub fn bucket() -> Result<String> {
    std::env::var(ENV_BUCKET).with_context(|| format!("{ENV_BUCKET} env var not set"))
}

/// Turbolite `prefix` — the logical database name under the bucket. Required.
pub fn db_name() -> Result<String> {
    std::env::var(ENV_DB_NAME).with_context(|| format!("{ENV_DB_NAME} env var not set"))
}

/// Local cache directory for turbolite's page cache.
/// Defaults to [`DEFAULT_CACHE_DIR`]; override via `DB_CACHE_DIR`.
#[must_use]
pub fn cache_dir() -> PathBuf {
    std::env::var(ENV_CACHE_DIR).map_or_else(|_| PathBuf::from(DEFAULT_CACHE_DIR), PathBuf::from)
}

/// Deployment environment label attached to every log event.
#[must_use]
pub fn environment() -> String {
    std::env::var(ENV_ENVIRONMENT).unwrap_or_else(|_| DEFAULT_ENVIRONMENT.to_owned())
}

/// Log output format: `json` (structured) or `pretty` (human-readable). Case-insensitive.
#[must_use]
pub fn log_format() -> String {
    std::env::var(ENV_LOG_FORMAT).unwrap_or_else(|_| "json".to_owned())
}

/// `true` when a custom S3 endpoint is configured (`LocalStack`, `MinIO`, etc.) —
/// those services require path-style addressing, not virtual-hosted buckets.
#[must_use]
pub fn use_path_style_s3() -> bool {
    std::env::var(ENV_AWS_ENDPOINT_URL).is_ok() || std::env::var(ENV_AWS_ENDPOINT_URL_S3).is_ok()
}

/// Initialize tracing. Idempotent — safe to call from both `lib` and `bin`.
///
/// Honors `RUST_LOG`; falls back to `LOG_LEVEL`; finally to [`DEFAULT_LOG_LEVEL`].
/// Format is selected by [`log_format`]:
/// - `json` (default) → structured one-line JSON per event, required fields per the sops logging spec
/// - `pretty` / `text` / `human` → human-readable console output for local dev
pub fn init_logging() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| {
                let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_owned());
                EnvFilter::try_new(level)
            })
            .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL));

        let fmt = tracing_subscriber::fmt().with_env_filter(filter).with_target(true);
        let format = log_format();
        if matches!(format.to_ascii_lowercase().as_str(), "pretty" | "text" | "human") {
            fmt.init();
        } else {
            fmt.event_format(JsonEventFormatter::new(SERVICE_NAME, environment())).init();
        }
    });
}
