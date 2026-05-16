//! Local micro-benchmarks for rustyhip's HTTP handler against the default
//! `SQLite` VFS (local file — no S3, no turbolite). Measures the CPU/SQLite
//! floor; production adds turbolite page reads + a synchronous S3 checkpoint
//! after every non-readonly /sql call (see `src/handler.rs:152-169`).
//!
//! Run from project root:
//!
//! ```bash
//! cargo run --release --example bench
//! ```
//!
//! Outputs:
//! - `results/{VERSION}-benchmark-results.jsonl` — one record per iteration
//! - `results/benchmarks.md` — human-readable summary

#![allow(clippy::pedantic, clippy::nursery, clippy::cargo)]

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use lambda_http::http::Request as HttpRequest;
use lambda_http::{Body, Request};
use rustyhip::VERSION;
use rustyhip::db::SqliteDb;
use rustyhip::handler::handle;
use rustyhip::state::AppState;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

#[derive(Clone, Copy)]
struct Op {
    name: &'static str,
    method: &'static str,
    path: &'static str,
    body: &'static str,
    iterations: usize,
}

fn build_request(method: &str, path: &str, body: &str) -> Request {
    HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .expect("build request")
}

async fn run_op(state: Arc<AppState>, op: Op) -> Vec<(u64, u16)> {
    let mut samples = Vec::with_capacity(op.iterations);
    for _ in 0..op.iterations {
        let req = build_request(op.method, op.path, op.body);
        let t0 = Instant::now();
        let resp = handle(state.clone(), req).await.expect("handler");
        let ns: u64 = t0.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
        samples.push((ns, resp.status().as_u16()));
    }
    samples
}

async fn seed(state: &AppState, rows: i64) -> Result<()> {
    let _ = state.db.exec("DROP TABLE IF EXISTS bench".into(), vec![]).await;
    state
        .db
        .exec("CREATE TABLE bench (id INTEGER PRIMARY KEY, ts INTEGER, worker_id INTEGER, payload TEXT)".into(), vec![])
        .await
        .context("create bench table")?;
    state.db.exec("CREATE INDEX idx_bench_worker ON bench(worker_id)".into(), vec![]).await.context("create index")?;
    let batch_size = 100_i64;
    let batches = rows / batch_size;
    for batch in 0..batches {
        let mut sql = String::from("INSERT INTO bench (ts, worker_id, payload) VALUES ");
        for i in 0..batch_size {
            if i > 0 {
                sql.push(',');
            }
            let id = batch * batch_size + i;
            sql.push_str(&format!("({}, {}, 'payload-b{batch}-i{i}')", 1_700_000_000 + id, id % 8));
        }
        state.db.exec(sql, vec![]).await.context("seed insert")?;
    }
    Ok(())
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let k = (sorted.len() - 1) as f64 * (pct / 100.0);
    let lo = k.floor() as usize;
    let hi = k.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (k - lo as f64)
}

fn jst_now() -> String {
    let jst = UtcOffset::from_hms(9, 0, 0).expect("+09:00 is a valid offset");
    OffsetDateTime::now_utc().to_offset(jst).format(&Rfc3339).unwrap_or_else(|_| "unknown".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let out_dir = PathBuf::from("results");
    fs::create_dir_all(&out_dir).context("create results dir")?;
    let jsonl_path = out_dir.join(format!("{VERSION}-benchmark-results.jsonl"));
    let md_path = out_dir.join("benchmarks.md");

    let db_path = PathBuf::from(format!("/tmp/rustyhip-bench-{}.db", std::process::id()));
    if db_path.exists() {
        fs::remove_file(&db_path).ok();
    }
    let db = Arc::new(SqliteDb::open(&db_path).context("open bench db")?);
    let state = Arc::new(AppState::new(db, None));

    let seed_rows: i64 = 1_000;
    seed(&state, seed_rows).await?;

    let ops: Vec<Op> = vec![
        Op { name: "health", method: "GET", path: "/health", body: "", iterations: 2000 },
        Op {
            name: "select_count_aggregate",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"SELECT COUNT(*) AS n FROM bench"}"#,
            iterations: 500,
        },
        Op {
            name: "select_param_by_worker",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"SELECT id, ts FROM bench WHERE worker_id = ? LIMIT 50","params":[3]}"#,
            iterations: 500,
        },
        Op {
            name: "select_recent_10",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"SELECT id, ts, worker_id FROM bench ORDER BY id DESC LIMIT 10"}"#,
            iterations: 500,
        },
        Op {
            name: "select_recent_500",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"SELECT id, ts, worker_id FROM bench ORDER BY id DESC LIMIT 500"}"#,
            iterations: 100,
        },
        Op {
            name: "insert_single",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"INSERT INTO bench (ts, worker_id, payload) VALUES (?, ?, ?)","params":[1700099999,0,"single-row-payload"]}"#,
            iterations: 300,
        },
        Op {
            name: "insert_batch_10",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"INSERT INTO bench (ts, worker_id, payload) VALUES (1,1,'a'),(2,2,'b'),(3,3,'c'),(4,4,'d'),(5,5,'e'),(6,6,'f'),(7,7,'g'),(8,0,'h'),(9,1,'i'),(10,2,'j')"}"#,
            iterations: 200,
        },
        Op { name: "err_bad_json", method: "POST", path: "/sql", body: "not valid json", iterations: 200 },
        Op {
            name: "err_missing_table",
            method: "POST",
            path: "/sql",
            body: r#"{"sql":"SELECT * FROM does_not_exist"}"#,
            iterations: 200,
        },
        Op { name: "err_not_found_route", method: "GET", path: "/no-such-route", body: "", iterations: 500 },
    ];

    eprintln!("rustyhip bench v{VERSION} — seeded {seed_rows} rows, warming up...");
    for _ in 0..200u32 {
        handle(state.clone(), build_request("GET", "/health", "")).await.expect("warm health");
    }
    for _ in 0..100u32 {
        handle(state.clone(), build_request("POST", "/sql", r#"{"sql":"SELECT 1 AS one"}"#))
            .await
            .expect("warm select 1");
    }

    let mut jsonl = BufWriter::new(File::create(&jsonl_path).context("create jsonl")?);
    let unix_ms_start: u64 =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0).try_into().unwrap_or(0);

    let mut rows_md: Vec<Value> = Vec::new();
    for op in &ops {
        eprintln!("  - {} (n={})", op.name, op.iterations);
        let samples = run_op(state.clone(), *op).await;

        for (i, (ns, status)) in samples.iter().enumerate() {
            let line = json!({
                "version": VERSION,
                "op": op.name,
                "iteration": i,
                "elapsed_ns": ns,
                "elapsed_ms": (*ns as f64) / 1_000_000.0,
                "status": status,
                "method": op.method,
                "path": op.path,
                "unix_ms_start": unix_ms_start,
            });
            writeln!(jsonl, "{line}").context("write jsonl")?;
        }

        let mut latencies_ms: Vec<f64> = samples.iter().map(|(ns, _)| *ns as f64 / 1_000_000.0).collect();
        latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = latencies_ms.len();
        let total_ms: f64 = latencies_ms.iter().sum();
        let mean_ms = if n > 0 { total_ms / n as f64 } else { 0.0 };
        let ok = samples.iter().filter(|(_, s)| *s < 400).count();
        let throughput = if total_ms > 0.0 { (n as f64) / (total_ms / 1000.0) } else { 0.0 };

        rows_md.push(json!({
            "op": op.name,
            "iterations": n,
            "ok": ok,
            "errors": n - ok,
            "min_ms": latencies_ms.first().copied().unwrap_or(0.0),
            "p50_ms": percentile(&latencies_ms, 50.0),
            "p95_ms": percentile(&latencies_ms, 95.0),
            "p99_ms": percentile(&latencies_ms, 99.0),
            "max_ms": latencies_ms.last().copied().unwrap_or(0.0),
            "mean_ms": mean_ms,
            "ops_per_s": throughput,
        }));
    }

    jsonl.flush().context("flush jsonl")?;

    let mut md = BufWriter::new(File::create(&md_path).context("create md")?);
    writeln!(md, "# rustyhip local benchmarks")?;
    writeln!(md)?;
    writeln!(md, "- Version: `{VERSION}`")?;
    writeln!(md, "- Generated (JST): `{}`", jst_now())?;
    writeln!(md, "- Raw samples: [`{VERSION}-benchmark-results.jsonl`](./{VERSION}-benchmark-results.jsonl)")?;
    writeln!(md, "- Reproduce: `cargo run --release --example bench`")?;
    writeln!(md)?;
    writeln!(md, "## What this measures")?;
    writeln!(md)?;
    writeln!(md, "Single-process, single-thread, **default `SQLite` VFS** (local file under `/tmp`). The")?;
    writeln!(md, "full Lambda HTTP path is exercised end-to-end:")?;
    writeln!(md)?;
    writeln!(md, "    lambda_http::Request → handler::handle → SqliteDb::exec → JSON response")?;
    writeln!(md)?;
    writeln!(md, "Setup: a `bench` table is created with an index on `worker_id`, then seeded with")?;
    writeln!(md, "`{seed_rows}` rows of synthetic data before any timing starts. A short warm-up of 200")?;
    writeln!(md, "`/health` + 100 trivial `SELECT 1` requests is run (untimed) to settle tokio + the")?;
    writeln!(md, "SQLite page cache.")?;
    writeln!(md)?;
    writeln!(md, "## What this does NOT measure")?;
    writeln!(md)?;
    writeln!(md, "Production rustyhip runs on AWS Lambda and adds three latency sources that are")?;
    writeln!(md, "absent here:")?;
    writeln!(md)?;
    writeln!(md, "1. **turbolite tiered VFS page reads** — cold pages fetch from S3; warm pages hit")?;
    writeln!(md, "   the local `/tmp` page cache.")?;
    writeln!(md, "2. **Synchronous S3 checkpoint after every write** — `src/handler.rs:152-169`")?;
    writeln!(md, "   issues `PRAGMA wal_checkpoint(TRUNCATE)` on every non-readonly /sql call so the")?;
    writeln!(md, "   canonical state lands in S3 *before* the response returns. Expect inserts to be")?;
    writeln!(md, "   substantially slower in Lambda than the numbers below.")?;
    writeln!(md, "3. **API Gateway + Lambda invocation overhead** — TLS, cold starts, and Lambda's")?;
    writeln!(md, "   own request plumbing.")?;
    writeln!(md)?;
    writeln!(md, "Treat the numbers here as the **CPU + SQLite floor**. For end-to-end measurements")?;
    writeln!(md, "against a deployed (or floci-emulated) endpoint use `scripts/loadtest_rustyhip.py`")?;
    writeln!(md, "(see `just loadtest`).")?;
    writeln!(md)?;
    writeln!(md, "## Results")?;
    writeln!(md)?;
    writeln!(md, "| op | n | ok | err | min ms | p50 ms | p95 ms | p99 ms | max ms | mean ms | ops/s |")?;
    writeln!(md, "|----|---:|---:|---:|-------:|-------:|-------:|-------:|-------:|--------:|------:|")?;
    for s in &rows_md {
        writeln!(
            md,
            "| `{}` | {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.0} |",
            s["op"].as_str().unwrap_or("?"),
            s["iterations"].as_u64().unwrap_or(0),
            s["ok"].as_u64().unwrap_or(0),
            s["errors"].as_u64().unwrap_or(0),
            s["min_ms"].as_f64().unwrap_or(0.0),
            s["p50_ms"].as_f64().unwrap_or(0.0),
            s["p95_ms"].as_f64().unwrap_or(0.0),
            s["p99_ms"].as_f64().unwrap_or(0.0),
            s["max_ms"].as_f64().unwrap_or(0.0),
            s["mean_ms"].as_f64().unwrap_or(0.0),
            s["ops_per_s"].as_f64().unwrap_or(0.0),
        )?;
    }
    writeln!(md)?;
    writeln!(md, "## Op definitions")?;
    writeln!(md)?;
    writeln!(md, "| op | description |")?;
    writeln!(md, "|----|-------------|")?;
    writeln!(md, "| `health` | `GET /health` — handler routing + JSON serialization, no DB. |")?;
    writeln!(md, "| `select_count_aggregate` | `SELECT COUNT(*) AS n FROM bench` — full-table aggregate. |")?;
    writeln!(
        md,
        "| `select_param_by_worker` | Index-backed parameterized `SELECT ... WHERE worker_id = ? LIMIT 50`. |"
    )?;
    writeln!(md, "| `select_recent_10` | `ORDER BY id DESC LIMIT 10` — small ordered result set. |")?;
    writeln!(
        md,
        "| `select_recent_500` | `ORDER BY id DESC LIMIT 500` — half the seed table; row-serialization heavy. |"
    )?;
    writeln!(md, "| `insert_single` | Parameterized 1-row `INSERT`. Local-file fsync only (no S3 checkpoint). |")?;
    writeln!(md, "| `insert_batch_10` | 10-row `VALUES (...),(...)` insert. |")?;
    writeln!(md, "| `err_bad_json` | Invalid request body — exercises the 400 / `RUSTYHIP_E_VALIDATION` path. |")?;
    writeln!(md, "| `err_missing_table` | Valid JSON, unknown table — exercises 400 / `RUSTYHIP_E_SQL`. |")?;
    writeln!(md, "| `err_not_found_route` | `GET /no-such-route` — 404 / `RUSTYHIP_E_NOT_FOUND`. |")?;
    writeln!(md)?;
    md.flush().context("flush md")?;

    fs::remove_file(&db_path).ok();
    eprintln!("wrote {}", jsonl_path.display());
    eprintln!("wrote {}", md_path.display());
    Ok(())
}
