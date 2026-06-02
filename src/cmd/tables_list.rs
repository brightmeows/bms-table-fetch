//! Generate a combined table list from fetched table info.json files.
//!
//! This is the reverse of the list → tables flow: instead of consuming a list
//! to fetch tables, it reads the actual `info.json` from every fetched table
//! directory and writes them all as a single JSON array (same format as list files).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bms_table::BmsTableInfo;
use log::{info, warn};
use serde_json::Value;
use tokio::fs;
use url::Url;

use crate::filesystem::{deep_sort_json_value, is_changed};

/// CLI arguments for the tables-list subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Directory containing fetched table data
    #[arg(long, default_value = "tables")]
    pub table_dir: PathBuf,

    /// Output file path for the combined table list JSON
    #[arg(long, default_value = "tables/tables.json")]
    pub output: PathBuf,
}

/// Read all `info.json` files from `table_dir` and write them as a combined JSON array.
///
/// The output is sorted by table URL for deterministic ordering.
///
/// # Errors
///
/// Returns an error if writing the output file fails.
pub async fn run_tables_list(args: &Args) -> Result<()> {
    let table_infos = load_all_table_infos(&args.table_dir).await?;
    info!("Loaded {} table info entries", table_infos.len());

    let serialized = serde_json::to_string_pretty(&table_infos)?;

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent).await?;
    }

    if is_changed::<Value>(&args.output, &serialized, deep_sort_json_value).await? {
        fs::write(&args.output, &serialized).await?;
        info!("Wrote combined table list: {}", args.output.display());
    } else {
        info!(
            "Combined table list unchanged — skipped write: {}",
            args.output.display()
        );
    }

    Ok(())
}

/// Read `info.json` from every subdirectory under `table_dir` and return them as a `Vec`,
/// sorted by URL for deterministic output.
///
/// Returns an empty vec if the directory does not exist.
async fn load_all_table_infos(table_dir: &Path) -> Result<Vec<BmsTableInfo>> {
    let mut map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();

    match fs::try_exists(table_dir).await {
        Ok(true) => {}
        Ok(false) => {
            info!(
                "Table directory {} does not exist — returning empty list",
                table_dir.display(),
            );
            return Ok(Vec::new());
        }
        Err(e) => {
            warn!(
                "Failed to check table directory {}: {e} — returning empty list",
                table_dir.display(),
            );
            return Ok(Vec::new());
        }
    }

    let mut entries = fs::read_dir(table_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let info_path = path.join("info.json");
        let content = match fs::read_to_string(&info_path).await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read {}: {e} — skipping", info_path.display());
                continue;
            }
        };

        let info: BmsTableInfo = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse {}: {e} — skipping", info_path.display());
                continue;
            }
        };

        map.insert(info.url.clone(), info);
    }

    // BTreeMap is sorted by key (URL), so iterating yields sorted order
    Ok(map.into_values().collect())
}
