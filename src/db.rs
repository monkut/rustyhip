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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::{debug, info};

use crate::logging::elapsed_ms;
use crate::settings::{ConfigKnobs, JournalMode, Synchronous, TempStore};

/// Subset of [`ConfigKnobs`] consumed by the DB layer. Cloned into [`SqliteDb`]
/// so per-request callers don't re-read env.
#[derive(Debug, Clone, Default)]
pub struct DbSettings {
    pub synchronous: Option<Synchronous>,
    pub journal_mode: Option<JournalMode>,
    pub page_cache_kb: Option<i64>,
    pub mmap_size: Option<i64>,
    pub temp_store: Option<TempStore>,
    pub busy_timeout_ms: Option<u32>,
    pub max_rows: Option<usize>,
    pub query_timeout_ms: Option<u64>,
}

impl DbSettings {
    /// Carve the DB-relevant fields out of the bootstrap-time knob set.
    #[must_use]
    pub const fn from_knobs(k: &ConfigKnobs) -> Self {
        Self {
            synchronous: k.synchronous,
            journal_mode: k.journal_mode,
            page_cache_kb: k.page_cache_kb,
            mmap_size: k.mmap_size,
            temp_store: k.temp_store,
            busy_timeout_ms: k.busy_timeout_ms,
            max_rows: k.max_rows,
            query_timeout_ms: k.query_timeout_ms,
        }
    }
}

/// Long-lived handle to a `SQLite` connection.
///
/// Opened once at Lambda cold-start and shared across requests — turbolite
/// caches pages on this connection, and reopening it per request would lose
/// that state + confuse the WAL/SHM files.
#[derive(Debug, Clone)]
pub struct SqliteDb {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    settings: DbSettings,
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
        Self::open_with(path, None, DbSettings::default())
    }

    /// Open through a named registered VFS (e.g. turbolite's "tiered").
    pub fn open_with_vfs(path: impl Into<PathBuf>, vfs: &str) -> Result<Self> {
        Self::open_with(path, Some(vfs), DbSettings::default())
    }

    /// Open with explicit VFS + settings. Applies any configured pragmas before
    /// returning so the connection comes out in its final state.
    pub fn open_with(path: impl Into<PathBuf>, vfs: Option<&str>, settings: DbSettings) -> Result<Self> {
        let path = path.into();
        let conn = match vfs {
            None => Connection::open(&path).with_context(|| format!("open {}", path.display()))?,
            Some(name) => {
                let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
                Connection::open_with_flags_and_vfs(&path, flags, name)
                    .with_context(|| format!("open {} via vfs {name}", path.display()))?
            }
        };
        apply_pragmas(&conn, &settings);
        Ok(Self { conn: Arc::new(Mutex::new(conn)), path, settings })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the resolved settings — useful for tests and structured logging.
    #[must_use]
    pub const fn settings(&self) -> &DbSettings {
        &self.settings
    }

    /// Run `sql` (optionally with `params`) and return rows + change metadata.
    pub async fn exec(&self, sql: String, params: Vec<Value>) -> Result<ExecOutcome> {
        let started = Instant::now();
        let sql_bytes = sql.len();
        let param_count = params.len();
        info!(op = "db_exec", phase = "start", sql_bytes, param_count, "START db_exec");

        let conn = self.conn.clone();
        let settings = self.settings.clone();
        let result: Result<ExecOutcome> = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|e| anyhow!("connection mutex poisoned: {e}"))?;
            run_exec_with_settings(&guard, &sql, params, &settings)
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

/// Wrap [`run_exec`] with timeout enforcement (`RUSTYHIP_QUERY_TIMEOUT_MS`) via
/// `Connection::progress_handler`. The handler returns `true` once the wall-clock
/// deadline elapses, which causes the current `step()` to return `SQLITE_INTERRUPT`.
/// The handler is installed before, and cleared after, the call so subsequent
/// queries on the same connection aren't affected by a stale closure.
fn run_exec_with_settings(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    settings: &DbSettings,
) -> Result<ExecOutcome> {
    let timeout_installed = settings.query_timeout_ms.is_some();
    if let Some(ms) = settings.query_timeout_ms {
        let deadline = Instant::now() + Duration::from_millis(ms);
        // Sample every ~10k VDBE ops — frequent enough to bound runaway queries
        // without measurably moving the floor (see results/benchmarks.md).
        conn.progress_handler(10_000, Some(move || Instant::now() > deadline));
    }
    let result = run_exec(conn, sql, params, settings.max_rows);
    if timeout_installed {
        conn.progress_handler::<fn() -> bool>(0, None);
    }
    match result {
        Err(e) if timeout_installed && error_is_interrupt(&e) => {
            // The progress_handler returned true → SQLite raised SQLITE_INTERRUPT.
            // Surface that as a clearer timeout-shaped error so handler logs make sense.
            Err(anyhow!("query timeout exceeded {}ms", settings.query_timeout_ms.unwrap_or(0)))
        }
        other => other,
    }
}

/// Walk the anyhow error chain looking for a `SQLite` `SQLITE_INTERRUPT` code.
/// Typed match against `rusqlite::Error::SqliteFailure` so we don't depend on
/// the underlying libsqlite3 message wording.
fn error_is_interrupt(e: &anyhow::Error) -> bool {
    e.chain().any(|src| {
        src.downcast_ref::<rusqlite::Error>()
            .and_then(rusqlite::Error::sqlite_error_code)
            .is_some_and(|code| code == rusqlite::ffi::ErrorCode::OperationInterrupted)
    })
}

fn run_exec(conn: &Connection, sql: &str, params: Vec<Value>, max_rows: Option<usize>) -> Result<ExecOutcome> {
    let bind_params: Vec<SqlValue> = params.into_iter().map(json_to_sql).collect::<Result<_>>()?;

    let mut stmt = conn.prepare(sql).context("prepare statement")?;
    let readonly = stmt.readonly();
    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let col_count = columns.len();

    let mut rows_iter = stmt.query(rusqlite::params_from_iter(bind_params.iter())).context("execute statement")?;
    let mut rows = Vec::new();
    while let Some(row) = rows_iter.next().context("fetch next row")? {
        if let Some(cap) = max_rows
            && rows.len() >= cap
        {
            return Err(anyhow!("result exceeded RUSTYHIP_MAX_ROWS={cap}; refusing to materialize more rows"));
        }
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

/// Apply configured pragmas to a freshly-opened connection. PRAGMA failures are
/// logged but non-fatal — a bad value (e.g. `journal_mode=wal` on a VFS that
/// rejects it) should not prevent bootstrap.
fn apply_pragmas(conn: &Connection, settings: &DbSettings) {
    let mut applied: Vec<(&'static str, String)> = Vec::new();
    if let Some(jm) = settings.journal_mode {
        try_pragma(conn, &format!("PRAGMA journal_mode = {}", jm.as_pragma()), "journal_mode");
        applied.push(("journal_mode", jm.as_pragma().to_owned()));
    }
    if let Some(sync) = settings.synchronous {
        try_pragma(conn, &format!("PRAGMA synchronous = {}", sync.as_pragma()), "synchronous");
        applied.push(("synchronous", sync.as_pragma().to_owned()));
    }
    if let Some(ts) = settings.temp_store {
        try_pragma(conn, &format!("PRAGMA temp_store = {}", ts.as_pragma()), "temp_store");
        applied.push(("temp_store", ts.as_pragma().to_owned()));
    }
    if let Some(kb) = settings.page_cache_kb {
        // SQLite negative-form: -N means N KB (positive = page count).
        try_pragma(conn, &format!("PRAGMA cache_size = -{}", kb.abs()), "cache_size");
        applied.push(("cache_size_kb", kb.to_string()));
    }
    if let Some(mmap) = settings.mmap_size {
        try_pragma(conn, &format!("PRAGMA mmap_size = {mmap}"), "mmap_size");
        applied.push(("mmap_size", mmap.to_string()));
    }
    if let Some(busy_ms) = settings.busy_timeout_ms {
        try_pragma(conn, &format!("PRAGMA busy_timeout = {busy_ms}"), "busy_timeout");
        applied.push(("busy_timeout_ms", busy_ms.to_string()));
    }
    if !applied.is_empty() {
        debug!(pragmas = ?applied, "applied DB pragmas from config");
    }
}

fn try_pragma(conn: &Connection, sql: &str, label: &str) {
    if let Err(e) = conn.execute_batch(sql) {
        tracing::warn!(pragma = label, sql, error = %e, "failed to apply pragma — continuing with previous value");
    }
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

    #[tokio::test]
    async fn synchronous_pragma_is_applied() {
        let file = NamedTempFile::new().expect("tempfile");
        let settings = DbSettings { synchronous: Some(Synchronous::Normal), ..DbSettings::default() };
        let db = SqliteDb::open_with(file.path().to_owned(), None, settings).expect("open");
        let out = db.exec("PRAGMA synchronous".into(), vec![]).await.expect("read pragma");
        // sqlite reports synchronous as 0/1/2/3 — NORMAL = 1.
        assert_eq!(out.rows[0]["synchronous"], 1);
    }

    #[tokio::test]
    async fn journal_mode_pragma_is_applied() {
        let file = NamedTempFile::new().expect("tempfile");
        let settings = DbSettings { journal_mode: Some(JournalMode::Memory), ..DbSettings::default() };
        let db = SqliteDb::open_with(file.path().to_owned(), None, settings).expect("open");
        let out = db.exec("PRAGMA journal_mode".into(), vec![]).await.expect("read pragma");
        assert_eq!(out.rows[0]["journal_mode"], "memory");
    }

    #[tokio::test]
    async fn max_rows_clip_returns_error_before_overrunning() {
        let file = NamedTempFile::new().expect("tempfile");
        let settings = DbSettings { max_rows: Some(2), ..DbSettings::default() };
        let db = SqliteDb::open_with(file.path().to_owned(), None, settings).expect("open");
        db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)".into(), vec![]).await.expect("create");
        db.exec("INSERT INTO t VALUES (1),(2),(3),(4)".into(), vec![]).await.expect("seed");
        let err = db.exec("SELECT id FROM t".into(), vec![]).await.expect_err("should clip");
        assert!(err.to_string().contains("RUSTYHIP_MAX_ROWS=2"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn max_rows_unset_returns_all() {
        let (_f, db) = empty_db();
        db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)".into(), vec![]).await.expect("create");
        db.exec("INSERT INTO t VALUES (1),(2),(3),(4)".into(), vec![]).await.expect("seed");
        let out = db.exec("SELECT id FROM t".into(), vec![]).await.expect("select");
        assert_eq!(out.rows.len(), 4);
    }

    #[tokio::test]
    async fn query_timeout_aborts_runaway_query() {
        let file = NamedTempFile::new().expect("tempfile");
        let settings = DbSettings { query_timeout_ms: Some(50), ..DbSettings::default() };
        let db = SqliteDb::open_with(file.path().to_owned(), None, settings).expect("open");
        // Recursive CTE that won't finish before the 50ms deadline — progress_handler
        // fires every 10k VDBE ops, so this trips well before any meaningful wall-clock
        // budget is consumed.
        let err = db
            .exec("WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c) SELECT COUNT(*) FROM c".into(), vec![])
            .await
            .expect_err("should time out");
        assert!(err.to_string().contains("query timeout exceeded"), "unexpected error: {err}");
    }
}
