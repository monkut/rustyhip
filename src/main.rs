//! CLI entry point for rustyhip.

use rustyhip::{greet, settings};

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "rustyhip", version, about = "RustyHip is a lambda front end providing Sqlite-like database over S3")]
struct Cli {
    /// Input file to process.
    #[arg(long)]
    filepath: Option<PathBuf>,

    /// Output directory.
    #[arg(long)]
    directory: Option<PathBuf>,
}

fn main() -> Result<()> {
    settings::init_logging();
    let cli = Cli::parse();
    info!(?cli, "parsed CLI arguments");

    if let Some(path) = &cli.filepath {
        let meta = std::fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
        anyhow::ensure!(meta.is_file(), "--filepath must point to a file: {}", path.display());
    }
    if let Some(dir) = &cli.directory {
        let meta = std::fs::metadata(dir).with_context(|| format!("failed to stat {}", dir.display()))?;
        anyhow::ensure!(meta.is_dir(), "--directory must point to a directory: {}", dir.display());
    }

    println!("{}", greet("rustyhip"));
    Ok(())
}
