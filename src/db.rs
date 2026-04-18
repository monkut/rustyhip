//! `SQLite` backend. Wraps synchronous rusqlite in `spawn_blocking` so async
//! callers (like the Lambda HTTP handler) can await statements without
//! starving the tokio runtime.
//!
//! In production the connection is opened through the `turbolite` VFS (see
//! `crate::main`), so all page I/O is routed to S3 via turbolite's tiered
//! cache. In tests we use the default `SQLite` VFS (local temp file) — the
//! queries themselves behave identically.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::info;

use crate::logging::elapsed_ms;

/// Long-lived handle to a `SQLite` connection.
///
/// Opened once at Lambda cold-start and shared across requests — turbolite
/// caches pages on this connection, and reopening it per request would lose
/// that state + confuse the WAL/SHM files.
#[derive(Debug, Clone)]
pub struct SqliteDb {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

/// Result of executing a single SQL statement.
#[derive(Debug, Serialize)]
pub struct ExecOutcome {
    /// Column names in declaration order (empty for non-`SELECT`).
    pub columns: Vec<String>,
    /// Result rows keyed by column name (empty for non-`SELECT`).
    pub rows: Vec<Value>,
    /// Rows changed by the statement (0 for `SELECT` / DDL).
    pub rowcount: i64,
    /// `last_insert_rowid` after the statement (0 when no `INSERT` has occurred in this connection).
    pub lastrowid: i64,
    /// `true` when the statement produced no schema/data changes (e.g. `SELECT`, read-only pragma).
    pub readonly: bool,
}

impl SqliteDb {
    /// Open through the default `SQLite` VFS (local file I/O). Used by tests.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), path })
    }

    /// Open through a named registered VFS (e.g. turbolite's "tiered").
    pub fn open_with_vfs(path: impl Into<PathBuf>, vfs: &str) -> Result<Self> {
        let path = path.into();
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
        let conn = Connection::open_with_flags_and_vfs(&path, flags, vfs)
            .with_context(|| format!("open {} via vfs {vfs}", path.display()))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `sql` (optionally with `params`) and return rows + change metadata.
    pub async fn exec(&self, sql: String, params: Vec<Value>) -> Result<ExecOutcome> {
        let started = Instant::now();
        let sql_bytes = sql.len();
        let param_count = params.len();
        info!(op = "db_exec", phase = "start", sql_bytes, param_count, "START db_exec");

        let conn = self.conn.clone();
        let result: Result<ExecOutcome> = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|e| anyhow!("connection mutex poisoned: {e}"))?;
            run_exec(&guard, &sql, params)
        })
        .await
        .context("sqlite worker panicked")?;

        match &result {
            Ok(o) => info!(
                op = "db_exec",
                phase = "end",
                duration_ms = elapsed_ms(started),
                outcome = "ok",
                readonly = o.readonly,
                row_count = o.rows.len(),
                rowcount = o.rowcount,
                "END db_exec"
            ),
            Err(e) => info!(
                op = "db_exec",
                phase = "end",
                duration_ms = elapsed_ms(started),
                outcome = "error",
                error = %format_args!("{e:#}"),
                "END db_exec"
            ),
        }
        result
    }
}

fn run_exec(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
    let bind_params: Vec<SqlValue> = params.into_iter().map(json_to_sql).collect::<Result<_>>()?;

    let mut stmt = conn.prepare(sql).context("prepare statement")?;
    let readonly = stmt.readonly();
    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let col_count = columns.len();

    let mut rows_iter = stmt.query(rusqlite::params_from_iter(bind_params.iter())).context("execute statement")?;
    let mut rows = Vec::new();
    while let Some(row) = rows_iter.next().context("fetch next row")? {
        let mut obj = Map::with_capacity(col_count);
        for (i, name) in columns.iter().enumerate() {
            let raw: SqlValue = row.get(i).with_context(|| format!("read column {i}"))?;
            obj.insert(name.clone(), sql_to_json(raw));
        }
        rows.push(Value::Object(obj));
    }
    drop(rows_iter);
    drop(stmt);

    let rowcount = i64::try_from(conn.changes()).unwrap_or(i64::MAX);
    let lastrowid = conn.last_insert_rowid();
    Ok(ExecOutcome { columns, rows, rowcount, lastrowid, readonly })
}

fn sql_to_json(v: SqlValue) -> Value {
    match v {
        SqlValue::Integer(n) => Value::from(n),
        // NaN/Inf aren't representable in JSON — fall back to null.
        SqlValue::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        SqlValue::Text(s) => Value::String(s),
        // TODO: surface blobs as base64 once we have a use case.
        SqlValue::Null | SqlValue::Blob(_) => Value::Null,
    }
}

fn json_to_sql(v: Value) -> Result<SqlValue> {
    match v {
        Value::Null => Ok(SqlValue::Null),
        // SQLite has no native bool — bind as 0/1 integers like rusqlite's ToSql does.
        Value::Bool(b) => Ok(SqlValue::Integer(i64::from(b))),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .ok_or_else(|| anyhow!("number {n} does not fit i64 or f64")),
        Value::String(s) => Ok(SqlValue::Text(s)),
        Value::Array(_) | Value::Object(_) => Err(anyhow!("cannot bind {v:?} as SQL parameter")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn empty_db() -> (NamedTempFile, SqliteDb) {
        let file = NamedTempFile::new().expect("tempfile");
        let db = SqliteDb::open(file.path().to_owned()).expect("open sqlite");
        (file, db)
    }

    async fn exec_sql(db: &SqliteDb, sql: &str) -> ExecOutcome {
        db.exec(sql.to_owned(), vec![]).await.expect("exec")
    }

    #[tokio::test]
    async fn create_table_is_not_readonly() {
        let (_f, db) = empty_db();
        let out = exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        assert!(!out.readonly);
        assert!(out.rows.is_empty());
    }

    #[tokio::test]
    async fn select_is_readonly() {
        let (_f, db) = empty_db();
        exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        let out = exec_sql(&db, "SELECT name FROM fruit").await;
        assert!(out.readonly);
        assert_eq!(out.columns, vec!["name".to_owned()]);
        assert_eq!(out.rows.len(), 0);
    }

    #[tokio::test]
    async fn insert_returns_rowcount_and_lastrowid() {
        let (_f, db) = empty_db();
        exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        let out = db
            .exec(
                "INSERT INTO fruit (name) VALUES (?), (?)".into(),
                vec![Value::String("apple".into()), Value::String("peach".into())],
            )
            .await
            .expect("insert");
        assert!(!out.readonly);
        assert_eq!(out.rowcount, 2);
        assert_eq!(out.lastrowid, 2);
    }

    #[tokio::test]
    async fn parameterized_select_works() {
        let (_f, db) = empty_db();
        exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        exec_sql(&db, "INSERT INTO fruit (name) VALUES ('apple'), ('peach'), ('pear')").await;
        let out = db
            .exec("SELECT name FROM fruit WHERE name = ?".into(), vec![Value::String("peach".into())])
            .await
            .expect("select");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0]["name"], "peach");
    }

    #[tokio::test]
    async fn missing_table_returns_error() {
        let (_f, db) = empty_db();
        let err = db.exec("SELECT * FROM does_not_exist".into(), vec![]).await.expect_err("should fail");
        assert!(err.chain().any(|e| e.to_string().contains("no such table")));
    }
}
