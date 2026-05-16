# rustyhip local benchmarks

- Version: `0.1.0`
- Generated (JST): `2026-05-16T10:33:11.181997889+09:00`
- Raw samples: [`0.1.0-benchmark-results.jsonl`](./0.1.0-benchmark-results.jsonl)
- Reproduce: `cargo run --release --example bench`

## What this measures

Single-process, single-thread, **default `SQLite` VFS** (local file under `/tmp`). The
full Lambda HTTP path is exercised end-to-end:

    lambda_http::Request → handler::handle → SqliteDb::exec → JSON response

Setup: a `bench` table is created with an index on `worker_id`, then seeded with
`1000` rows of synthetic data before any timing starts. A short warm-up of 200
`/health` + 100 trivial `SELECT 1` requests is run (untimed) to settle tokio + the
SQLite page cache.

## What this does NOT measure

Production rustyhip runs on AWS Lambda and adds three latency sources that are
absent here:

1. **turbolite tiered VFS page reads** — cold pages fetch from S3; warm pages hit
   the local `/tmp` page cache.
2. **Synchronous S3 checkpoint after every write** — `src/handler.rs:152-169`
   issues `PRAGMA wal_checkpoint(TRUNCATE)` on every non-readonly /sql call so the
   canonical state lands in S3 *before* the response returns. Expect inserts to be
   substantially slower in Lambda than the numbers below.
3. **API Gateway + Lambda invocation overhead** — TLS, cold starts, and Lambda's
   own request plumbing.

Treat the numbers here as the **CPU + SQLite floor**. For end-to-end measurements
against a deployed (or floci-emulated) endpoint use `scripts/loadtest_rustyhip.py`
(see `just loadtest`).

## Results

| op | n | ok | err | min ms | p50 ms | p95 ms | p99 ms | max ms | mean ms | ops/s |
|----|---:|---:|---:|-------:|-------:|-------:|-------:|-------:|--------:|------:|
| `health` | 2000 | 2000 | 0 | 0.001 | 0.001 | 0.001 | 0.001 | 0.009 | 0.001 | 1226169 |
| `select_count_aggregate` | 500 | 500 | 0 | 0.016 | 0.031 | 0.042 | 0.102 | 0.120 | 0.032 | 31650 |
| `select_param_by_worker` | 500 | 500 | 0 | 0.035 | 0.078 | 0.134 | 0.186 | 0.275 | 0.081 | 12314 |
| `select_recent_10` | 500 | 500 | 0 | 0.017 | 0.033 | 0.043 | 0.046 | 0.122 | 0.029 | 34007 |
| `select_recent_500` | 100 | 100 | 0 | 0.166 | 0.254 | 0.355 | 0.451 | 0.476 | 0.256 | 3900 |
| `insert_single` | 300 | 300 | 0 | 1.474 | 2.607 | 3.663 | 4.532 | 5.816 | 2.673 | 374 |
| `insert_batch_10` | 200 | 200 | 0 | 2.102 | 3.105 | 4.325 | 4.996 | 10.742 | 3.211 | 311 |
| `err_bad_json` | 200 | 0 | 200 | 0.002 | 0.002 | 0.002 | 0.003 | 0.019 | 0.002 | 511461 |
| `err_missing_table` | 200 | 0 | 200 | 0.028 | 0.042 | 0.096 | 0.139 | 0.173 | 0.043 | 23466 |
| `err_not_found_route` | 500 | 0 | 500 | 0.001 | 0.001 | 0.001 | 0.002 | 0.047 | 0.001 | 847341 |

## Op definitions

| op | description |
|----|-------------|
| `health` | `GET /health` — handler routing + JSON serialization, no DB. |
| `select_count_aggregate` | `SELECT COUNT(*) AS n FROM bench` — full-table aggregate. |
| `select_param_by_worker` | Index-backed parameterized `SELECT ... WHERE worker_id = ? LIMIT 50`. |
| `select_recent_10` | `ORDER BY id DESC LIMIT 10` — small ordered result set. |
| `select_recent_500` | `ORDER BY id DESC LIMIT 500` — half the seed table; row-serialization heavy. |
| `insert_single` | Parameterized 1-row `INSERT`. Local-file fsync only (no S3 checkpoint). |
| `insert_batch_10` | 10-row `VALUES (...),(...)` insert. |
| `err_bad_json` | Invalid request body — exercises the 400 / `RUSTYHIP_E_VALIDATION` path. |
| `err_missing_table` | Valid JSON, unknown table — exercises 400 / `RUSTYHIP_E_SQL`. |
| `err_not_found_route` | `GET /no-such-route` — 404 / `RUSTYHIP_E_NOT_FOUND`. |
