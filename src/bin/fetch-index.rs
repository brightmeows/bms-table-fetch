use std::path::PathBuf;

use anyhow::Result;
use bms_table::{BmsTableInfo, fetch::reqwest::Fetcher};
use clap::Parser;
use log::info;
use tokio::fs;

use bms_table_mirror::config::index::load_index_config;
use bms_table_mirror::logger::init_logger;

/// Fetch table indexes from configured sources and save as unified JSON.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to configuration file
    #[arg(long, default_value = "config/index.toml")]
    config: PathBuf,

    /// Output directory for index JSON files
    #[arg(long, default_value = "data/indexes")]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();

    let cli = Cli::parse();

    let config = load_index_config(&cli.config).await?;

    let indexes_dir = &cli.output_dir;
    fs::create_dir_all(indexes_dir).await?;

    let fetcher = Fetcher::lenient()?;

    for idx in &config.source {
        info!("Fetching table index from: {} ({})", idx.name, idx.url);
        let fetched_list = fetcher.fetch_table_list(idx.url.as_str()).await?;
        let infos: Vec<BmsTableInfo> = fetched_list.tables;

        let file_path = indexes_dir.join(format!("{}.json", idx.name));
        let serialized = serde_json::to_string_pretty(&infos)?;
        fs::write(file_path, serialized).await?;

        info!("Saved {} tables from {}", infos.len(), idx.name);
    }

    info!("Index fetch completed.");
    Ok(())
}
