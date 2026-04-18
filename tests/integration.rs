//! End-to-end tests for the Lambda HTTP handler against a real (default-VFS)
//! `SQLite` file. The turbolite VFS itself is exercised by the floci-backed
//! smoke tests in the justfile (`just floci-seed` + `just rustyhip-dev`).

use std::sync::Arc;

use lambda_http::http::Request as HttpRequest;
use lambda_http::{Body, Request};
use rustyhip::db::SqliteDb;
use rustyhip::handler::handle;
use rustyhip::state::AppState;
use tempfile::TempDir;

fn test_state() -> (TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("rustyhip.db");
    let db = Arc::new(SqliteDb::open(db_path).expect("open sqlite"));
    (dir, Arc::new(AppState::new(db)))
}

fn post_sql(body: &str) -> Request {
    HttpRequest::builder()
        .method("POST")
        .uri("/sql")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("build request")
}

#[tokio::test]
async fn health_endpoint_returns_200() {
    let (_dir, state) = test_state();
    let req = HttpRequest::builder().method("GET").uri("/health").body(Body::Empty).expect("build request");
    let resp = handle(state, req).await.expect("handler");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let (_dir, state) = test_state();
    let req = HttpRequest::builder().method("GET").uri("/does-not-exist").body(Body::Empty).expect("build request");
    let resp = handle(state, req).await.expect("handler");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn readonly_select_returns_rows() {
    let (_dir, state) = test_state();
    state.db.exec("CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)".into(), vec![]).await.expect("create");
    state.db.exec("INSERT INTO fruit (name) VALUES ('apple'), ('peach')".into(), vec![]).await.expect("insert");
    let resp = handle(state, post_sql(r#"{"sql":"SELECT name FROM fruit ORDER BY id"}"#)).await.expect("handler");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(resp.body().as_ref()).unwrap();
    assert_eq!(body["readonly"], true);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "apple");
}

#[tokio::test]
async fn sql_against_missing_table_returns_400() {
    let (_dir, state) = test_state();
    let resp = handle(state, post_sql(r#"{"sql":"SELECT * FROM does_not_exist"}"#)).await.expect("handler");
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = serde_json::from_slice(resp.body().as_ref()).unwrap();
    assert!(body["error"].as_str().unwrap().to_lowercase().contains("no such table"));
}
