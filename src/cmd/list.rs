//! Fetch table lists from configured sources and write them to disk.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::list::load_list_config;
use crate::fetch::fetch_list_sources;

/// CLI arguments for the list subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Path to configuration file
    #[arg(long, default_value = "config/list.toml")]
    pub config: PathBuf,

    /// Output directory for list JSON files
    #[arg(long, default_value = "lists")]
    pub output_dir: PathBuf,
}

/// Fetch table lists from configured sources and save as unified JSON.
///
/// On individual source failure, logs a warning and preserves cached files.
///
/// # Errors
///
/// Returns an error if reading the list config fails.
pub async fn run_list(args: &Args) -> Result<()> {
    let config = load_list_config(&args.config).await?;
    fetch_list_sources(&config.source, &args.output_dir).await
}
