//! Fetch table headers and data, layering list results and config rules.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use bms_table::fetch::reqwest::Fetcher;
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::fetch::fetch_and_save_table;
use crate::filesystem::clean_tmp_files;
use crate::overlay;
use crate::scan;

/// CLI arguments for the tables subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Path to configuration file (for add/replace/disable rules)
    #[arg(long, default_value = "config/table.toml")]
    pub config: PathBuf,

    /// Directory containing list JSON files
    #[arg(long, default_value = "lists")]
    pub list_dir: PathBuf,

    /// Only process specific list files by name (comma-separated, without .json extension)
    #[arg(long = "list-names", value_delimiter = ',')]
    pub list_names: Vec<String>,

    /// Output directory for table data
    #[arg(long, default_value = "tables")]
    pub output_dir: PathBuf,
}

/// Fetch table header/data, layering list results and config rules.
///
/// The active table set is determined by lists + config only (lists are authoritative).
/// Disk tables are used only for rename mapping.
///
/// # Errors
///
/// Returns an error if building the active set or creating the output directory fails.
pub async fn run_tables(args: &Args) -> Result<()> {
    // Clean any stale .tmp files before starting
    clean_tmp_files(&args.output_dir).await.ok();

    // Scan for base layer and old_dir_map
    let entries = scan::scan_dirs(&args.output_dir).await?;
    let base_info_map: std::collections::BTreeMap<Url, bms_table::BmsTableInfo> = entries
        .iter()
        .map(|e| (e.info.url.clone(), e.info.clone()))
        .collect();
    let old_dir_map: HashMap<Url, String> = entries
        .iter()
        .map(|e| (e.info.url.clone(), e.dir_name.clone()))
        .collect();

    // Build active set from base + lists + config
    let active = overlay::build_active_set(
        base_info_map,
        old_dir_map,
        &args.list_dir,
        &args.list_names,
        &args.config,
    )
    .await?;

    info!(
        "Total tables after processing: {}",
        active.table_info_map.len()
    );

    // Fetch each table concurrently
    let base_dir = &args.output_dir;
    fs::create_dir_all(base_dir).await?;

    let fetcher = Arc::new(Fetcher::lenient()?);
    let mut join_set = tokio::task::JoinSet::new();

    for (url, info) in active.table_info_map {
        let fetcher = Arc::clone(&fetcher);
        let base_dir = base_dir.clone();
        let old_dir = active.old_dir_map.get(&url).cloned();
        let name = info.name.clone();

        join_set.spawn(async move {
            if let Err(e) = fetch_and_save_table(&fetcher, info, &base_dir, old_dir).await {
                warn!(
                    "Failed to fetch {} from {} -> {}",
                    name,
                    url,
                    e.chain()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" -> ")
                );
            } else {
                info!("Saved table {name} from {url}");
            }
        });
    }

    while let Some(result) = join_set.join_next().await {
        if let Err(e) = result {
            warn!("A table fetch task panicked: {e}");
        }
    }

    info!("All table fetch tasks finished.");
    Ok(())
}
