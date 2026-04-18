//! JSON structured logging per the weyucou/sops logging guidelines.
//!
//! Emits one JSON object per log event with the required fields:
//!   `timestamp` (ISO 8601 UTC), `level`, `event`, `logger`, `service`, `environment`.
//! Extra key/value fields from `tracing` macros are folded in alongside those.
//!
//! Sensitive-field scrubber: any field whose name contains one of
//! [`SENSITIVE_FIELD_NAMES`] (case-insensitive substring match) is replaced
//! with `[REDACTED]` before serialization. This is a safety net — the primary
//! rule is never to pass sensitive data to the logger in the first place.

use std::fmt;

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Field-name substrings that trigger value redaction. Case-insensitive.
pub const SENSITIVE_FIELD_NAMES: &[&str] =
    &["password", "token", "secret", "ssn", "email", "credit_card", "authorization", "api_key"];

const REDACTED: &str = "[REDACTED]";

/// Saturating `Instant.elapsed()` → `u64` milliseconds. For log `duration_ms` fields.
#[must_use]
pub fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[must_use]
pub fn is_sensitive_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_FIELD_NAMES.iter().any(|needle| lower.contains(needle))
}

/// JSON event formatter bound to a fixed `service` + `environment`.
#[derive(Debug, Clone)]
pub struct JsonEventFormatter {
    service: String,
    environment: String,
}

impl JsonEventFormatter {
    pub fn new(service: impl Into<String>, environment: impl Into<String>) -> Self {
        Self { service: service.into(), environment: environment.into() }
    }
}

impl<S, N> FormatEvent<S, N> for JsonEventFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, _ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let meta = event.metadata();
        let mut obj: Map<String, Value> = Map::new();

        let ts = OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| String::from("unknown"));
        obj.insert("timestamp".into(), Value::String(ts));
        obj.insert("level".into(), Value::String(level_name(*meta.level()).to_owned()));
        obj.insert("logger".into(), Value::String(meta.target().to_owned()));
        obj.insert("service".into(), Value::String(self.service.clone()));
        obj.insert("environment".into(), Value::String(self.environment.clone()));

        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        for (k, v) in visitor.fields {
            if k == "message" {
                obj.insert("event".into(), v);
            } else if is_sensitive_field(&k) {
                obj.insert(k, Value::String(REDACTED.into()));
            } else {
                obj.insert(k, v);
            }
        }

        let line = serde_json::to_string(&obj).unwrap_or_else(|_| String::from("{}"));
        writeln!(writer, "{line}")
    }
}

const fn level_name(level: tracing::Level) -> &'static str {
    match level {
        tracing::Level::ERROR => "ERROR",
        tracing::Level::WARN => "WARNING",
        tracing::Level::INFO => "INFO",
        tracing::Level::DEBUG => "DEBUG",
        tracing::Level::TRACE => "TRACE",
    }
}

#[derive(Default)]
struct JsonVisitor {
    fields: Vec<(String, Value)>,
}

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.push((field.name().into(), Value::String(value.into())));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.push((field.name().into(), Value::Bool(value)));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.push((field.name().into(), Value::Number(value.into())));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.push((field.name().into(), Value::Number(value.into())));
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        let v = i64::try_from(value).map_or_else(|_| Value::String(value.to_string()), |n| Value::Number(n.into()));
        self.fields.push((field.name().into(), v));
    }
    fn record_u128(&mut self, field: &Field, value: u128) {
        let v = u64::try_from(value).map_or_else(|_| Value::String(value.to_string()), |n| Value::Number(n.into()));
        self.fields.push((field.name().into(), v));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.fields.push((field.name().into(), Value::Number(n)));
        } else {
            self.fields.push((field.name().into(), Value::Null));
        }
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.push((field.name().into(), Value::String(format!("{value:#}"))));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields.push((field.name().into(), Value::String(format!("{value:?}"))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denylist_matches_case_insensitively() {
        assert!(is_sensitive_field("password"));
        assert!(is_sensitive_field("PASSWORD"));
        assert!(is_sensitive_field("user_password"));
        assert!(is_sensitive_field("API_KEY"));
        assert!(is_sensitive_field("authorization"));
        assert!(is_sensitive_field("user_email"));
        assert!(is_sensitive_field("credit_card_last4"));
    }

    #[test]
    fn denylist_does_not_match_unrelated_fields() {
        assert!(!is_sensitive_field("path"));
        assert!(!is_sensitive_field("bucket"));
        assert!(!is_sensitive_field("user_id"));
        assert!(!is_sensitive_field("sql_bytes"));
        assert!(!is_sensitive_field("rows"));
    }
}
