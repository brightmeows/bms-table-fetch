#![warn(clippy::clone_on_copy)]
#![warn(clippy::redundant_clone)]

mod config;
mod filesystem;
mod logger;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::fs;

use anyhow::Result;
use bms_table::{
    BmsTable, BmsTableData, BmsTableHeader, BmsTableInfo, BmsTableRaw, fetch::reqwest::Fetcher,
};
use log::{info, warn};
use url::Url;

use crate::{
    config::{AddTableInfo, TableConfig, load_table_config},
    filesystem::{deep_sort_json_value, is_changed, sanitize_filename},
    logger::init_logger,
};

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();

    // Load configuration from tables.toml
    let config: TableConfig = load_table_config("tables.toml").await?;

    // Build BTreeMap<Url, BmsTableInfo>
    let mut table_info_map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();

    // Prepare index output directory
    let lists_dir = Path::new("lists");
    fs::create_dir_all(lists_dir).await?;

    let fetcher = Fetcher::lenient()?;

    // Fetch and merge indexes from configured index endpoints
    for idx in &config.table_list {
        // Fetch table list
        info!("Fetching table index from: {} ({})", idx.name, idx.url);
        let fetched_list = fetcher.fetch_table_list(idx.url.as_str()).await?;
        let infos = fetched_list.tables;
        let original_json = fetched_list.raw_json;
        // Write list json file
        let list_file_path = lists_dir.join(format!("{}.json", idx.name));
        fs::write(list_file_path, original_json).await?;
        // Yield into table_info_map
        for info in infos {
            table_info_map.insert(info.url.clone(), info);
        }
    }

    // Add extra tables directly
    for item in &config.add_table {
        let copied: BmsTableInfo = AddTableInfo {
            name: item.name.clone(),
            url: item.url.clone(),
            symbol: item.symbol.clone(),
            extra: item.extra.clone(),
        }
        .into();
        table_info_map.insert(copied.url.clone(), copied);
    }

    // Replace specified table URLs (before disabling)
    for rule in &config.replace_table_url {
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
    for url in &config.disable_table_url {
        if table_info_map.remove(url).is_some() {
            info!("Disabled table: {}", url);
        }
    }

    // Prepare base output directory
    let base_dir = Path::new("tables");
    fs::create_dir_all(base_dir).await?;

    // Fetch each table concurrently based on built map
    let mut join_set = tokio::task::JoinSet::new();
    for info in table_info_map.into_values() {
        spawn_fetch(&mut join_set, info, base_dir)?;
    }

    while let Some(_res) = join_set.join_next().await {}

    info!("All tasks finished.");
    Ok(())
}

fn spawn_fetch(
    join_set: &mut tokio::task::JoinSet<()>,
    info: BmsTableInfo,
    base_dir: &Path,
) -> Result<()> {
    let url = info.url.clone();
    let name = info.name.clone();
    // JoinSet 需要 'static future，捕获一个拥有所有权的 PathBuf
    let base_dir_owned = base_dir.to_path_buf();

    let fetcher = Fetcher::lenient()?;

    join_set.spawn(async move {
        let idx = info;
        if let Err(e) = fetch_and_save_table(&fetcher, idx, base_dir_owned.as_path()).await {
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
    // 先获取表数据与原始JSON，再创建目录与写入
    let fetched = fetcher.fetch_table(info.url.as_str()).await?;
    let BmsTable { header, data } = fetched.table;
    let BmsTableRaw {
        header_raw,
        data_raw,
        header_json_url,
        data_json_url,
    } = fetched.raw;

    // 使用 BmsTableHeader 的 name 作为目录名（经 sanitize）
    let dir_name = sanitize_filename(&format!(
        "[{}] {}",
        header_json_url.domain().unwrap_or("unknown.domain"),
        header.name
    ));
    let out_dir = base_dir.join(dir_name);

    // 使用 BmsTableHeader 的 data_url 字段，直接用 String::replace 将其替换为 "data.json"
    let patched_header = header_raw.replace(&header.data_url, "data.json");

    // 在成功获取后再创建目录与写入文件
    fs::create_dir_all(&out_dir).await?;
    let header_path: PathBuf = out_dir.join("header.json");
    let data_path = out_dir.join("data.json");

    // 条件写入：仅在解析失败或对象不同的情况下替换
    if is_changed::<BmsTableHeader>(&header_path, &patched_header, |header| {
        header.extra = Default::default()
    })
    .await?
    {
        // check if `patched_header` can parse
        let header_to_write = match serde_json::from_str::<BmsTableHeader>(&patched_header) {
            Ok(_) => patched_header,
            Err(_) => serde_json::to_string_pretty(&header)?,
        };
        fs::write(&header_path, &header_to_write).await?;
    }
    if is_changed::<BmsTableData>(&data_path, &data_raw, |data| {
        data.charts
            .iter_mut()
            .for_each(|v| v.extra = Default::default())
    })
    .await?
    {
        // check if `data_raw` can parse
        let data_to_write = match serde_json::from_str::<BmsTableData>(&data_raw) {
            Ok(_) => data_raw,
            Err(_) => serde_json::to_string_pretty(&data)?,
        };
        fs::write(&data_path, &data_to_write).await?;
    }

    // 向info同步实际获取的难度表信息
    info.name = header.name;
    info.symbol = header.symbol;

    // 向info的extra字段写入url_header_json和url_data_json
    *info.extra.entry("url_header_json".to_string()).or_default() =
        serde_json::to_value(header_json_url)?;
    *info.extra.entry("url_data_json".to_string()).or_default() =
        serde_json::to_value(data_json_url)?;

    // 写入info
    let info_path: PathBuf = out_dir.join("info.json");
    let info_data = serde_json::to_string_pretty(&info)?;
    if is_changed::<serde_json::Value>(&info_path, &info_data, deep_sort_json_value).await? {
        fs::write(&info_path, &info_data).await?;
    }

    Ok(())
}
