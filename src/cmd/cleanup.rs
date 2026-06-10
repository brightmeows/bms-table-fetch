//! Move orphaned table directories to `_orphaned/`.
//!
//! A directory is considered orphaned if its URL (from `info.json`) is not in the
//! current set of active tables, determined by loading list files and config rules.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bms_table::BmsTableInfo;
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::{cmd::tables, config::table::load_table_config};

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
    let base_dir = &args.table_dir;

    // Phase 1: Load existing tables
    let (table_info_map, _) = tables::load_existing_table_infos(base_dir).await?;

    // Phase 2: Override with list files
    let mut active_map = table_info_map;
    let list_map = tables::load_list_files(&args.list_dir, &args.list_names).await?;
    if !list_map.is_empty() {
        active_map.extend(list_map);
    }

    // Phase 3: Apply config
    match load_table_config(&args.config).await {
        Ok(cfg) => tables::apply_config(&mut active_map, &cfg, None),
        Err(e) => warn!(
            "Failed to load config {}: {e} — skipping disable rules",
            args.config.display(),
        ),
    }

    let active_urls: HashSet<Url> = active_map.into_keys().collect();
    info!("Active tables after loading: {}", active_urls.len());

    // Phase 4: Cleanup orphans
    let moved = cleanup_orphans(base_dir, &active_urls).await?;
    if moved > 0 {
        info!("Moved {moved} orphaned director(ies) to _orphaned/");
    } else {
        info!("No orphaned directories found");
    }

    Ok(())
}

/// Move directories whose URL is no longer in `active_urls` to `_orphaned/`.
///
/// Reads each directory's `info.json`, checks if its URL is in the active set.
/// If not, the directory is moved under `_orphaned/<original_dir_name>/`.
/// Skips the `_orphaned/` directory itself.
///
/// Returns the number of directories moved.
pub(crate) async fn cleanup_orphans(base_dir: &Path, active_urls: &HashSet<Url>) -> Result<usize> {
    let Ok(mut entries) = fs::read_dir(base_dir).await else {
        return Ok(0);
    };

    let orphan_dir = base_dir.join("_orphaned");
    let mut moved = 0;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Skip the orphan directory itself
        if dir_name == "_orphaned" {
            continue;
        }

        let info_path = path.join("info.json");
        let Ok(content) = fs::read_to_string(&info_path).await else {
            continue;
        };
        let Ok(info) = serde_json::from_str::<BmsTableInfo>(&content) else {
            continue;
        };

        if active_urls.contains(&info.url) {
            continue;
        }

        // This directory is orphaned: move to _orphaned/
        let destination = orphan_dir.join(&dir_name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Remove stale orphan if it already exists
        if destination.exists() {
            fs::remove_dir_all(&destination).await?;
        }

        match fs::rename(&path, &destination).await {
            Ok(()) => {
                info!("Moved orphaned directory: {dir_name} -> _orphaned/{dir_name}");
                moved += 1;
            }
            Err(e) => {
                warn!("Failed to move orphaned {dir_name}: {e}");
            }
        }
    }

    Ok(moved)
}
