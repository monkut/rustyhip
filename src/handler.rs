//! HTTP Lambda handler. Accepts JSON request bodies and returns JSON responses.
//!
//! Wire format:
//!   `GET  /`        → health
//!   `GET  /health`  → health
//!   `POST /sql`     → `{"sql": "...", "params": [...]?}`
//!                   → `{"columns": [...], "rows": [...], "rowcount": N, "lastrowid": M, "readonly": bool}`
//!
//! Writes are persisted to S3 by the underlying turbolite VFS — the handler
//! itself does no S3 I/O.

use std::sync::Arc;
use std::time::Instant;

use lambda_http::{Body, Error, Request, Response, http::StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::VERSION;
use crate::logging::elapsed_ms;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
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
    info!(op = "handle_request", phase = "start", %method, %path, "START handle_request");

    let result = match (method.as_str(), path.as_str()) {
        ("GET", "/" | "/health") => health(),
        ("POST", "/sql") => sql(state.as_ref(), req.body()).await,
        _ => json_response(StatusCode::NOT_FOUND, &ErrorResponse { error: format!("no route for {method} {path}") }),
    };

    let status = result.as_ref().map_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.as_u16(), |r| r.status().as_u16());
    info!(
        op = "handle_request",
        phase = "end",
        duration_ms = elapsed_ms(started),
        status,
        %method,
        %path,
        "END handle_request"
    );
    result
}

fn health() -> Result<Response<Body>, Error> {
    json_response(StatusCode::OK, &HealthResponse { version: VERSION, status: "ok" })
}

async fn sql(state: &AppState, body: &Body) -> Result<Response<Body>, Error> {
    let bytes: &[u8] = body.as_ref();
    let req: SqlRequest = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "invalid JSON request body");
            return json_response(StatusCode::BAD_REQUEST, &ErrorResponse { error: format!("invalid JSON: {e}") });
        }
    };
    let started = Instant::now();
    info!(
        op = "handle_sql",
        phase = "start",
        sql_bytes = req.sql.len(),
        param_count = req.params.len(),
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
            if !outcome.readonly {
                if let Err(e) = state.db.exec("PRAGMA wal_checkpoint(TRUNCATE)".to_owned(), vec![]).await {
                    error!(error = ?e, "post-write checkpoint failed — write may not be durable in S3");
                    info!(
                        op = "handle_sql",
                        phase = "end",
                        duration_ms = elapsed_ms(started),
                        outcome = "checkpoint_error",
                        "END handle_sql"
                    );
                    return json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &ErrorResponse { error: format!("checkpoint failed: {e:#}") },
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
            let resp = json_response(StatusCode::BAD_REQUEST, &ErrorResponse { error: format!("{e:#}") });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SqliteDb;
    use lambda_http::http::Request as HttpRequest;
    use tempfile::TempDir;

    fn make_state() -> (TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("rustyhip.db");
        let db = Arc::new(SqliteDb::open(db_path).expect("open sqlite"));
        (dir, Arc::new(AppState::new(db)))
    }

    fn make_request(method: &str, path: &str, body: &str) -> Request {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .expect("build request")
    }

    fn parse_body(resp: &Response<Body>) -> serde_json::Value {
        let bytes: &[u8] = resp.body().as_ref();
        serde_json::from_slice(bytes).expect("json body")
    }

    /// State backed by an in-memory `SQLite` DB — fine for tests that never hit `/sql`.
    fn dummy_state() -> Arc<AppState> {
        let db = Arc::new(SqliteDb::open(":memory:").expect("open in-memory sqlite"));
        Arc::new(AppState::new(db))
    }

    #[tokio::test]
    async fn health_returns_ok_with_version() {
        let resp = handle(dummy_state(), make_request("GET", "/health", "")).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_body(&resp);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], VERSION);
    }

    #[tokio::test]
    async fn root_also_returns_health() {
        let resp = handle(dummy_state(), make_request("GET", "/", "")).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sql_rejects_invalid_json() {
        let resp = handle(dummy_state(), make_request("POST", "/sql", "not json")).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let resp = handle(dummy_state(), make_request("GET", "/nope", "")).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn readonly_select_works_through_default_vfs() {
        let (_dir, state) = make_state();
        // Seed via the same connection the handler uses — a second Connection::open
        // on the same file would clash with the still-held primary connection.
        state.db.exec("CREATE TABLE t (x INT)".into(), vec![]).await.expect("create");
        state.db.exec("INSERT INTO t VALUES (1)".into(), vec![]).await.expect("insert");
        let resp = handle(state, make_request("POST", "/sql", r#"{"sql":"SELECT x FROM t"}"#)).await.expect("handler");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = parse_body(&resp);
        assert_eq!(body["readonly"], true);
        assert_eq!(body["rows"][0]["x"], 1);
    }
}
