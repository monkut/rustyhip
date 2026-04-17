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
