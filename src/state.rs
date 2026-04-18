//! Shared per-invocation state. With the turbolite VFS doing its own S3 I/O
//! there's nothing else for the handler to reach through — the DB handle and
//! (optional) auth token are it.

use std::sync::Arc;

use crate::db::SqliteDb;

#[derive(Debug, Clone)]
pub struct AppState {
    pub db: Arc<SqliteDb>,
    /// Bearer token required on every request. `None` = auth disabled
    /// (dev-only; a warning is logged at startup when this is the case).
    pub auth_token: Option<String>,
}

impl AppState {
    pub const fn new(db: Arc<SqliteDb>, auth_token: Option<String>) -> Self {
        Self { db, auth_token }
    }
}
