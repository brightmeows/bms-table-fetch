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
/// The output is sorted by table URL for deterministic ordering. If the output file already
/// exists and its content differs from the newly loaded entries, the file is overwritten
/// and a detailed diff is logged. Otherwise, the write is skipped.
///
/// # Errors
///
/// Returns an error if writing the output file fails.
pub async fn run_tables_list(args: &Args) -> Result<()> {
    let table_infos = load_all_table_infos(&args.table_dir).await?;
    let new_count = table_infos.len();
    info!("Loaded {new_count} table info entries");

    let serialized = serde_json::to_string_pretty(&table_infos)?;

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent).await?;
    }

    // Load existing file for diff reporting
    let old_infos: Option<Vec<BmsTableInfo>> = fs::read_to_string(&args.output)
        .await
        .ok()
        .as_deref()
        .and_then(|c| serde_json::from_str(c).ok());

    let needs_update = is_changed::<Value>(&args.output, &serialized, deep_sort_json_value).await?;

    match (&old_infos, needs_update) {
        (Some(old), true) => {
            let old_count = old.len();
            let diff = compute_table_diff(old, &table_infos);
            let changes = format_changes_summary(
                diff.added.len(),
                diff.removed.len(),
                diff.modified.len(),
            );

            warn!(
                "tables.json was out of sync — regenerated ({changes}, {old_count} → {new_count} entries)"
            );

            for entry in &diff.added {
                info!("  + Added: {}", display_entry_name(entry));
            }
            for entry in &diff.removed {
                info!("  - Removed: {}", display_entry_name(entry));
            }
            for entry in &diff.modified {
                info!("  ~ Modified: {}", display_entry_name(entry));
            }

            fs::write(&args.output, &serialized).await?;
            info!("Wrote combined table list: {}", args.output.display());
        }
        (None, true) => {
            warn!("tables.json was missing or corrupt — regenerated ({new_count} entries)");
            fs::write(&args.output, &serialized).await?;
            info!("Wrote combined table list: {}", args.output.display());
        }
        (_, false) => {
            info!(
                "tables.json is consistent with tables/ ({new_count} entries) — no update needed"
            );
        }
    }

    Ok(())
}

/// The result of comparing two sets of [`BmsTableInfo`] entries, keyed by URL.
struct TableDiff {
    /// Entries present in the new set but not in the old set.
    added: Vec<BmsTableInfo>,
    /// Entries present in the old set but not in the new set.
    removed: Vec<BmsTableInfo>,
    /// Entries present in both sets but with changed content (by [`PartialEq`]).
    modified: Vec<BmsTableInfo>,
}

/// Compare old and new table info lists, returning the categorized differences.
///
/// Entries are identified by their URL. An entry is considered modified if any field
/// (including `extra`) differs between the old and new version.
fn compute_table_diff(old: &[BmsTableInfo], new: &[BmsTableInfo]) -> TableDiff {
    let old_by_url: BTreeMap<&Url, &BmsTableInfo> = old.iter().map(|e| (&e.url, e)).collect();
    let new_by_url: BTreeMap<&Url, &BmsTableInfo> = new.iter().map(|e| (&e.url, e)).collect();

    let mut added = Vec::new();
    let mut modified = Vec::new();

    for (url, new_entry) in &new_by_url {
        match old_by_url.get(url) {
            None => added.push((*new_entry).clone()),
            Some(old_entry) if **old_entry != **new_entry => modified.push((*new_entry).clone()),
            Some(_) => {}
        }
    }

    let removed: Vec<BmsTableInfo> = old
        .iter()
        .filter(|e| !new_by_url.contains_key(&e.url))
        .cloned()
        .collect();

    TableDiff { added, removed, modified }
}

/// Format a human-readable changes summary like `+3/-0/~1`.
fn format_changes_summary(added: usize, removed: usize, modified: usize) -> String {
    [
        if added > 0 { Some(format!("+{added}")) } else { None },
        if removed > 0 { Some(format!("-{removed}")) } else { None },
        if modified > 0 { Some(format!("~{modified}")) } else { None },
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("/")
}

/// Return the display name for a table entry, falling back to URL if name is empty.
fn display_entry_name(info: &BmsTableInfo) -> String {
    if info.name.is_empty() {
        info.url.to_string()
    } else {
        info.name.clone()
    }
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
