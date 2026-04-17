//! Runtime configuration — log level, AWS region, etc. Mirrors
//! `settings.py` in the Python cookiecutter template.

use std::sync::Once;

use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

/// Default level used when `LOG_LEVEL` / `RUST_LOG` are unset.
pub const DEFAULT_LOG_LEVEL: &str = "debug";
#[allow(dead_code)] // scaffold — remove when you wire AWS in
pub const DEFAULT_REGION: &str = "ap-northeast-1";

/// AWS region from `REGION` env var, falling back to `DEFAULT_REGION`.
#[allow(dead_code)] // scaffold — remove when you wire AWS in
#[must_use]
pub fn region() -> String {
    std::env::var("REGION").unwrap_or_else(|_| DEFAULT_REGION.to_string())
}

/// Initialize tracing. Idempotent — safe to call from both `lib` and `bin`.
///
/// Honors `RUST_LOG`; falls back to `LOG_LEVEL`; finally to [`DEFAULT_LOG_LEVEL`].
pub fn init_logging() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| {
                let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());
                EnvFilter::try_new(level)
            })
            .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL));

        tracing_subscriber::fmt().with_env_filter(filter).with_target(true).init();
    });
}
