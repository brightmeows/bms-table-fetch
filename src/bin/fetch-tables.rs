use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use bms_table::{
    BmsTable, BmsTableData, BmsTableHeader, BmsTableInfo, BmsTableRaw,
    fetch::reqwest::Fetcher,
};
use clap::Parser;
use log::{info, warn};
use serde_json::Value;
use tokio::fs;
use url::Url;

use bms_table_mirror::{
    config::{load_table_config, AddTableInfo, TableConfig},
    filesystem::{deep_sort_json_value, is_changed, sanitize_filename},
    logger::init_logger,
};

/// Fetch table header/data from index results.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to configuration file (for add/replace/disable rules)
    #[arg(long, default_value = "config/tables.toml")]
    config: String,

    /// Only process specific index files (comma-separated, without .json extension)
    #[arg(long, value_delimiter = ',')]
    index: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();

    let cli = Cli::parse();

    // ── Step 1: Read all index files ──────────────────────────
    let indexes_dir = Path::new("data/indexes");
    let mut table_info_map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();

    if !fs::try_exists(indexes_dir).await.unwrap_or(false) {
        warn!("Index directory {:?} does not exist", indexes_dir);
    } else {
        let mut entries = fs::read_dir(indexes_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            // If --index is specified, skip files not in the list
            if !cli.index.is_empty() {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if !cli.index.contains(&stem.to_string()) {
                    continue;
                }
            }

            info!("Loading index file: {:?}", path);
            let content = match fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read {:?}: {}", path, e);
                    continue;
                }
            };
            let infos: Vec<BmsTableInfo> = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse {:?}: {}", path, e);
                    continue;
                }
            };
            for info in infos {
                table_info_map.insert(info.url.clone(), info);
            }
        }
    }

    info!(
        "Loaded {} tables from index files",
        table_info_map.len()
    );

    // ── Step 2: Load config and apply add/replace/disable ────
    let config: Option<TableConfig> = match load_table_config(&cli.config).await {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            warn!(
                "Failed to load config {:?}: {} — skipping add/replace/disable rules",
                cli.config, e
            );
            None
        }
    };

    if let Some(ref cfg) = config {
        // Add extra tables
        for item in &cfg.add_table {
            let copied: BmsTableInfo = AddTableInfo {
                name: item.name.clone(),
                url: item.url.clone(),
                symbol: item.symbol.clone(),
                extra: item.extra.clone(),
            }
            .into();
            table_info_map.insert(copied.url.clone(), copied);
        }

        // Replace specified table URLs
        for rule in &cfg.replace_table_url {
            // Try exact key match first
            if let Some(mut info) = table_info_map.remove(&rule.from) {
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
            if let Some(mut info) = table_info_map.remove(&old_key) {
                info.url = rule.to.clone();
                table_info_map.insert(rule.to.clone(), info);
                info!("Replaced similar URL: {} -> {}", old_key, rule.to);
            } else {
                table_info_map.insert(
                    rule.to.clone(),
                    BmsTableInfo {
                        name: rule.to.domain().unwrap_or("unknown").to_string(),
                        url: rule.to.clone(),
                        symbol: "-".to_string(),
                        extra: Default::default(),
                    },
                );
                info!("Old table not found, added new table: {}", rule.to);
            }
        }

        // Disable specified table URLs
        for url in &cfg.disable_table_url {
            if table_info_map.remove(url).is_some() {
                info!("Disabled table: {}", url);
            }
        }
    }

    info!(
        "Total tables to fetch after processing: {}",
        table_info_map.len()
    );

    // ── Step 3: Fetch each table concurrently ─────────────────
    let base_dir = Path::new("data/tables");
    fs::create_dir_all(base_dir).await?;

    let mut join_set = tokio::task::JoinSet::new();
    for info in table_info_map.into_values() {
        spawn_fetch(&mut join_set, info, base_dir)?;
    }

    while let Some(_res) = join_set.join_next().await {}

    info!("All table fetch tasks finished.");
    Ok(())
}

fn spawn_fetch(
    join_set: &mut tokio::task::JoinSet<()>,
    info: BmsTableInfo,
    base_dir: &Path,
) -> Result<()> {
    let url = info.url.clone();
    let name = info.name.clone();
    let base_dir_owned = base_dir.to_path_buf();
    let fetcher = Fetcher::lenient()?;

    join_set.spawn(async move {
        if let Err(e) = fetch_and_save_table(&fetcher, info, base_dir_owned.as_path()).await {
            warn!(
                "Failed to fetch {} from {} -> {}",
                name,
                url,
                e.chain()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            );
        } else {
            info!("Saved table {} from {}", name, url);
        }
    });

    Ok(())
}

async fn fetch_and_save_table(
    fetcher: &Fetcher,
    mut info: BmsTableInfo,
    base_dir: &Path,
) -> Result<()> {
    let fetched = fetcher.fetch_table(info.url.as_str()).await?;
    let BmsTable { header, data } = fetched.table;
    let BmsTableRaw {
        header_raw,
        data_raw,
        header_json_url,
        data_json_url,
    } = fetched.raw;

    // Use BmsTableHeader's name as directory name (via sanitize)
    let dir_name = sanitize_filename(&format!(
        "[{}] {}",
        header_json_url.domain().unwrap_or("unknown.domain"),
        header.name
    ));
    let out_dir = base_dir.join(dir_name);

    // Patch header to point to "data.json" instead of original data_url
    let patched_header = header_raw.replace(&header.data_url, "data.json");

    fs::create_dir_all(&out_dir).await?;
    let header_path: PathBuf = out_dir.join("header.json");
    let data_path = out_dir.join("data.json");

    // Conditional write for header
    if is_changed::<BmsTableHeader>(&header_path, &patched_header, |header| {
        header.extra = Default::default()
    })
    .await?
    {
        let header_to_write = match serde_json::from_str::<BmsTableHeader>(&patched_header) {
            Ok(_) => patched_header,
            Err(_) => serde_json::to_string_pretty(&header)?,
        };
        fs::write(&header_path, &header_to_write).await?;
    }

    // Conditional write for data
    if is_changed::<BmsTableData>(&data_path, &data_raw, |data| {
        data.charts
            .iter_mut()
            .for_each(|v| v.extra = Default::default())
    })
    .await?
    {
        let data_to_write = match serde_json::from_str::<BmsTableData>(&data_raw) {
            Ok(_) => data_raw,
            Err(_) => serde_json::to_string_pretty(&data)?,
        };
        fs::write(&data_path, &data_to_write).await?;
    }

    // Sync actual header info back to info struct
    info.name = header.name;
    info.symbol = header.symbol;

    // Write URL sources into extra
    *info.extra.entry("url_header_json".to_string()).or_default() =
        serde_json::to_value(header_json_url)?;
    *info.extra.entry("url_data_json".to_string()).or_default() =
        serde_json::to_value(data_json_url)?;

    // Write info.json
    let info_path: PathBuf = out_dir.join("info.json");
    let info_data = serde_json::to_string_pretty(&info)?;
    if is_changed::<Value>(&info_path, &info_data, deep_sort_json_value).await? {
        fs::write(&info_path, &info_data).await?;
    }

    Ok(())
}
