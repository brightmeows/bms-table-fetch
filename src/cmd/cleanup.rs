//! Move orphaned table directories to `_orphaned/`.
//!
//! A directory is considered orphaned if its URL (from `info.json`) is not in the
//! current set of active tables, determined by loading list files and config rules.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use log::info;
use url::Url;

use crate::{orphan, overlay, scan};

/// CLI arguments for the cleanup subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Path to configuration file (for disable rules)
    #[arg(long, default_value = "config/table.toml")]
    pub config: PathBuf,

    /// Directory containing list JSON files
    #[arg(long, default_value = "lists")]
    pub list_dir: PathBuf,

    /// Only consider specific list files by name (comma-separated, without .json extension)
    #[arg(long = "list-names", value_delimiter = ',')]
    pub list_names: Vec<String>,

    /// Directory containing fetched table data
    #[arg(long, default_value = "tables")]
    pub table_dir: PathBuf,
}

/// Move orphaned directories to `_orphaned/`.
///
/// Loads the current table list (list files + config rules) and compares it against
/// existing table directories. Directories whose URL is no longer in the active set
/// are moved to `_orphaned/`.
///
/// # Errors
///
/// Returns an error if reading list files or the config fails.
pub async fn run_cleanup(args: &Args) -> Result<()> {
    // Scan for base layer and old_dir_map
    let entries = scan::scan_dirs(&args.table_dir).await?;
    let base_info_map: BTreeMap<Url, bms_table::BmsTableInfo> = entries
        .iter()
        .map(|e| (e.info.url.clone(), e.info.clone()))
        .collect();
    let old_dir_map = entries
        .iter()
        .map(|e| (e.info.url.clone(), e.dir_name.clone()))
        .collect();

    // Build the active set (base + lists + config)
    let active = overlay::build_active_set(
        base_info_map,
        old_dir_map,
        &args.list_dir,
        &args.list_names,
        &args.config,
    )
    .await?;

    // Determine orphans
    let orphans = orphan::compute_orphans(&entries, &active.active_urls);

    // Execute moves
    let moved = orphan::execute_orphans(&orphans, &args.table_dir).await;

    if moved > 0 {
        info!("Moved {moved} orphaned director(ies) to _orphaned/");
    } else {
        info!("No orphaned directories found");
    }

    Ok(())
}
