//! `RustyHip` is a lambda front end providing `SQLite`-like database over S3,
//! backed by the turbolite tiered VFS.

pub mod db;
pub mod handler;
pub mod logging;
pub mod settings;
pub mod state;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
