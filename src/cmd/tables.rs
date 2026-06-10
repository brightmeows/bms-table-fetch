//! Fetch table headers and data, layering existing tables, list results, then add/replace/disable rules.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use bms_table::{BmsTableInfo, fetch::reqwest::Fetcher};
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::{
    config::table::{TableConfig, TableEntry, load_table_config},
    filesystem::clean_tmp_files,
    sync,
};

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

/// Fetch table header/data, layering existing tables, list results, then add/replace/disable rules.
///
/// The table info map is built in three layers (last wins):
/// 1. **Base** — existing tables: read `info.json` from each subdirectory under `output_dir`.
/// 2. **Overlay** — list results: entries from list JSON files override base entries by URL.
/// 3. **Override** — `table.toml` rules: add new entries, replace URLs, disable entries.
///
/// # Errors
///
/// Returns an error if reading list files, loading the config, or fetching table data fails.
///
pub async fn run_tables(args: &Args) -> Result<()> {
    // Clean any stale .tmp files before starting
    clean_tmp_files(&args.output_dir).await.ok();

    // ── Phase 1: Load existing tables as the base layer ───────
    let (mut table_info_map, mut old_dir_map) = load_existing_table_infos(&args.output_dir).await?;

    // ── Phase 2: Override with list files (layer 2) ──────────
    let list_map = load_list_files(&args.list_dir, &args.list_names).await?;
    let list_count = list_map.len();
    if !list_map.is_empty() {
        table_info_map.extend(list_map);
        info!("Overlaid {list_count} entries from list files");
    }

    // ── Phase 3: Load config and apply add/replace/disable ────
    let config: Option<TableConfig> = match load_table_config(&args.config).await {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            warn!(
                "Failed to load config {}: {e} — skipping add/replace/disable rules",
                args.config.display(),
            );
            None
        }
    };

    if let Some(ref cfg) = config {
        apply_config(&mut table_info_map, cfg, Some(&mut old_dir_map));
    }

    info!("Total tables after processing: {}", table_info_map.len());

    // ── Phase 4: Fetch each table concurrently (bounded) ──────
    let base_dir = &args.output_dir;
    fs::create_dir_all(base_dir).await?;

    let fetcher = Arc::new(Fetcher::lenient()?);
    let mut join_set = tokio::task::JoinSet::new();
    for (url, info) in table_info_map {
        let fetcher = Arc::clone(&fetcher);
        let base_dir = base_dir.clone();
        let old_dir = old_dir_map.get(&url).cloned();
        let name = info.name.clone();

        join_set.spawn(async move {
            if let Err(e) = sync::fetch_and_save_table(&fetcher, info, &base_dir, old_dir).await {
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

/// Read all JSON list files from `list_dir`, optionally filtered by `list_names`.
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

/// Read `info.json` from every subdirectory under `table_dir` to build the base layer.
///
/// Returns a tuple of (`info_map`, `old_dir_map`) where `old_dir_map` maps each URL to the
/// actual directory name on disk (for detecting name changes on re-fetch).
///
/// Returns an empty map if the directory does not exist (first run).
pub(crate) async fn load_existing_table_infos(
    table_dir: &Path,
) -> Result<(BTreeMap<Url, BmsTableInfo>, HashMap<Url, String>)> {
    let mut table_info_map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();
    let mut old_dir_map: HashMap<Url, String> = HashMap::new();

    match fs::try_exists(table_dir).await {
        Ok(true) => {}
        Ok(false) => {
            info!(
                "Table directory {} does not exist — starting with empty base",
                table_dir.display(),
            );
            return Ok((table_info_map, old_dir_map));
        }
        Err(e) => {
            warn!(
                "Failed to check table directory {}: {e} — starting with empty base",
                table_dir.display(),
            );
            return Ok((table_info_map, old_dir_map));
        }
    }

    let mut entries = fs::read_dir(table_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Skip the _orphaned/ directory itself
        if dir_name == "_orphaned" {
            continue;
        }

        let info_path = path.join("info.json");
        let Ok(content) = fs::read_to_string(&info_path).await else {
            continue; // no info.json → not a fetched table directory
        };

        let info: BmsTableInfo = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse {}: {e}", info_path.display());
                continue;
            }
        };

        old_dir_map.insert(info.url.clone(), dir_name);
        table_info_map.insert(info.url.clone(), info);
    }

    info!(
        "Loaded {} existing tables from {}",
        table_info_map.len(),
        table_dir.display(),
    );
    Ok((table_info_map, old_dir_map))
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
        // old_key was cloned from a key in table_info_map above
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
