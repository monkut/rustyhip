//! (unused) The direct S3 fetch/upload helpers have been removed — turbolite's
//! tiered VFS handles all S3 I/O transparently inside
//! [`rusqlite::Connection::open_with_flags_and_vfs`]. This file is intentionally
//! empty; run `rm src/s3.rs` once you're comfortable deleting it from git.
