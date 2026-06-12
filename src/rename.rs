//! Directory rename logic: compute expected directory names and execute renames.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bms_table::BmsTableInfo;
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::filesystem::sanitize_filename;

/// A pending directory rename (old name → new name), with paths relative to the table base directory.
pub struct RenameAction {
    /// Current (stale) path, relative to table base dir.
    pub old_path: PathBuf,
    /// Desired (canonical) path, relative to table base dir.
    pub new_path: PathBuf,
}

/// Trait for accessing directory entry fields needed for rename and orphan computation.
pub trait DirInfo {
    /// Returns the actual directory name on disk.
    fn dir_name(&self) -> &str;
    /// Returns the table info associated with this directory.
    fn info(&self) -> &BmsTableInfo;
}

/// Compute the expected directory name from table info.
///
/// Domain extraction priority:
/// 1. `info.url`'s domain (the canonical table page URL)
/// 2. `[domain]` prefix parsed from `fallback_dir_name`
/// 3. `"unknown.domain"` as last resort
///
/// The result is `sanitize_filename("[{domain}] {info.name}")`.
#[must_use]
pub fn expected_dir_name(info: &BmsTableInfo, fallback_dir_name: Option<&str>) -> String {
    let domain: String = info
        .url
        .domain()
        .map(String::from)
        .or_else(|| {
            fallback_dir_name
                .and_then(|n| n.strip_prefix('['))
                .and_then(|s| s.split_once(']'))
                .map(|(d, _)| d.to_string())
        })
        .unwrap_or_else(|| "unknown.domain".to_string());

    sanitize_filename(&format!("[{domain}] {}", info.name))
}

/// Compute which directories need renaming.
///
/// If `overlaid_info` is provided, uses it to look up each entry's URL and uses
/// the overlaid info for name computation. Otherwise uses the entry's own info.
#[must_use]
pub fn compute_renames(
    entries: &[impl DirInfo],
    overlaid_info: Option<&BTreeMap<Url, BmsTableInfo>>,
) -> Vec<RenameAction> {
    let mut actions = Vec::new();

    for entry in entries {
        let info = overlaid_info
            .and_then(|m| m.get(&entry.info().url))
            .unwrap_or_else(|| entry.info());

        let expected = expected_dir_name(info, Some(entry.dir_name()));

        if entry.dir_name() != expected {
            actions.push(RenameAction {
                old_path: PathBuf::from(entry.dir_name()),
                new_path: PathBuf::from(&expected),
            });
        }
    }

    actions
}

/// Execute a list of rename actions, returning the ones that succeeded.
pub async fn execute_renames(actions: &[RenameAction], base_dir: &Path) -> Vec<RenameAction> {
    let mut executed = Vec::new();

    for action in actions {
        let old_path = base_dir.join(&action.old_path);
        let new_path = base_dir.join(&action.new_path);

        if !fs::try_exists(&old_path).await.unwrap_or(false) {
            continue;
        }
        if fs::try_exists(&new_path).await.unwrap_or(false) {
            warn!(
                "Cannot rename {} -> {}: target already exists",
                old_path.display(),
                new_path.display()
            );
            continue;
        }

        match fs::rename(&old_path, &new_path).await {
            Ok(()) => {
                info!("Renamed: {} -> {}", old_path.display(), new_path.display());
                executed.push(RenameAction {
                    old_path: action.old_path.clone(),
                    new_path: action.new_path.clone(),
                });
            }
            Err(e) => warn!(
                "Failed to rename {} -> {}: {e}",
                old_path.display(),
                new_path.display()
            ),
        }
    }

    executed
}

/// Rename a single directory from `old_name` to `new_name` under `base_dir`.
///
/// If `old_name` is `None` or same as `new_name`, does nothing.
/// If the old directory doesn't exist, does nothing.
/// If the new directory already exists, skips the rename (no data loss).
///
/// # Errors
///
/// Returns an error if the filesystem rename fails.
pub async fn maybe_rename_dir(
    base_dir: &Path,
    new_name: &str,
    old_name: Option<&str>,
) -> Result<()> {
    let Some(ref old) = old_name else {
        return Ok(());
    };
    if *old == new_name {
        return Ok(());
    }

    let old_path = base_dir.join(old);
    if !fs::try_exists(&old_path).await.unwrap_or(false) {
        return Ok(());
    }

    let new_path = base_dir.join(new_name);
    if fs::try_exists(&new_path).await.unwrap_or(false) {
        warn!("Cannot rename {old} -> {new_name}: target already exists, skipping");
        return Ok(());
    }

    info!("Renaming directory {old} -> {new_name} (table name changed)");
    fs::rename(&old_path, &new_path).await?;

    Ok(())
}
