//! HTTP Lambda handler. Accepts JSON request bodies and returns JSON responses.
//!
//! Wire format:
//!   `GET  /`        → health
//!   `GET  /health`  → health
//!   `POST /sql`     → `{"sql": "...", "params": [...]?}`
//!                   → `{"columns": [...], "rows": [...], "rowcount": N, "lastrowid": M, "readonly": bool}`
//!
//! Auth: when `RUSTYHIP_AUTH_TOKEN` is set, every request (including `/health`)
//! must carry `Authorization: Bearer <token>`. Requests without a matching
//! token get `401` + structured error body. When the env var is unset the
//! handler accepts anonymous traffic (dev-only; bootstrap logs a warning).
//!
//! Error responses all follow the same shape:
//!     `{"error": {"code": "RUSTYHIP_E_*", "message": "...", "request_id": "..."}}`
//! Writes are persisted to S3 by the underlying turbolite VFS — the handler
//! itself does no S3 I/O.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use lambda_http::{Body, Error, Request, RequestExt, Response, http::StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::VERSION;
use crate::errors;
use crate::logging::elapsed_ms;
use crate::settings::CheckpointMode;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub request_id: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub version: &'static str,
    pub status: &'static str,
}

/// HTTP entry point. Routes by method + path and returns JSON.
pub async fn handle(state: Arc<AppState>, req: Request) -> Result<Response<Body>, Error> {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let request_id = extract_request_id(&req);
    info!(op = "handle_request", phase = "start", %method, %path, %request_id, "START handle_request");

    let result = match check_auth(&state, &req, &request_id) {
        Err(resp) => Ok(resp),
        Ok(()) => match (method.as_str(), path.as_str()) {
            ("GET", "/" | "/health") => health(),
            ("POST", "/sql") => sql(state.as_ref(), req.body(), &request_id).await,
            _ => json_error(
                StatusCode::NOT_FOUND,
                errors::NOT_FOUND,
                format!("no route for {method} {path}"),
                &request_id,
            ),
        },
    };

    let status = result.as_ref().map_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.as_u16(), |r| r.status().as_u16());
    info!(
        op = "handle_request",
        phase = "end",
        duration_ms = elapsed_ms(started),
        status,
        %method,
        %path,
        %request_id,
        "END handle_request"
    );
    result
}

fn health() -> Result<Response<Body>, Error> {
    json_response(StatusCode::OK, &HealthResponse { version: VERSION, status: "ok" })
}

/// Returns `Err(response)` when auth fails — caller short-circuits with it.
/// The Err variant intentionally carries a full `Response<Body>` so the caller
/// can return it verbatim; clippy's `result_large_err` is noise here.
#[allow(clippy::result_large_err)]
fn check_auth(state: &AppState, req: &Request, request_id: &str) -> Result<(), Response<Body>> {
    let Some(expected) = state.auth_token.as_deref() else {
        return Ok(());
    };
    let provided = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")));
    if provided == Some(expected) {
        return Ok(());
    }
    warn!(%request_id, "auth failed — rejecting request");
    let resp = json_error(
        StatusCode::UNAUTHORIZED,
        errors::UNAUTHORIZED,
        "invalid or missing bearer token".into(),
        request_id,
    )
    .unwrap_or_else(|_| {
        Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::Empty).expect("static 401 response")
    });
    Err(resp)
}

async fn sql(state: &AppState, body: &Body, request_id: &str) -> Result<Response<Body>, Error> {
    let bytes: &[u8] = body.as_ref();
    if let Some(max) = state.max_body_bytes
        && bytes.len() > max
    {
        warn!(body_bytes = bytes.len(), max, %request_id, "request body exceeded RUSTYHIP_MAX_BODY_BYTES");
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            errors::VALIDATION,
            format!("request body of {} bytes exceeds RUSTYHIP_MAX_BODY_BYTES={max}", bytes.len()),
            request_id,
        );
    }
    let req: SqlRequest = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "invalid JSON request body");
            return json_error(StatusCode::BAD_REQUEST, errors::VALIDATION, format!("invalid JSON: {e}"), request_id);
        }
    };
    let started = Instant::now();
    info!(
        op = "handle_sql",
        phase = "start",
        sql_bytes = req.sql.len(),
        param_count = req.params.len(),
        %request_id,
        "START handle_sql"
    );
    debug!(op = "handle_sql", sql = %req.sql, "sql text");

    match state.db.exec(req.sql, req.params).await {
        Ok(outcome) => {
            // Writes live in turbolite's local WAL on /tmp until checkpoint; SQLite's
            // default wal_autocheckpoint only fires every 1000 frames, so in a Lambda
            // a container eviction between writes and checkpoint would silently drop
            // data. Force a checkpoint after every non-readonly call — sync_mode=Durable
            // (turbolite default) blocks it until the S3 manifest+pages land.
            //
            // The mode is configurable via RUSTYHIP_CHECKPOINT_MODE; Lambda must
            // keep the default `Truncate` (see CLAUDE.md / state.rs).
            if !outcome.readonly
                && state.checkpoint_mode != CheckpointMode::Off
                && let Some(arg) = state.checkpoint_mode.as_pragma_arg()
            {
                let sql = format!("PRAGMA wal_checkpoint({arg})");
                if let Err(e) = state.db.exec(sql, vec![]).await {
                    error!(error = ?e, mode = ?state.checkpoint_mode, "post-write checkpoint failed — write may not be durable in S3");
                    info!(
                        op = "handle_sql",
                        phase = "end",
                        duration_ms = elapsed_ms(started),
                        outcome = "checkpoint_error",
                        "END handle_sql"
                    );
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        errors::INTERNAL,
                        format!("checkpoint failed: {e:#}"),
                        request_id,
                    );
                }
            }
            let resp = json_response(StatusCode::OK, &outcome);
            info!(
                op = "handle_sql",
                phase = "end",
                duration_ms = elapsed_ms(started),
                outcome = "ok",
                readonly = outcome.readonly,
                row_count = outcome.rows.len(),
                rowcount = outcome.rowcount,
                "END handle_sql"
            );
            resp
        }
        Err(e) => {
            error!(error = ?e, "sql exec failed");
            // `{:#}` surfaces the full anyhow context chain (e.g. "prepare statement: no such table: foo").
            let resp = json_error(StatusCode::BAD_REQUEST, errors::SQL, format!("{e:#}"), request_id);
            info!(
                op = "handle_sql",
                phase = "end",
                duration_ms = elapsed_ms(started),
                outcome = "error",
                "END handle_sql"
            );
            resp
        }
    }
}

fn json_response<T: Serialize>(status: StatusCode, payload: &T) -> Result<Response<Body>, Error> {
    let body = serde_json::to_vec(payload)?;
    let resp = Response::builder().status(status).header("content-type", "application/json").body(Body::from(body))?;
    Ok(resp)
}

fn json_error(
    status: StatusCode,
    code: &'static str,
    message: String,
    request_id: &str,
) -> Result<Response<Body>, Error> {
    json_response(status, &ErrorResponse { error: ErrorBody { code, message, request_id: request_id.to_owned() } })
}

fn extract_request_id(req: &Request) -> String {
    // In production `lambda_http` exposes the AWS-assigned request id via the
    // Lambda context. When absent (unit tests, local tooling) we fall back to
    // a timestamp-derived id so every log + error body still has something
    // traceable.
    if let Some(ctx) = req.lambda_context_ref() {
        return ctx.request_id.clone();
    }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("local-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SqliteDb;
    use lambda_http::http::Request as HttpRequest;
    use tempfile::TempDir;

    fn make_state(auth: Option<&str>) -> (TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("rustyhip.db");
        let db = Arc::new(SqliteDb::open(db_path).expect("open sqlite"));
        (dir, Arc::new(AppState::new(db, auth.map(str::to_owned))))
    }

    fn make_request(method: &str, path: &str, body: &str, auth_header: Option<&str>) -> Request {
        let mut builder = HttpRequest::builder().method(method).uri(path).header("content-type", "application/json");
        if let Some(token) = auth_header {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_owned())).expect("build request")
    }

    fn parse_body(resp: &Response<Body>) -> serde_json::Value {
        let bytes: &[u8] = resp.body().as_ref();
        serde_json::from_slice(bytes).expect("json body")
    }

    /// State backed by an in-memory `SQLite` DB with auth disabled.
    fn dummy_state() -> Arc<AppState> {
        let db = Arc::new(SqliteDb::open(":memory:").expect("open in-memory sqlite"));
        Arc::new(AppState::new(db, None))
    }

    #[tokio::test]
    async fn health_returns_ok_with_version() {
        let resp = handle(dummy_state(), make_request("GET", "/health", "", None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_body(&resp);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], VERSION);
    }

    #[tokio::test]
    async fn root_also_returns_health() {
        let resp = handle(dummy_state(), make_request("GET", "/", "", None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sql_rejects_invalid_json() {
        let resp = handle(dummy_state(), make_request("POST", "/sql", "not json", None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = parse_body(&resp);
        assert_eq!(body["error"]["code"], errors::VALIDATION);
        assert!(!body["error"]["request_id"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_route_returns_404_with_error_code() {
        let resp = handle(dummy_state(), make_request("GET", "/nope", "", None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = parse_body(&resp);
        assert_eq!(body["error"]["code"], errors::NOT_FOUND);
    }

    #[tokio::test]
    async fn missing_bearer_returns_401() {
        let (_dir, state) = make_state(Some("expected-token"));
        let resp = handle(state, make_request("GET", "/health", "", None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = parse_body(&resp);
        assert_eq!(body["error"]["code"], errors::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_bearer_returns_401() {
        let (_dir, state) = make_state(Some("expected-token"));
        let resp = handle(state, make_request("GET", "/health", "", Some("wrong-token"))).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn matching_bearer_passes_through() {
        let (_dir, state) = make_state(Some("expected-token"));
        let resp = handle(state, make_request("GET", "/health", "", Some("expected-token"))).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readonly_select_works_through_default_vfs() {
        let (_dir, state) = make_state(None);
        state.db.exec("CREATE TABLE t (x INT)".into(), vec![]).await.expect("create");
        state.db.exec("INSERT INTO t VALUES (1)".into(), vec![]).await.expect("insert");
        let resp =
            handle(state, make_request("POST", "/sql", r#"{"sql":"SELECT x FROM t"}"#, None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_body(&resp);
        assert_eq!(body["readonly"], true);
        assert_eq!(body["rows"][0]["x"], 1);
    }

    #[tokio::test]
    async fn body_exceeding_max_returns_413() {
        let (_dir, state) = make_state(None);
        let state = Arc::new((*state).clone().with_max_body_bytes(Some(16)));
        let resp =
            handle(state, make_request("POST", "/sql", r#"{"sql":"SELECT 1 AS one"}"#, None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = parse_body(&resp);
        assert_eq!(body["error"]["code"], errors::VALIDATION);
        assert!(body["error"]["message"].as_str().unwrap().contains("RUSTYHIP_MAX_BODY_BYTES=16"));
    }

    #[tokio::test]
    async fn body_within_max_passes_through() {
        let (_dir, state) = make_state(None);
        let state = Arc::new((*state).clone().with_max_body_bytes(Some(1024)));
        let resp =
            handle(state, make_request("POST", "/sql", r#"{"sql":"SELECT 1 AS one"}"#, None)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn checkpoint_mode_off_skips_pragma_call() {
        let (_dir, state) = make_state(None);
        let state = Arc::new((*state).clone().with_checkpoint_mode(CheckpointMode::Off));
        // A non-readonly write should succeed with no checkpoint attempted.
        let resp = handle(state.clone(), make_request("POST", "/sql", r#"{"sql":"CREATE TABLE t (x INTEGER)"}"#, None))
            .await
            .expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_body(&resp);
        assert_eq!(body["readonly"], false);
    }
}
