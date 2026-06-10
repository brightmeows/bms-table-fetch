//! Fix directory name mismatches with table info.
//!
//! Scans all table directories and renames any whose name doesn't match
//! the `[domain] sanitize(name)` pattern derived from its `info.json`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bms_table::BmsTableInfo;
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::filesystem::sanitize_filename;

/// CLI arguments for the reconcile subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Directory containing fetched table data
    #[arg(long, default_value = "tables")]
    pub table_dir: PathBuf,
}

/// Fix directory name mismatches with table info.
///
/// # Errors
///
/// Returns an error if reading directories fails unexpectedly.
pub async fn run_reconcile(args: &Args) -> Result<()> {
    let renamed = reconcile_directories(&args.table_dir).await?;
    if renamed > 0 {
        info!("Reconciled {renamed} directory name(s) with table info");
    } else {
        info!("All directory names are consistent — no rename needed");
    }
    Ok(())
}

/// Ensure every directory name matches `[domain] sanitize(info.name)`.
///
/// Scans all subdirectories under `base_dir`, reads each `info.json`, computes the expected
/// directory name, and renames the directory if it doesn't match. Skips `_orphaned/`.
///
/// Returns the number of directories renamed.
async fn reconcile_directories(base_dir: &Path) -> Result<usize> {
    let Ok(mut entries) = fs::read_dir(base_dir).await else {
        info!(
            "Table directory {} does not exist — nothing to reconcile",
            base_dir.display(),
        );
        return Ok(0);
    };

    let mut renamed = 0;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let actual_dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Skip the _orphaned/ directory
        if actual_dir_name == "_orphaned" {
            continue;
        }

        let info_path = path.join("info.json");
        let Ok(content) = fs::read_to_string(&info_path).await else {
            continue;
        };

        let Ok(info) = serde_json::from_str::<BmsTableInfo>(&content) else {
            continue;
        };

        // Determine domain: prefer extra.url_header_json, fall back to parsing from dir name
        let domain: String = info
            .extra
            .get("url_header_json")
            .and_then(|v| v.as_str())
            .and_then(|s| Url::parse(s).ok())
            .and_then(|u| u.domain().map(String::from))
            .or_else(|| {
                // Fallback: parse [domain] from directory name
                actual_dir_name
                    .strip_prefix('[')
                    .and_then(|s| s.split_once(']'))
                    .map(|(d, _)| d.to_string())
            })
            .unwrap_or_else(|| "unknown.domain".to_string());

        let expected_dir_name = sanitize_filename(&format!("[{domain}] {}", info.name));

        if actual_dir_name == expected_dir_name {
            continue;
        }

        let expected_path = base_dir.join(&expected_dir_name);
        if expected_path.exists() {
            warn!("Cannot rename {actual_dir_name} -> {expected_dir_name}: target already exists");
            continue;
        }

        match fs::rename(&path, &expected_path).await {
            Ok(()) => {
                info!("Renamed directory: {actual_dir_name} -> {expected_dir_name}");
                renamed += 1;
            }
            Err(e) => {
                warn!("Failed to rename {actual_dir_name} -> {expected_dir_name}: {e}");
            }
        }
    }

    Ok(renamed)
}
