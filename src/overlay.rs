//! Active set computation: build the list of tables to fetch from lists + config.
//!
//! Lists are the authoritative source of truth for the active table set.
//! Disk tables are only used for rename mapping (`old_dir_map`), not for
//! determining which tables should be fetched.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use bms_table::BmsTableInfo;
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::config::table::{self as table_config, TableConfig, TableEntry};

/// Active table set after applying list overlay and config rules.
pub struct ActiveSet {
    /// URLs that are considered "active" (should be kept, not orphaned).
    pub active_urls: HashSet<Url>,
    /// Full table info map (lists + config overrides).
    pub table_info_map: BTreeMap<Url, BmsTableInfo>,
    /// Maps each URL to its directory name from the previous run (for rename detection).
    pub old_dir_map: HashMap<Url, String>,
}

/// Build the active table set from base layer + lists + config.
///
/// The `base_info_map` contains tables discovered from disk (reading `info.json`),
/// the `old_dir_map` maps URLs to existing directory names (for rename detection).
/// The active set is computed as: base → lists (extend) → config (add/replace/disable).
///
/// # Errors
///
/// Returns an error if reading list files fails.
#[expect(
    clippy::implicit_hasher,
    reason = "HashMap<Url, String> is the canonical type for old directory mapping"
)]
pub async fn build_active_set(
    base_info_map: BTreeMap<Url, BmsTableInfo>,
    old_dir_map: HashMap<Url, String>,
    list_dir: &Path,
    list_names: &[String],
    config_path: &Path,
) -> Result<ActiveSet> {
    // Start with base layer (disk info.json)
    let mut table_info_map = base_info_map;

    // Overlay with list files (extend — adds/overwrites, never removes)
    let list_map = load_list_files(list_dir, list_names).await?;
    if !list_map.is_empty() {
        table_info_map.extend(list_map);
    }

    // Apply config rules (add/replace/disable)
    if let Ok(cfg) = table_config::load_table_config(config_path).await {
        apply_config(&mut table_info_map, &cfg, Some(&mut old_dir_map.clone()));
    }

    let active_urls: HashSet<Url> = table_info_map.keys().cloned().collect();
    info!("Active tables after overlay: {}", active_urls.len());

    Ok(ActiveSet {
        active_urls,
        table_info_map,
        old_dir_map,
    })
}

/// Read all JSON list files from `list_dir`, optionally filtered by `list_names`.
///
/// # Errors
///
/// Returns an error if reading the list directory fails.
pub(crate) async fn load_list_files(
    list_dir: &Path,
    list_names: &[String],
) -> Result<BTreeMap<Url, BmsTableInfo>> {
    let mut table_info_map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();

    match fs::try_exists(list_dir).await {
        Ok(true) => {}
        Ok(false) => {
            warn!("List directory {} does not exist", list_dir.display());
            return Ok(table_info_map);
        }
        Err(e) => {
            warn!(
                "Failed to check list directory {}: {e} — skipping",
                list_dir.display()
            );
            return Ok(table_info_map);
        }
    }

    let mut entries = fs::read_dir(list_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        // If --list-names is specified, skip files not in the list
        if !list_names.is_empty() {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !list_names.contains(&stem.to_string()) {
                continue;
            }
        }

        info!("Loading list file: {}", path.display());
        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read {}: {e}", path.display());
                continue;
            }
        };
        let infos: Vec<BmsTableInfo> = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse {}: {e}", path.display());
                continue;
            }
        };
        for info in infos {
            table_info_map.insert(info.url.clone(), info);
        }
    }

    info!("Loaded {} tables from list files", table_info_map.len());
    Ok(table_info_map)
}

/// Apply add/replace/disable rules from `config` to `table_info_map` in place.
///
/// When `old_dir_map` is provided, URL replacement rules also migrate the corresponding
/// directory name mapping so that subsequent rename logic can find the old directory.
pub(crate) fn apply_config(
    table_info_map: &mut BTreeMap<Url, BmsTableInfo>,
    config: &TableConfig,
    mut old_dir_map: Option<&mut HashMap<Url, String>>,
) {
    // Add extra tables
    for item in &config.table {
        let copied: BmsTableInfo = TableEntry {
            name: item.name.clone(),
            url: item.url.clone(),
            symbol: item.symbol.clone(),
            extra: item.extra.clone(),
        }
        .into();
        table_info_map.insert(copied.url.clone(), copied);
    }

    // Replace specified table URLs
    for rule in &config.replace {
        // Try exact key match first
        if let Some(mut info) = table_info_map.remove(&rule.from) {
            if let Some(ref mut dirs) = old_dir_map
                && let Some(dir_name) = dirs.remove(&rule.from)
            {
                dirs.insert(rule.to.clone(), dir_name);
            }
            info.url = rule.to.clone();
            table_info_map.insert(rule.to.clone(), info);
            info!("Replaced table URL: {} -> {}", rule.from, rule.to);
            continue;
        }

        // Fallback: match ignoring trailing slashes
        let from_str = rule.from.as_str().trim_end_matches('/');
        let mut found_key: Option<Url> = None;
        for k in table_info_map.keys() {
            if k.as_str().trim_end_matches('/') == from_str {
                found_key = Some(k.clone());
                break;
            }
        }
        let Some(old_key) = found_key else {
            warn!("URL to replace not found: {}", rule.from);
            continue;
        };
        let mut info = table_info_map
            .remove(&old_key)
            .unwrap_or_else(|| unreachable!("old_key verified present via let-else guard"));
        if let Some(ref mut dirs) = old_dir_map
            && let Some(dir_name) = dirs.remove(&old_key)
        {
            dirs.insert(rule.to.clone(), dir_name);
        }
        info.url = rule.to.clone();
        table_info_map.insert(rule.to.clone(), info);
        info!("Replaced similar URL: {} -> {}", old_key, rule.to);
    }

    // Disable specified table URLs
    for entry in &config.disable {
        let url = &entry.url;
        if table_info_map.remove(url).is_some() {
            info!("Disabled table: {url}");
        }
    }
}
