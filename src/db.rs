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
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{debug, info};

use crate::logging::elapsed_ms;
use crate::settings::{CheckpointMode, ConfigKnobs, JournalMode, Synchronous, TempStore};

/// Failure modes of [`SqliteDb::exec_durable`], split so the HTTP layer can map
/// them to distinct status codes without string-matching.
#[derive(Debug)]
pub enum ExecError {
    /// Statement preparation/execution failed — a client error (bad SQL,
    /// missing table, parameter mismatch, …).
    Sql(anyhow::Error),
    /// The statement succeeded but the durability checkpoint did not complete —
    /// the write may not have reached S3 and must not be acked.
    Checkpoint(anyhow::Error),
    /// Worker/runtime failure (blocking task panicked or was cancelled).
    Internal(anyhow::Error),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `{:#}` surfaces the full anyhow context chain in one line.
        let (Self::Sql(e) | Self::Checkpoint(e) | Self::Internal(e)) = self;
        write!(f, "{e:#}")
    }
}

impl std::error::Error for ExecError {}

/// Result row of `PRAGMA wal_checkpoint(...)`: `(busy, log, checkpointed)`.
/// `busy != 0` means the checkpoint could not run to completion.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointStats {
    pub busy: i64,
    pub log: i64,
    pub checkpointed: i64,
}

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

/// Shape of each element of [`ExecOutcome::rows`], selected per request via
/// the `rows_format` body field (monkut/rustyhip#23).
///
/// `Objects` (default, backward-compatible) repeats every column name in every
/// row; `Arrays` emits each row as a plain value array aligned with
/// [`ExecOutcome::columns`], cutting both serialization work and payload size
/// for large results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RowsFormat {
    #[default]
    Objects,
    Arrays,
}

/// Result of executing a single SQL statement.
#[derive(Debug, Serialize)]
pub struct ExecOutcome {
    /// Column names in declaration order (empty for non-`SELECT`).
    pub columns: Vec<String>,
    /// Result rows (empty for non-`SELECT`): objects keyed by column name, or
    /// value arrays aligned with `columns` — see [`RowsFormat`].
    pub rows: Vec<Value>,
    /// Rows changed by the statement. Always 0 for read-only statements
    /// (`SELECT`, read-form pragmas); 0 for DDL.
    pub rowcount: i64,
    /// `last_insert_rowid` after a write statement (0 for read-only
    /// statements; for non-`INSERT` writes it carries the connection's most
    /// recent `INSERT` rowid).
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
        // Installed AFTER apply_pragmas so bootstrap-time configuration (which
        // uses value-form pragmas) is unaffected. From here on every prepared
        // statement — client SQL and the internal wal_checkpoint alike — goes
        // through `authorize_action`.
        conn.authorizer(Some(|ctx: AuthContext<'_>| authorize_action(&ctx.action)));
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
    /// No durability checkpoint — tests and local tooling only. Production
    /// callers use [`Self::exec_durable`].
    pub async fn exec(&self, sql: String, params: Vec<Value>) -> Result<ExecOutcome> {
        self.exec_durable(sql, params, RowsFormat::Objects, CheckpointMode::Off).await.map_err(|e| match e {
            ExecError::Sql(e) | ExecError::Checkpoint(e) | ExecError::Internal(e) => e,
        })
    }

    /// Run `sql`, then — when the statement leaves durable work in the WAL —
    /// run `PRAGMA wal_checkpoint(<mode>)` **in the same blocking task, under
    /// the same connection lock**, so the canonical state lands in S3 (via the
    /// turbolite VFS) before the caller acks. See `CLAUDE.md`.
    ///
    /// Checkpoint gating: `sqlite3_stmt_readonly` reports transaction-control
    /// statements (`BEGIN`/`COMMIT`/`ROLLBACK`/…) as read-only, so the readonly
    /// flag alone would skip the flush of a committed transaction. Instead we
    /// checkpoint when the connection is in autocommit after the statement AND
    /// (the statement wrote, OR it just closed an explicit transaction).
    /// Statements *inside* an open transaction are never checkpointed —
    /// uncommitted frames cannot be flushed, and until `COMMIT` returns the
    /// client holds no durability promise for them.
    ///
    /// A checkpoint that reports `busy != 0` did not flush everything and is
    /// surfaced as [`ExecError::Checkpoint`] — callers must not ack the write.
    pub async fn exec_durable(
        &self,
        sql: String,
        params: Vec<Value>,
        rows_format: RowsFormat,
        mode: CheckpointMode,
    ) -> Result<ExecOutcome, ExecError> {
        let started = Instant::now();
        let sql_bytes = sql.len();
        let param_count = params.len();
        info!(op = "db_exec", phase = "start", sql_bytes, param_count, "START db_exec");

        let conn = self.conn.clone();
        let settings = self.settings.clone();
        let result: Result<ExecOutcome, ExecError> = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|e| ExecError::Internal(anyhow!("connection mutex poisoned: {e}")))?;
            let was_in_txn = !guard.is_autocommit();
            let outcome =
                run_exec_with_settings(&guard, &sql, params, rows_format, &settings).map_err(ExecError::Sql)?;
            let needs_checkpoint = guard.is_autocommit() && (!outcome.readonly || was_in_txn);
            if needs_checkpoint && let Some(arg) = mode.as_pragma_arg() {
                let stats = run_checkpoint(&guard, arg).map_err(ExecError::Checkpoint)?;
                info!(
                    op = "db_checkpoint",
                    mode = arg,
                    busy = stats.busy,
                    log = stats.log,
                    checkpointed = stats.checkpointed,
                    "post-write wal_checkpoint"
                );
                if stats.busy != 0 {
                    return Err(ExecError::Checkpoint(anyhow!(
                        "wal_checkpoint({arg}) could not complete (busy={}, log={}, checkpointed={})",
                        stats.busy,
                        stats.log,
                        stats.checkpointed
                    )));
                }
            }
            drop(guard);
            Ok(outcome)
        })
        .await
        .map_err(|e| ExecError::Internal(anyhow!("sqlite worker panicked: {e}")))?;

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
                error = %e,
                "END db_exec"
            ),
        }
        result
    }
}

/// Statement-surface policy, enforced via the `SQLite` authorizer on every
/// prepared statement (monkut/rustyhip#18):
///
/// - `ATTACH` / `DETACH` are denied — an attached database bypasses the
///   turbolite VFS, so anything written there silently never reaches S3, and
///   the filename argument would allow reading/writing arbitrary container
///   filesystem paths.
/// - `PRAGMA` is denied except:
///   - `wal_checkpoint` (any form) — it *is* the durability flush and is
///     harmless when a client triggers it early;
///   - argument-taking introspection pragmas (`table_info`, `index_list`, …)
///     which never write;
///   - any pragma in read form (no value) — reconfiguring the shared
///     long-lived connection (`journal_mode`, `synchronous`, …) is what we
///     must prevent, and every config change requires the value form.
/// - Everything else (DML, DDL, transactions, reads) is allowed.
fn authorize_action(action: &AuthAction<'_>) -> Authorization {
    match action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        AuthAction::Pragma { pragma_name, pragma_value } => authorize_pragma(pragma_name, pragma_value.is_some()),
        _ => Authorization::Allow,
    }
}

/// Introspection pragmas whose argument selects *what to read* (a table, an
/// index, a row budget) — safe in both no-arg and arg forms.
const ARG_SAFE_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

fn authorize_pragma(name: &str, has_value: bool) -> Authorization {
    let lower = name.to_ascii_lowercase();
    if lower == "wal_checkpoint" || ARG_SAFE_PRAGMAS.contains(&lower.as_str()) {
        return Authorization::Allow;
    }
    if has_value { Authorization::Deny } else { Authorization::Allow }
}

/// Run `PRAGMA wal_checkpoint(<arg>)` through a lean path — no JSON row
/// materialization — and return its `(busy, log, checkpointed)` result row.
/// On a non-WAL database `SQLite` returns `(0, -1, -1)`, i.e. success.
fn run_checkpoint(conn: &Connection, arg: &str) -> Result<CheckpointStats> {
    let sql = format!("PRAGMA wal_checkpoint({arg})");
    conn.query_row(&sql, [], |row| {
        Ok(CheckpointStats { busy: row.get(0)?, log: row.get(1)?, checkpointed: row.get(2)? })
    })
    .with_context(|| format!("run {sql}"))
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
    rows_format: RowsFormat,
    settings: &DbSettings,
) -> Result<ExecOutcome> {
    let timeout_installed = settings.query_timeout_ms.is_some();
    if let Some(ms) = settings.query_timeout_ms {
        let deadline = Instant::now() + Duration::from_millis(ms);
        // Sample every ~10k VDBE ops — frequent enough to bound runaway queries
        // without measurably moving the floor (see results/benchmarks.md).
        conn.progress_handler(10_000, Some(move || Instant::now() > deadline));
    }
    let result = run_exec(conn, sql, params, settings.max_rows, rows_format);
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

fn run_exec(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    max_rows: Option<usize>,
    rows_format: RowsFormat,
) -> Result<ExecOutcome> {
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
        let value = match rows_format {
            RowsFormat::Objects => {
                let mut obj = Map::with_capacity(col_count);
                for (i, name) in columns.iter().enumerate() {
                    let raw: SqlValue = row.get(i).with_context(|| format!("read column {i}"))?;
                    obj.insert(name.clone(), sql_to_json(raw));
                }
                Value::Object(obj)
            }
            RowsFormat::Arrays => {
                let mut arr = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let raw: SqlValue = row.get(i).with_context(|| format!("read column {i}"))?;
                    arr.push(sql_to_json(raw));
                }
                Value::Array(arr)
            }
        };
        rows.push(value);
    }
    drop(rows_iter);
    drop(stmt);

    // sqlite3_changes / last_insert_rowid are connection-scoped and NOT reset
    // by read-only statements — reading them here for a SELECT would leak the
    // counters of a previous, unrelated request on this shared connection
    // (monkut/rustyhip#20). Report 0 for read-only statements instead.
    let (rowcount, lastrowid) =
        if readonly { (0, 0) } else { (i64::try_from(conn.changes()).unwrap_or(i64::MAX), conn.last_insert_rowid()) };
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

    /// Regression for monkut/rustyhip#20: a SELECT after an INSERT on the same
    /// (shared, long-lived) connection must not report the INSERT's counters.
    #[tokio::test]
    async fn select_does_not_leak_previous_write_counters() {
        let (_f, db) = empty_db();
        exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        let write = exec_sql(&db, "INSERT INTO fruit (name) VALUES ('apple'), ('peach')").await;
        assert_eq!(write.rowcount, 2);
        assert_eq!(write.lastrowid, 2);
        let read = exec_sql(&db, "SELECT name FROM fruit").await;
        assert_eq!(read.rowcount, 0, "SELECT must report rowcount 0");
        assert_eq!(read.lastrowid, 0, "SELECT must report lastrowid 0");
    }

    #[tokio::test]
    async fn rows_format_arrays_aligns_with_columns() {
        let (_f, db) = empty_db();
        exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        exec_sql(&db, "INSERT INTO fruit (name) VALUES ('apple'), ('peach')").await;
        let out = db
            .exec_durable(
                "SELECT id, name FROM fruit ORDER BY id".into(),
                vec![],
                RowsFormat::Arrays,
                CheckpointMode::Off,
            )
            .await
            .expect("select");
        assert_eq!(out.columns, vec!["id".to_owned(), "name".to_owned()]);
        assert_eq!(out.rows[0], serde_json::json!([1, "apple"]));
        assert_eq!(out.rows[1], serde_json::json!([2, "peach"]));
    }

    /// Regression for monkut/rustyhip#29: JOINs legally produce duplicate
    /// column names. Object-keyed rows collapse duplicates (later column
    /// overwrites earlier); arrays mode must round-trip every value by
    /// position.
    #[tokio::test]
    async fn rows_format_arrays_preserves_duplicate_column_names() {
        let (_f, db) = empty_db();
        let out = db
            .exec_durable("SELECT 1 AS a, 2 AS a".into(), vec![], RowsFormat::Arrays, CheckpointMode::Off)
            .await
            .expect("select");
        assert_eq!(out.columns, vec!["a".to_owned(), "a".to_owned()]);
        assert_eq!(out.rows[0], serde_json::json!([1, 2]), "both duplicate-named columns must survive");
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

    /// WAL sidecar size in bytes (0 when the file doesn't exist yet).
    fn wal_size(db_path: &Path) -> u64 {
        let mut wal = db_path.as_os_str().to_owned();
        wal.push("-wal");
        std::fs::metadata(&wal).map_or(0, |m| m.len())
    }

    fn wal_mode_db() -> (NamedTempFile, SqliteDb) {
        let file = NamedTempFile::new().expect("tempfile");
        let settings = DbSettings { journal_mode: Some(JournalMode::Wal), ..DbSettings::default() };
        let db = SqliteDb::open_with(file.path().to_owned(), None, settings).expect("open");
        (file, db)
    }

    #[tokio::test]
    async fn plain_write_checkpoint_truncates_wal() {
        let (_f, db) = wal_mode_db();
        db.exec_durable("CREATE TABLE t (x INTEGER)".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect("create");
        assert_eq!(wal_size(db.path()), 0, "wal should be truncated after a checkpointed write");
    }

    #[tokio::test]
    async fn readonly_select_does_not_checkpoint() {
        let (_f, db) = wal_mode_db();
        // Write WITHOUT a checkpoint so frames stay in the WAL…
        db.exec("CREATE TABLE t (x INTEGER)".into(), vec![]).await.expect("create");
        let before = wal_size(db.path());
        assert!(before > 0, "un-checkpointed write should leave WAL frames");
        // …then a readonly SELECT with checkpointing enabled must not flush them.
        db.exec_durable("SELECT * FROM t".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect("select");
        assert_eq!(wal_size(db.path()), before, "readonly statement must not checkpoint");
    }

    /// Regression for monkut/rustyhip#17: `sqlite3_stmt_readonly` reports
    /// COMMIT as readonly, so gating the checkpoint on the statement's readonly
    /// flag alone would leave a committed transaction sitting in the local WAL.
    #[tokio::test]
    async fn commit_of_explicit_transaction_is_checkpointed() {
        let (_f, db) = wal_mode_db();
        db.exec_durable("CREATE TABLE t (x INTEGER)".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect("create");
        db.exec_durable("BEGIN".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate).await.expect("begin");
        db.exec_durable("INSERT INTO t VALUES (1)".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect("insert in txn");
        db.exec_durable("COMMIT".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate).await.expect("commit");
        assert_eq!(wal_size(db.path()), 0, "COMMIT must flush the transaction out of the WAL");
        let out = db.exec("SELECT x FROM t".into(), vec![]).await.expect("select");
        assert_eq!(out.rows.len(), 1);
    }

    /// A checkpoint that cannot complete (`busy != 0`) must fail the call —
    /// acking a write whose frames are still local-only would violate the
    /// durability contract (CLAUDE.md).
    #[tokio::test]
    async fn blocked_checkpoint_surfaces_as_checkpoint_error() {
        let (file, db) = wal_mode_db();
        db.exec_durable("CREATE TABLE t (x INTEGER)".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect("create");
        // Second connection holds an open read snapshot, preventing WAL reset.
        let reader = Connection::open(file.path()).expect("open reader");
        reader.execute_batch("BEGIN").expect("begin read txn");
        let _count: i64 = reader.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).expect("acquire read snapshot");
        let err = db
            .exec_durable("INSERT INTO t VALUES (1)".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect_err("checkpoint should report busy");
        assert!(matches!(err, ExecError::Checkpoint(_)), "unexpected error variant: {err:?}");
    }

    #[tokio::test]
    async fn attach_is_denied() {
        let (_f, db) = empty_db();
        let err = db
            .exec("ATTACH DATABASE '/tmp/rustyhip-escape.db' AS x".into(), vec![])
            .await
            .expect_err("ATTACH must be rejected");
        assert!(err.chain().any(|e| e.to_string().contains("not authorized")), "unexpected error: {err:#}");
    }

    #[tokio::test]
    async fn config_altering_pragma_is_denied() {
        let (_f, db) = empty_db();
        for sql in ["PRAGMA journal_mode = DELETE", "PRAGMA synchronous = OFF", "PRAGMA wal_autocheckpoint = 0"] {
            let err = db.exec(sql.into(), vec![]).await.expect_err("value-form pragma must be rejected");
            assert!(err.chain().any(|e| e.to_string().contains("not authorized")), "{sql}: {err:#}");
        }
    }

    #[tokio::test]
    async fn introspection_pragmas_are_allowed() {
        let (_f, db) = empty_db();
        exec_sql(&db, "CREATE TABLE fruit (id INTEGER PRIMARY KEY, name TEXT)").await;
        let out = db.exec("PRAGMA table_info(fruit)".into(), vec![]).await.expect("table_info allowed");
        assert_eq!(out.rows.len(), 2);
        // Read form of a config pragma is harmless and stays available.
        db.exec("PRAGMA journal_mode".into(), vec![]).await.expect("read form allowed");
    }

    /// The internal durability flush must survive the authorizer (it runs
    /// through the same connection).
    #[tokio::test]
    async fn internal_checkpoint_still_works_with_authorizer() {
        let (_f, db) = wal_mode_db();
        db.exec_durable("CREATE TABLE t (x INTEGER)".into(), vec![], RowsFormat::Objects, CheckpointMode::Truncate)
            .await
            .expect("checkpointed write with authorizer installed");
        assert_eq!(wal_size(db.path()), 0);
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
