//! Shared per-invocation state. With the turbolite VFS doing its own S3 I/O
//! there's nothing else for the handler to reach through — the DB handle is it.

use std::sync::Arc;

use crate::db::SqliteDb;

#[derive(Debug, Clone)]
pub struct AppState {
    pub db: Arc<SqliteDb>,
}

impl AppState {
    pub const fn new(db: Arc<SqliteDb>) -> Self {
        Self { db }
    }
}
