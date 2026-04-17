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

# ---- AWS helpers ----
create-project-bucket:
    aws s3 mb s3://rustyhip --region ${AWS_DEFAULT_REGION:-ap-northeast-1}

get-project-data:
    aws s3 cp s3://rustyhip/ ./data --recursive

put-project-data:
    aws s3 cp ./data s3://rustyhip/ --recursive
