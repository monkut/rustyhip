# rustyhip local benchmarks

- Version: `0.1.0`
- Generated (JST): `2026-07-04T22:05:55.606914666+09:00`
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
2. **Synchronous S3 checkpoint after every write** — `SqliteDb::exec_durable` (`src/db.rs`)
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
| `health` | 2000 | 2000 | 0 | 0.001 | 0.001 | 0.001 | 0.001 | 0.010 | 0.001 | 1547609 |
| `select_count_aggregate` | 500 | 500 | 0 | 0.008 | 0.010 | 0.011 | 0.013 | 0.127 | 0.010 | 100549 |
| `select_param_by_worker` | 500 | 500 | 0 | 0.028 | 0.032 | 0.038 | 0.040 | 0.113 | 0.032 | 30912 |
| `select_recent_10` | 500 | 500 | 0 | 0.014 | 0.017 | 0.020 | 0.021 | 0.068 | 0.017 | 58202 |
| `select_recent_500` | 100 | 100 | 0 | 0.176 | 0.182 | 0.294 | 0.402 | 0.414 | 0.200 | 4990 |
| `select_wide_1k_objects` | 50 | 50 | 0 | 0.927 | 1.164 | 1.327 | 1.420 | 1.460 | 1.144 | 874 |
| `select_wide_1k_arrays` | 50 | 50 | 0 | 0.488 | 0.614 | 0.723 | 0.789 | 0.808 | 0.608 | 1644 |
| `insert_single` | 300 | 300 | 0 | 1.644 | 2.255 | 3.681 | 4.571 | 8.731 | 2.450 | 408 |
| `insert_batch_10` | 200 | 200 | 0 | 1.611 | 2.191 | 3.137 | 4.063 | 4.449 | 2.314 | 432 |
| `err_bad_json` | 200 | 0 | 200 | 0.002 | 0.002 | 0.002 | 0.003 | 0.022 | 0.002 | 548452 |
| `err_missing_table` | 200 | 0 | 200 | 0.026 | 0.031 | 0.036 | 0.042 | 0.101 | 0.030 | 32949 |
| `err_not_found_route` | 500 | 0 | 500 | 0.001 | 0.001 | 0.001 | 0.001 | 0.004 | 0.001 | 1030263 |

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
