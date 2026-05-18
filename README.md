# rustyhip

RustyHip is a lambda front end providing Sqlite-like database over S3

## 構成

```
リポジトリディレクトリ
│ .gitignore
│ .pre-commit-config.yaml
│ Cargo.toml
│ rust-toolchain.toml
│ rustfmt.toml / clippy.toml / deny.toml / typos.toml
│ justfile
│ LICENSE
│ README.md
│
├── .cargo/config.toml       (alias + build tweaks)
├── .config/nextest.toml     (cargo-nextest profiles)
├── .github/workflows/       (ci.yml + release.yml)
├── data/                    (データ用ディレクトリ)
├── src/
│     lib.rs / main.rs / settings.rs
└── tests/
      integration.rs
```

## Local Development

Rust: `1.85+` (edition `2024`), toolchain pinned via `rust-toolchain.toml`.

> Requires [rustup](https://rustup.rs/) for toolchain management.

### 開発環境のインストール

1. toolchain をインストール (`rust-toolchain.toml` から自動で):

    ```bash
    rustup show
    ```

2. 開発用ツールをインストール:

    ```bash
    # cargo-binstall が入っていれば高速 (prebuilt binary)
    cargo binstall just cargo-nextest cargo-deny cargo-audit cargo-machete typos-cli
    # 無ければ
    cargo install just cargo-nextest cargo-deny cargo-audit cargo-machete typos-cli
    ```

3. `pre-commit` フックを登録 (`cargo fmt` / `clippy` / `typos`):

    ```bash
    pre-commit install
    ```

4. 依存を取得:

    ```bash
    cargo fetch
    ```

### 新パッケージの追加

プロジェクトルートから:

```bash
cargo add {PACKAGE_NAME}
cargo add --dev {PACKAGE_NAME}    # dev-dependency
```

## コードチェックを実行

```bash
just check          # cargo fmt --check && cargo clippy -- -D warnings
just fix            # auto-fix
```

## テストケースを実行

[cargo-nextest](https://nexte.st/) を使用します。テストコードは `src/**` の `#[cfg(test)]` モジュールと `tests/` に置きます。

```bash
just test           # default profile
just test-ci        # CI profile (retries, junit.xml)
just test-doc       # doctests
```

## 依存監査 / セキュリティ

```bash
just audit          # cargo audit + cargo deny check
just unused         # cargo machete (未使用 dependency 検出)
just typos          # typos CLI
```

## パッケージをビルド

```bash
just build          # cargo build --release
```

`vX.Y.Z` タグを push すると GitHub Actions (`release.yml`) が multi-arch (linux / macOS / windows) でビルドし、成果物を GitHub Releases に添付します。`main` への push / PR では `ci.yml` が fmt / clippy / nextest / audit / MSRV チェックを実行します。

## Deployment

rustyhip is an AWS Lambda function. The SAM template is **generated** from
`scripts/generate_template.py` rather than committed — `template.yaml` is
listed in `.gitignore` because it gets regenerated per deploy. The generator
emits only CloudFormation `Parameters` for environment-specific values
(bucket, DB name, auth token); all structural settings (arm64, 512 MB,
30 s timeout, `ReservedConcurrentExecutions: 1`) are baked in.

### Prerequisites

- AWS CLI + valid credentials for the target account
- `sam` CLI (`brew install aws-sam-cli` or equivalent)
- `cargo-lambda` (`cargo binstall cargo-lambda`)
- `uv` (the generator script is `uv run`-able)

### One-time deploy

```bash
# 1. Cross-compile the Lambda (arm64 musl).
just lambda-build

# 2. Generate the SAM template. Optional flags pass through, e.g.
#    `just template-gen -- --architecture x86_64 --memory-mb 1024`.
just template-gen

# 3. Deploy. Replace placeholders with real values. AuthToken must be ≥16 chars.
just template-deploy STACK=my-rustyhip \
    BUCKET=my-s3-bucket \
    DB_NAME=my-app-db \
    AUTH_TOKEN="$(openssl rand -hex 32)"
```

Resource tags can be attached at deploy time via `TAGS=` (space-separated
`Key=Value` pairs), which `sam deploy --tags` consumes verbatim:

```bash
just template-deploy STACK=my-rustyhip BUCKET=my-bucket DB_NAME=my-db \
    AUTH_TOKEN=... TAGS="Env=prod Team=platform"
```

### Stack outputs

After deploy, `sam` prints the API Gateway invoke URL (`ApiEndpoint`). All
requests must include `Authorization: Bearer <AuthToken>`; no anonymous
traffic is accepted in deployed environments.

### Concurrency contract

The template pins `ReservedConcurrentExecutions: 1`. This is **required**
for correctness on the current turbolite VFS — see issue #1 (multi-writer
support) and `CLAUDE.md` (Lambda ephemeral compute architecture). Do not
raise this without first reading both.

## Configuration

All knobs are environment variables read once at Lambda cold-start. Bad
values log a warning and fall back to the unset behavior — they will not
prevent bootstrap.

### Required (deployed)

| Env | Purpose |
|-----|---------|
| `BUCKET` | S3 bucket holding the turbolite-managed DB pages |
| `DB_NAME` | Turbolite prefix (logical DB name) within the bucket |
| `RUSTYHIP_AUTH_TOKEN` | Bearer token every request must present. Anonymous traffic logs a warning at startup. |

### AWS / runtime

| Env | Default | Purpose |
|-----|---------|---------|
| `AWS_REGION` / `REGION` | `ap-northeast-1` | AWS region for S3 |
| `AWS_ENDPOINT_URL` / `AWS_ENDPOINT_URL_S3` | unset | Custom S3 endpoint (floci / MinIO / LocalStack) — triggers path-style addressing |
| `DB_CACHE_DIR` | `/tmp/rustyhip-cache` | Turbolite local page cache directory |
| `ENVIRONMENT` | `development` | Label attached to every log event |
| `LOG_LEVEL` / `RUST_LOG` | `info` | tracing-subscriber filter |
| `LOG_FORMAT` | `json` | `json` (structured) or `pretty` (human-readable) |

### SQLite pragmas (P0)

Applied once at connection open. Unset = SQLite default. The bench
(`results/benchmarks.md`) shows `insert_single` is fsync-bound at 2.6 ms
p50 with defaults — `synchronous` and `journal_mode` are the biggest
levers if you can trade durability for throughput.

| Env | Maps to |
|-----|---------|
| `RUSTYHIP_SYNCHRONOUS` | `PRAGMA synchronous` — `full` / `normal` / `off` / `extra` |
| `RUSTYHIP_JOURNAL_MODE` | `PRAGMA journal_mode` — `delete` / `truncate` / `persist` / `memory` / `wal` / `off` |
| `RUSTYHIP_PAGE_CACHE_KB` | `PRAGMA cache_size = -N` (size in KB) |
| `RUSTYHIP_MMAP_SIZE` | `PRAGMA mmap_size` (bytes) |
| `RUSTYHIP_TEMP_STORE` | `PRAGMA temp_store` — `default` / `file` / `memory` |
| `RUSTYHIP_BUSY_TIMEOUT_MS` | `PRAGMA busy_timeout` (ms) |

### Request shaping (P0/P1)

| Env | Default | Purpose |
|-----|---------|---------|
| `RUSTYHIP_MAX_ROWS` | unset (no cap) | Hard limit on rows returned per `/sql` call. Exceeding the cap returns a clean error rather than materializing the full response. |
| `RUSTYHIP_QUERY_TIMEOUT_MS` | unset (no timeout) | Wall-clock budget per statement, enforced via `Connection::progress_handler`. Surfaced as `"query timeout exceeded Nms"`. |
| `RUSTYHIP_MAX_BODY_BYTES` | unset (Lambda's 6 MB ceiling applies) | Pre-parse `/sql` body size cap. Oversized requests get **413 Payload Too Large** with `RUSTYHIP_E_VALIDATION`. |

### Durability override (P1) — read CLAUDE.md before changing

| Env | Default | Purpose |
|-----|---------|---------|
| `RUSTYHIP_CHECKPOINT_MODE` | `truncate` | Post-write checkpoint mode: `truncate` / `restart` / `full` / `passive` / `off`. **`truncate` is the only Lambda-safe value.** Bootstrap logs a `warn!` when overridden. |

### Turbolite (inherited, set independently of rustyhip)

The turbolite VFS reads many of its own knobs from env directly. The
notable ones for tuning a deployed Lambda:

| Env | Default | Purpose |
|-----|---------|---------|
| `TURBOLITE_MEM_CACHE_BUDGET` | `64MB` | In-memory page cache budget |
| `TURBOLITE_CACHE_LIMIT` | unlimited | Sub-chunk eviction budget on `/tmp` |
| `TURBOLITE_PREFETCH_THREADS` | `cpus+1` | Prefetch worker pool size |
| `TURBOLITE_EVICT_ON_CHECKPOINT` | `false` | Evict data tier after checkpoint upload |

See `turbolite::tiered::config` for the full list.

## Testing as a downstream consumer

Three options for projects that talk to rustyhip via `POST /sql`, in
increasing order of fidelity:

### 1. Mock the HTTP layer in your own tests (fastest, no rustyhip)

If your test only needs to exercise *your* code's SQL generation and JSON
parsing, mock the `/sql` endpoint with a fixture-driven HTTP stub. This is
the right choice for unit tests where rustyhip's behavior is not the
subject under test.

### 2. Run rustyhip locally against floci (recommended for integration tests)

The full Lambda is reproducible offline with no AWS account needed:

```bash
# Terminal 1 — bring up floci (local S3 emulator) and seed the bucket.
just floci-up
just floci-seed       # idempotent — creates `rustyhip-dev` bucket

# Terminal 2 — run rustyhip via cargo-lambda watch on http://localhost:9000.
just rustyhip-dev
```

Point your test client at `http://localhost:9000`. The end-to-end Django
reproducer `just verify-django-rustyhip` is a working example.

For unit tests that should be skippable in CI without floci, gate on a
`MEETS_RUSTYHIP_BASE_URL` env (or similar). When unset, skip; when set,
run against either the local floci-backed instance or a shared dev stack.

### 3. Run against a deployed dev stack (highest fidelity, requires creds)

Use the `scripts/loadtest_rustyhip.py` harness pattern:

```bash
uv run scripts/loadtest_rustyhip.py \
    --url https://abc123.execute-api.ap-northeast-1.amazonaws.com \
    --token "$RUSTYHIP_AUTH_TOKEN" \
    --duration-s 10
```

Required env: `RUSTYHIP_AUTH_TOKEN`. The harness creates and tears down
its own `loadtest_events` table so it cannot disturb production data.

### Local micro-benchmarks (no S3 round-trips)

For latency-floor measurements of the handler itself, run:

```bash
just bench          # cargo run --release --example bench
```

Writes `results/{VERSION}-benchmark-results.jsonl` + `results/benchmarks.md`.
See `results/benchmarks.md` for a description of what's measured (and
what's not — the harness deliberately excludes the turbolite S3 layer).
