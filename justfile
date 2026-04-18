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
