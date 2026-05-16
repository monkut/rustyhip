//! Runtime configuration — environment variables and logging setup.
//!
//! Logging follows the weyucou/sops logging guidelines: JSON structured output
//! by default (production / Lambda), opt into human-readable text locally with
//! `LOG_FORMAT=pretty`. See [`crate::logging`] for the JSON formatter.

use std::path::PathBuf;
use std::sync::Once;

use anyhow::{Context, Result, anyhow};
use tracing::warn;
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
/// Environment variable holding the bearer token every request must present.
/// Unset / empty = auth disabled (local dev only; startup logs a warning).
pub const ENV_AUTH_TOKEN: &str = "RUSTYHIP_AUTH_TOKEN";
/// Standard AWS SDK override for the service endpoint. When set, force S3 path-style addressing.
pub const ENV_AWS_ENDPOINT_URL: &str = "AWS_ENDPOINT_URL";
/// Per-service override for the S3 endpoint URL.
pub const ENV_AWS_ENDPOINT_URL_S3: &str = "AWS_ENDPOINT_URL_S3";

// --- DB / SQLite pragmas (P0) — applied once at connection open. Unset = SQLite default. ---
/// `PRAGMA synchronous` — `full` (default) / `normal` / `off` / `extra`.
pub const ENV_SYNCHRONOUS: &str = "RUSTYHIP_SYNCHRONOUS";
/// `PRAGMA journal_mode` — `delete` / `wal` / `memory` / `off` / `truncate` / `persist`.
pub const ENV_JOURNAL_MODE: &str = "RUSTYHIP_JOURNAL_MODE";
/// `PRAGMA cache_size` in **KB** (negative-form to `SQLite`, so we negate before applying).
pub const ENV_PAGE_CACHE_KB: &str = "RUSTYHIP_PAGE_CACHE_KB";
/// `PRAGMA mmap_size` in bytes.
pub const ENV_MMAP_SIZE: &str = "RUSTYHIP_MMAP_SIZE";
/// `PRAGMA temp_store` — `default` / `file` / `memory`.
pub const ENV_TEMP_STORE: &str = "RUSTYHIP_TEMP_STORE";
/// `PRAGMA busy_timeout` in ms.
pub const ENV_BUSY_TIMEOUT_MS: &str = "RUSTYHIP_BUSY_TIMEOUT_MS";

// --- Request-shaping knobs (P0/P1) ---
/// Hard cap on rows returned per `/sql` call. `0` or unset = no cap.
pub const ENV_MAX_ROWS: &str = "RUSTYHIP_MAX_ROWS";
/// Per-statement wall-clock timeout in ms. Unset = no timeout.
pub const ENV_QUERY_TIMEOUT_MS: &str = "RUSTYHIP_QUERY_TIMEOUT_MS";
/// Max request body size in bytes. Unset = no cap (Lambda enforces its own 6 MB ceiling).
pub const ENV_MAX_BODY_BYTES: &str = "RUSTYHIP_MAX_BODY_BYTES";

// --- Durability knob (P1) ---
/// Post-write checkpoint mode.
///
/// Values: `truncate` (default) / `restart` / `full` / `passive` / `off`.
/// **Lambda durability requires `truncate`.** Override only in single-tenant
/// container deployments where you understand the trade-off (see `CLAUDE.md`).
pub const ENV_CHECKPOINT_MODE: &str = "RUSTYHIP_CHECKPOINT_MODE";

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

/// Bearer token every HTTP request must present in the `Authorization` header.
///
/// `None` when the env var is unset or empty — auth is disabled. The main
/// bootstrap logs a warning on startup when that happens.
#[must_use]
pub fn auth_token() -> Option<String> {
    std::env::var(ENV_AUTH_TOKEN).ok().filter(|s| !s.is_empty())
}

/// `PRAGMA synchronous` setting. `SQLite` default is `Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synchronous {
    Off,
    Normal,
    Full,
    Extra,
}

impl Synchronous {
    #[must_use]
    pub const fn as_pragma(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }
}

/// `PRAGMA journal_mode` setting. `SQLite` default is `Delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMode {
    Delete,
    Truncate,
    Persist,
    Memory,
    Wal,
    Off,
}

impl JournalMode {
    #[must_use]
    pub const fn as_pragma(self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::Persist => "PERSIST",
            Self::Memory => "MEMORY",
            Self::Wal => "WAL",
            Self::Off => "OFF",
        }
    }
}

/// `PRAGMA temp_store` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempStore {
    Default,
    File,
    Memory,
}

impl TempStore {
    #[must_use]
    pub const fn as_pragma(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::File => "FILE",
            Self::Memory => "MEMORY",
        }
    }
}

/// Post-write checkpoint mode. `Truncate` is required for Lambda durability —
/// any other value trades durability or visibility for throughput.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMode {
    #[default]
    Truncate,
    Restart,
    Full,
    Passive,
    Off,
}

impl CheckpointMode {
    /// SQL fragment to splice into `PRAGMA wal_checkpoint(<here>)`. `Off` returns
    /// `None` — callers should skip the checkpoint entirely instead of executing.
    #[must_use]
    pub const fn as_pragma_arg(self) -> Option<&'static str> {
        match self {
            Self::Truncate => Some("TRUNCATE"),
            Self::Restart => Some("RESTART"),
            Self::Full => Some("FULL"),
            Self::Passive => Some("PASSIVE"),
            Self::Off => None,
        }
    }
}

/// Parsed config knobs read once at bootstrap. `None` = honor SQLite/handler default.
#[derive(Debug, Clone, Default)]
pub struct ConfigKnobs {
    pub synchronous: Option<Synchronous>,
    pub journal_mode: Option<JournalMode>,
    pub page_cache_kb: Option<i64>,
    pub mmap_size: Option<i64>,
    pub temp_store: Option<TempStore>,
    pub busy_timeout_ms: Option<u32>,
    pub max_rows: Option<usize>,
    pub query_timeout_ms: Option<u64>,
    pub max_body_bytes: Option<usize>,
    pub checkpoint_mode: CheckpointMode,
}

/// Read all P0/P1 knobs from the environment. Bad values log a warning and fall
/// back to the unset behavior rather than failing bootstrap — a typo in
/// `RUSTYHIP_SYNCHRONOUS` shouldn't take production down.
#[must_use]
pub fn config_knobs() -> ConfigKnobs {
    ConfigKnobs {
        synchronous: parse_env(ENV_SYNCHRONOUS, parse_synchronous),
        journal_mode: parse_env(ENV_JOURNAL_MODE, parse_journal_mode),
        page_cache_kb: parse_env(ENV_PAGE_CACHE_KB, |s| s.parse::<i64>().map_err(|e| anyhow!("{e}"))),
        mmap_size: parse_env(ENV_MMAP_SIZE, |s| s.parse::<i64>().map_err(|e| anyhow!("{e}"))),
        temp_store: parse_env(ENV_TEMP_STORE, parse_temp_store),
        busy_timeout_ms: parse_env(ENV_BUSY_TIMEOUT_MS, |s| s.parse::<u32>().map_err(|e| anyhow!("{e}"))),
        max_rows: parse_env(ENV_MAX_ROWS, |s| s.parse::<usize>().map_err(|e| anyhow!("{e}")))
            .and_then(|n| if n == 0 { None } else { Some(n) }),
        query_timeout_ms: parse_env(ENV_QUERY_TIMEOUT_MS, |s| s.parse::<u64>().map_err(|e| anyhow!("{e}"))).and_then(
            |n| {
                if n == 0 { None } else { Some(n) }
            },
        ),
        max_body_bytes: parse_env(ENV_MAX_BODY_BYTES, |s| s.parse::<usize>().map_err(|e| anyhow!("{e}"))).and_then(
            |n| {
                if n == 0 { None } else { Some(n) }
            },
        ),
        checkpoint_mode: parse_env(ENV_CHECKPOINT_MODE, parse_checkpoint_mode).unwrap_or_default(),
    }
}

fn parse_env<T>(key: &str, parser: fn(&str) -> Result<T>) -> Option<T> {
    let raw = std::env::var(key).ok().filter(|s| !s.is_empty())?;
    match parser(raw.trim()) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(env = key, value = %raw, error = %e, "invalid env value — falling back to default");
            None
        }
    }
}

fn parse_synchronous(s: &str) -> Result<Synchronous> {
    match s.to_ascii_lowercase().as_str() {
        "off" | "0" => Ok(Synchronous::Off),
        "normal" | "1" => Ok(Synchronous::Normal),
        "full" | "2" => Ok(Synchronous::Full),
        "extra" | "3" => Ok(Synchronous::Extra),
        other => Err(anyhow!("unknown synchronous mode '{other}'")),
    }
}

fn parse_journal_mode(s: &str) -> Result<JournalMode> {
    match s.to_ascii_lowercase().as_str() {
        "delete" => Ok(JournalMode::Delete),
        "truncate" => Ok(JournalMode::Truncate),
        "persist" => Ok(JournalMode::Persist),
        "memory" => Ok(JournalMode::Memory),
        "wal" => Ok(JournalMode::Wal),
        "off" => Ok(JournalMode::Off),
        other => Err(anyhow!("unknown journal mode '{other}'")),
    }
}

fn parse_temp_store(s: &str) -> Result<TempStore> {
    match s.to_ascii_lowercase().as_str() {
        "default" | "0" => Ok(TempStore::Default),
        "file" | "1" => Ok(TempStore::File),
        "memory" | "2" | "mem" => Ok(TempStore::Memory),
        other => Err(anyhow!("unknown temp_store '{other}'")),
    }
}

fn parse_checkpoint_mode(s: &str) -> Result<CheckpointMode> {
    match s.to_ascii_lowercase().as_str() {
        "truncate" => Ok(CheckpointMode::Truncate),
        "restart" => Ok(CheckpointMode::Restart),
        "full" => Ok(CheckpointMode::Full),
        "passive" => Ok(CheckpointMode::Passive),
        "off" | "skip" | "none" => Ok(CheckpointMode::Off),
        other => Err(anyhow!("unknown checkpoint mode '{other}'")),
    }
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
