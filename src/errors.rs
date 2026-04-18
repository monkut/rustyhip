//! Stable error codes for the JSON error response body.
//!
//! Emitted as `{"error": {"code": "RUSTYHIP_E_*", "message": "...", "request_id": "..."}}`.
//! Downstream clients can pattern-match on `code`; the `message` is
//! human-readable and may change across releases.

pub const VALIDATION: &str = "RUSTYHIP_E_VALIDATION";
pub const SQL: &str = "RUSTYHIP_E_SQL";
pub const UNAUTHORIZED: &str = "RUSTYHIP_E_UNAUTHORIZED";
pub const NOT_FOUND: &str = "RUSTYHIP_E_NOT_FOUND";
pub const INTERNAL: &str = "RUSTYHIP_E_INTERNAL";
