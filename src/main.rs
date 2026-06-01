use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use bms_table::{
    BmsTable, BmsTableData, BmsTableHeader, BmsTableInfo, BmsTableRaw,
    fetch::reqwest::Fetcher,
};
use clap::{Args, Parser, Subcommand};
use log::{info, warn};
use serde_json::Value;
use tokio::fs;
use url::Url;

use bms_table_fetch::{
    config::list::load_list_config,
    config::table::{load_table_config, TableEntry, TableConfig},
    filesystem::{deep_sort_json_value, is_changed, sanitize_filename},
    logger::init_logger,
};

/// Fetch table lists and/or table data from BMS table sources.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch table lists from configured sources and save as unified JSON.
    List(ListArgs),
    /// Fetch table header/data from list results.
    Tables(TablesArgs),
}

#[derive(Args)]
struct ListArgs {
    /// Path to configuration file
    #[arg(long, default_value = "config/list.toml")]
    config: PathBuf,

    /// Output directory for list JSON files
    #[arg(long, default_value = "lists")]
    output_dir: PathBuf,
}

#[derive(Args)]
struct TablesArgs {
    /// Path to configuration file (for add/replace/disable rules)
    #[arg(long, default_value = "config/table.toml")]
    config: PathBuf,

    /// Directory containing list JSON files
    #[arg(long, default_value = "lists")]
    list_dir: PathBuf,

    /// Only process specific list files by name (comma-separated, without .json extension)
    #[arg(long = "list-names", value_delimiter = ',')]
    list_names: Vec<String>,

    /// Output directory for table data
    #[arg(long, default_value = "tables")]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();

    let cli = Cli::parse();

    match cli.command {
        None => {
            // No subcommand: run list then tables with defaults
            run_list(&ListArgs {
                config: PathBuf::from("config/list.toml"),
                output_dir: PathBuf::from("lists"),
            })
            .await?;

            run_tables(&TablesArgs {
                config: PathBuf::from("config/table.toml"),
                list_dir: PathBuf::from("lists"),
                list_names: vec![],
                output_dir: PathBuf::from("tables"),
            })
            .await?;
        }
        Some(Command::List(args)) => {
            run_list(&args).await?;
        }
        Some(Command::Tables(args)) => {
            run_tables(&args).await?;
        }
    }

    Ok(())
}

async fn run_list(args: &ListArgs) -> Result<()> {
    let config = load_list_config(&args.config).await?;

    let lists_dir = &args.output_dir;
    fs::create_dir_all(lists_dir).await?;

    let fetcher = Fetcher::lenient()?;

    for idx in &config.source {
        info!("Fetching table list from: {} ({})", idx.name, idx.url);
        let fetched_list = fetcher.fetch_table_list(idx.url.as_str()).await?;
        let infos: Vec<BmsTableInfo> = fetched_list.tables;

        let file_path = lists_dir.join(format!("{}.json", idx.name));
        let serialized = serde_json::to_string_pretty(&infos)?;
        fs::write(file_path, serialized).await?;

        info!("Saved {} tables from {}", infos.len(), idx.name);
    }

    info!("List fetch completed.");
    Ok(())
}

async fn run_tables(args: &TablesArgs) -> Result<()> {
    // ── Step 1: Read all list files ───────────────────────────
    let lists_dir = &args.list_dir;
    let mut table_info_map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();

    if !fs::try_exists(lists_dir).await.unwrap_or(false) {
        warn!("List directory {:?} does not exist", lists_dir);
    } else {
        let mut entries = fs::read_dir(lists_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            // If --list-names is specified, skip files not in the list
            if !args.list_names.is_empty() {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if !args.list_names.contains(&stem.to_string()) {
                    continue;
                }
            }

            info!("Loading list file: {:?}", path);
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
        "Loaded {} tables from list files",
        table_info_map.len()
    );

    // ── Step 2: Load config and apply add/replace/disable ────
    let config: Option<TableConfig> = match load_table_config(&args.config).await {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            warn!(
                "Failed to load config {:?}: {} — skipping add/replace/disable rules",
                args.config, e
            );
            None
        }
    };

    if let Some(ref cfg) = config {
        // Add extra tables
        for item in &cfg.table {
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
        for rule in &cfg.replace {
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
            // Unwrap is safe: old_key was found in the map above
            let mut info = table_info_map.remove(&old_key).unwrap();
            info.url = rule.to.clone();
            table_info_map.insert(rule.to.clone(), info);
            info!("Replaced similar URL: {} -> {}", old_key, rule.to);
        }

        // Disable specified table URLs
        for entry in &cfg.disable {
            let url = &entry.url;
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
    let base_dir = &args.output_dir;
    fs::create_dir_all(base_dir).await?;

    let fetcher = Arc::new(Fetcher::lenient()?);
    let mut join_set = tokio::task::JoinSet::new();
    for info in table_info_map.into_values() {
        spawn_fetch(&mut join_set, Arc::clone(&fetcher), info, base_dir);
    }

    while let Some(_res) = join_set.join_next().await {}

    info!("All table fetch tasks finished.");
    Ok(())
}

fn spawn_fetch(
    join_set: &mut tokio::task::JoinSet<()>,
    fetcher: Arc<Fetcher>,
    info: BmsTableInfo,
    base_dir: &Path,
) {
    let url = info.url.clone();
    let name = info.name.clone();
    let base_dir_owned = base_dir.to_path_buf();

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
