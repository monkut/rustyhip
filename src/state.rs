//! Shared per-invocation state. With the turbolite VFS doing its own S3 I/O
//! there's nothing else for the handler to reach through — the DB handle and
//! (optional) auth token are it.

use std::sync::Arc;

use crate::db::SqliteDb;
use crate::settings::CheckpointMode;

#[derive(Debug, Clone)]
pub struct AppState {
    pub db: Arc<SqliteDb>,
    /// Bearer token required on every request. `None` = auth disabled
    /// (dev-only; a warning is logged at startup when this is the case).
    pub auth_token: Option<String>,
    /// Hard cap on `/sql` request body bytes. `None` = no cap (Lambda's own
    /// 6 MB ceiling still applies in production).
    pub max_body_bytes: Option<usize>,
    /// `wal_checkpoint` mode applied after every non-readonly /sql call.
    /// `Truncate` is the only Lambda-safe value — see `CLAUDE.md`.
    pub checkpoint_mode: CheckpointMode,
}

impl AppState {
    pub const fn new(db: Arc<SqliteDb>, auth_token: Option<String>) -> Self {
        Self { db, auth_token, max_body_bytes: None, checkpoint_mode: CheckpointMode::Truncate }
    }

    /// Builder-style setter for the request body cap.
    #[must_use]
    pub const fn with_max_body_bytes(mut self, max: Option<usize>) -> Self {
        self.max_body_bytes = max;
        self
    }

    /// Builder-style setter for the post-write checkpoint mode.
    #[must_use]
    pub const fn with_checkpoint_mode(mut self, mode: CheckpointMode) -> Self {
        self.checkpoint_mode = mode;
        self
    }
}
