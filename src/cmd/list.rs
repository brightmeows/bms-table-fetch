//! Fetch table lists from configured sources and write them to disk.

use std::path::PathBuf;

use anyhow::Result;
use bms_table::{BmsTableInfo, fetch::reqwest::Fetcher};
use log::info;
use tokio::fs;

use crate::config::list::load_list_config;

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
/// # Errors
///
/// Returns an error if fetching table lists or writing to disk fails.
pub async fn run_list(args: &Args) -> Result<()> {
    let config = load_list_config(&args.config).await?;

    let lists_dir = &args.output_dir;
    fs::create_dir_all(lists_dir).await?;

    let fetcher = Fetcher::lenient()?;

    for idx in &config.source {
        info!("Fetching table list from: {} ({})", idx.name, idx.url);
        let fetched_list = fetcher.fetch_table_list(idx.url.as_str()).await?;
        let infos: Vec<BmsTableInfo> = fetched_list.tables;

        let file_path = lists_dir.join(format!("{}.json", idx.name));
        let serialized = serde_json::to_string_pretty(&infos)?;
        fs::write(file_path, serialized).await?;

        info!("Saved {} tables from {}", infos.len(), idx.name);
    }

    info!("List fetch completed.");
    Ok(())
}
