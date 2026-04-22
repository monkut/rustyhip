# Task runner — https://just.systems
# Run `just` with no args to list recipes.

set shell := ["bash", "-cu"]
set dotenv-load

default:
    @just --list

# Format + lint (read-only)
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings

# Apply auto-fixes (formatter + clippy)
fix:
    cargo fmt --all
    cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged

# Run tests (cargo-nextest, default profile)
test:
    cargo nextest run --all-features

# Run tests with CI profile (retries, junit.xml output)
test-ci:
    mkdir -p test-reports
    cargo nextest run --all-features --profile ci
    cp target/nextest/ci/junit.xml test-reports/junit.xml || true

# Run integration + doc tests (nextest does not execute doctests)
test-doc:
    cargo test --doc --all-features

# Audit dependencies for vulnerabilities + license/ban policy
audit:
    cargo audit
    cargo deny check

# Detect unused dependencies declared in Cargo.toml
unused:
    cargo machete

# Spellcheck
typos:
    typos

# Build release binary/library
build:
    cargo build --release --all-features

# Generate docs
doc:
    cargo doc --no-deps --all-features

# Update dependencies (respects Cargo.lock semver)
update:
    cargo update

# Everything the CI should run before merge
all: check test audit typos unused

# ---- Floci (local S3 emulator; https://github.com/floci-io/floci) ----
# Requires docker. Brings up S3 on http://localhost:4566. rustyhip itself runs
# natively via `cargo lambda watch` — floci handles S3 only.

# Start floci and wait until healthy.
floci-up:
    docker compose up -d floci
    @echo "Waiting for floci..."
    @timeout 60 sh -c 'until curl -sf http://localhost:4566/ > /dev/null 2>&1; do sleep 1; done'
    @echo "Floci ready (http://localhost:4566)."

# Stop + remove the floci container (state is lost in `memory` mode — re-seed after restart).
floci-down:
    docker compose down

# Create the bucket. Turbolite creates its own page objects on first write, so
# there's nothing to pre-upload. Idempotent.
floci-seed BUCKET="rustyhip-dev":
    #!/usr/bin/env bash
    set -euo pipefail
    export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 AWS_ENDPOINT_URL=http://localhost:4566
    aws s3api create-bucket --bucket {{BUCKET}} >/dev/null 2>&1 || true
    echo "Bucket ready: s3://{{BUCKET}}/"

# Run rustyhip against floci via cargo lambda watch (http://localhost:9000).
# Clears the local turbolite cache dir so cold-start behaves like a fresh Lambda.
rustyhip-dev BUCKET="rustyhip-dev" DB_NAME="rustyhip":
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf /tmp/rustyhip-cache
    export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
    export AWS_ENDPOINT_URL=http://localhost:4566
    export BUCKET={{BUCKET}} DB_NAME={{DB_NAME}}
    export LOG_FORMAT=pretty LOG_LEVEL=info ENVIRONMENT=development
    cargo lambda watch

# End-to-end verification that django-rustyhip routes Django ORM traffic to
# rustyhip, which in turn shards the DB across turbolite-managed objects in S3
# (floci). Starts rustyhip in the background, runs `manage.py migrate` + a
# CRUD round-trip against it, then lists the turbolite objects to prove the
# DB is *not* a single sqlite3 file. Fails if a local db.sqlite3 appears,
# which would mean the django-rustyhip backend silently fell back to sqlite3.
#
# First run cold-starts `cargo lambda watch` (~2-5 min). Subsequent runs reuse
# target/ and finish in under a minute. Logs: /tmp/rustyhip-verify.log.
verify-django-rustyhip BUCKET="rustyhip-dev" DB_NAME="rustyhip":
    #!/usr/bin/env bash
    set -euo pipefail

    echo "[1/6] Ensuring floci + bucket..."
    curl -sf http://localhost:4566/ >/dev/null 2>&1 || just floci-up
    just floci-seed {{BUCKET}} >/dev/null

    if curl -sf http://localhost:9000/health 2>/dev/null | grep -q '"status":"ok"'; then
        echo "ERROR: something already answering on :9000 — stop it so we can start a clean rustyhip." >&2
        exit 1
    fi

    export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
    export AWS_ENDPOINT_URL=http://localhost:4566
    export BUCKET={{BUCKET}} DB_NAME={{DB_NAME}}
    export LOG_FORMAT=pretty LOG_LEVEL=info ENVIRONMENT=development

    echo "[2/6] Wiping stale state (local sqlite + turbolite cache + S3 prefix) for a fresh run..."
    rm -f tools/sample-django/db.sqlite3
    rm -rf /tmp/rustyhip-cache
    aws s3 rm s3://{{BUCKET}}/{{DB_NAME}}/ --recursive >/dev/null 2>&1 || true

    echo "[3/6] Starting rustyhip (cargo lambda watch → /tmp/rustyhip-verify.log)..."
    cargo lambda watch >/tmp/rustyhip-verify.log 2>&1 &
    RUSTYHIP_PID=$!
    cleanup() {
        kill "$RUSTYHIP_PID" 2>/dev/null || true
        wait "$RUSTYHIP_PID" 2>/dev/null || true
    }
    trap cleanup EXIT
    timeout 600 bash -c 'until curl -sf http://localhost:9000/health 2>/dev/null | grep -q "\"status\":\"ok\""; do sleep 2; done'
    echo "       rustyhip ready on :9000 (pid $RUSTYHIP_PID)"

    echo "[4/6] Django migrate via django-rustyhip backend..."
    pushd tools/sample-django >/dev/null
    uv sync -q
    RUSTYHIP_ENDPOINT=http://localhost:9000 uv run python manage.py migrate --no-input

    echo "[5/6] CRUD round-trip (INSERT + COUNT) via rustyhip..."
    RUSTYHIP_ENDPOINT=http://localhost:9000 uv run python manage.py shell -c "
    from django.contrib.auth.models import User
    User.objects.get_or_create(username='rustyhip-smoke')
    print(f'user count = {User.objects.count()}')
    "
    popd >/dev/null

    echo "[6/6] Verifying S3 representation (expect multiple turbolite objects, no single sqlite file)..."
    echo "       Objects under s3://{{BUCKET}}/{{DB_NAME}}/:"
    aws s3 ls s3://{{BUCKET}}/{{DB_NAME}}/ --recursive | sed 's/^/         /'
    n=$(aws s3 ls s3://{{BUCKET}}/{{DB_NAME}}/ --recursive | wc -l)
    if [ "$n" -lt 2 ]; then
        echo "FAIL: expected multiple turbolite objects, found $n" >&2
        exit 1
    fi
    if [ -f tools/sample-django/db.sqlite3 ]; then
        echo "FAIL: tools/sample-django/db.sqlite3 was created — django-rustyhip did not route to rustyhip" >&2
        exit 1
    fi
    echo ""
    echo "PASS: django-rustyhip → rustyhip → turbolite wrote $n objects to s3://{{BUCKET}}/{{DB_NAME}}/. No local sqlite file."

# ---- CAS integration test ----
# Proves two concurrent turbolite writers against the same floci prefix can't
# both silently commit — the second's checkpoint fails with the CAS
# precondition error (handle.rs::sync → commit_manifest). Requires floci up.
cas-test BUCKET="rustyhip-cas-test":
    #!/usr/bin/env bash
    set -euo pipefail
    curl -sf http://localhost:4566/ >/dev/null 2>&1 || just floci-up
    just floci-seed {{BUCKET}} >/dev/null
    export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
    export AWS_ENDPOINT_URL=http://localhost:4566
    cargo test --test cas_conflict -- --ignored --nocapture

# ---- Load testing ----
# Drives a mixed read/write workload against a rustyhip deployment and records
# latency percentiles + QPS. Baseline for the RCE=1 saturation trigger tracked
# in github.com/monkut/rustyhip/issues/1. Works against both `just rustyhip-dev`
# (default) and a deployed API Gateway URL.
#
# Example — local floci:
#   just loadtest
# Example — deployed Lambda:
#   just loadtest URL=https://xyz.execute-api.ap-northeast-1.amazonaws.com \
#                 TOKEN=$RUSTYHIP_AUTH_TOKEN DURATION_S=60 CONCURRENCY=8
loadtest URL="http://localhost:9000" TOKEN="" DURATION_S="30" CONCURRENCY="4" WRITE_RATIO="0.5":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p data
    ts=$(date -u +%Y%m%d-%H%M%S)
    out="data/loadtest-${ts}.json"
    args=(--url {{URL}} --duration-s {{DURATION_S}} --concurrency {{CONCURRENCY}} --write-ratio {{WRITE_RATIO}} --output "$out")
    if [ -n "{{TOKEN}}" ]; then
        args+=(--token {{TOKEN}})
    fi
    uv run scripts/loadtest_rustyhip.py "${args[@]}"
    echo "Report: $out"

# ---- Lambda (cargo-lambda) ----
# Install once:  cargo binstall cargo-lambda  (or cargo install cargo-lambda)

# Cross-compile the Lambda binary (arm64 / aarch64-unknown-linux-musl)
lambda-build:
    cargo lambda build --release --arm64

# Local dev server — emulates the Lambda runtime on http://localhost:9000
lambda-watch:
    cargo lambda watch

# Deploy to AWS (requires AWS creds + an IAM role; pass via env or flags).
# Env vars consumed by the binary at runtime: BUCKET, DB_KEY, optional DB_CACHE_PATH.
lambda-deploy FUNCTION="rhp-rustyhip":
    cargo lambda deploy {{FUNCTION}}

# Generate the SAM CloudFormation template via the uv-runnable script.
# Additional flags pass through, e.g. `just template-gen -- --architecture x86_64`.
template-gen *FLAGS:
    uv run scripts/generate_template.py --output template.yaml {{FLAGS}}

# Deploy via SAM (requires `sam` CLI + AWS creds + Bucket/DbName/AuthToken overrides).
template-deploy STACK="rhp-rustyhip" BUCKET="" DB_NAME="" AUTH_TOKEN="":
    sam deploy --template-file template.yaml \
        --stack-name {{STACK}} \
        --capabilities CAPABILITY_IAM \
        --resolve-s3 \
        --parameter-overrides BucketName={{BUCKET}} DbName={{DB_NAME}} AuthToken={{AUTH_TOKEN}}

# ---- AWS helpers ----
create-project-bucket:
    aws s3 mb s3://rustyhip --region ${AWS_DEFAULT_REGION:-ap-northeast-1}

get-project-data:
    aws s3 cp s3://rustyhip/ ./data --recursive

put-project-data:
    aws s3 cp ./data s3://rustyhip/ --recursive
