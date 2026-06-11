//! HTTP fetch and persistence: fetch table data and list data from remote, save to disk.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use bms_table::{BmsTableData, BmsTableHeader, BmsTableInfo, fetch::reqwest::Fetcher};
use log::{info, warn};
use serde_json::Value;
use tokio::fs;

use crate::config::list::Source;
use crate::filesystem::{deep_sort_json_value, is_changed, sanitize_filename, write_atomic};
use crate::rename::{expected_dir_name, maybe_rename_dir};

/// Fetch all list sources and save to `output_dir`.
///
/// On individual source failure, logs a warning and preserves the old cached file.
/// Other sources continue to be fetched.
///
/// # Errors
///
/// Returns an error if the output directory cannot be created.
pub async fn fetch_list_sources(sources: &[Source], output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir).await?;
    let fetcher = Fetcher::lenient()?;

    for idx in sources {
        info!("Fetching table list from: {} ({})", idx.name, idx.url);
        match fetcher.fetch_table_list(idx.url.as_str()).await {
            Ok(fetched_list) => {
                let infos: Vec<BmsTableInfo> = fetched_list.tables;
                let serialized = serde_json::to_string_pretty(&infos)?;

                let file_path = output_dir.join(format!("{}.json", idx.name));
                if is_changed::<Value>(&file_path, &serialized, deep_sort_json_value).await? {
                    write_atomic(&file_path, &serialized).await?;
                    info!("Saved {} tables from {}", infos.len(), idx.name);
                } else {
                    info!(
                        "{} tables from {} unchanged — skipping write",
                        infos.len(),
                        idx.name
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Failed to fetch list from {} ({}): {e} — keeping cached file",
                    idx.name, idx.url
                );
            }
        }
    }

    info!("List fetch completed.");
    Ok(())
}

/// Fetch a single table from remote and save to disk.
///
/// Steps:
/// 1. Pre-fetch rename using overlaid info
/// 2. HTTP fetch
/// 3. Compute response-based directory name, rename if needed
/// 4. Write info.json, header.json, data.json (conditional)
///
/// # Errors
///
/// Returns an error if the HTTP fetch fails or file writing fails.
pub async fn fetch_and_save_table(
    fetcher: &Fetcher,
    mut info: BmsTableInfo,
    base_dir: &Path,
    old_dir_name: Option<String>,
) -> Result<()> {
    // Phase 1: Pre-fetch rename using overlaid info
    let pre_dir_name = expected_dir_name(&info, old_dir_name.as_deref());
    maybe_rename_dir(base_dir, &pre_dir_name, old_dir_name.as_deref()).await?;

    // Phase 2: HTTP fetch
    let response = fetcher.fetch_table(info.url.as_str()).await?;
    let bms_table::BmsTable { header, data } = response.table;
    let bms_table::BmsTableRaw {
        header_raw,
        data_raw,
        header_json_url,
        data_json_url,
    } = response.raw;

    // Phase 3: Compute response-based directory name
    let dir_name = sanitize_filename(&format!(
        "[{}] {}",
        header_json_url.domain().unwrap_or("unknown.domain"),
        header.name
    ));

    let final_dir_name = if dir_name == pre_dir_name {
        pre_dir_name
    } else {
        maybe_rename_dir(base_dir, &dir_name, Some(&pre_dir_name)).await?;
        dir_name
    };
    let out_dir = base_dir.join(&final_dir_name);

    let patched_header = patch_data_url(&header_raw);

    fs::create_dir_all(&out_dir).await?;
    let header_path = out_dir.join("header.json");
    let data_path = out_dir.join("data.json");

    // Conditional write for header
    if is_changed::<BmsTableHeader>(&header_path, &patched_header, |h| {
        h.extra = BTreeMap::default();
    })
    .await?
    {
        let header_to_write = match serde_json::from_str::<BmsTableHeader>(&patched_header) {
            Ok(_) => patched_header,
            Err(_) => serde_json::to_string_pretty(&header)?,
        };
        write_atomic(&header_path, &header_to_write).await?;
    }

    // Conditional write for data
    if is_changed::<BmsTableData>(&data_path, &data_raw, |d| {
        d.charts
            .iter_mut()
            .for_each(|v| v.extra = BTreeMap::default());
    })
    .await?
    {
        let data_to_write = match serde_json::from_str::<BmsTableData>(&data_raw) {
            Ok(_) => data_raw,
            Err(_) => serde_json::to_string_pretty(&data)?,
        };
        write_atomic(&data_path, &data_to_write).await?;
    }

    // Sync header info back
    info.name = header.name;
    info.symbol = header.symbol;
    *info.extra.entry("url_header_json".to_string()).or_default() =
        serde_json::to_value(header_json_url)?;
    *info.extra.entry("url_data_json".to_string()).or_default() =
        serde_json::to_value(data_json_url)?;

    // Write info.json
    let info_path = out_dir.join("info.json");
    let info_data = serde_json::to_string_pretty(&info)?;
    if is_changed::<Value>(&info_path, &info_data, deep_sort_json_value).await? {
        write_atomic(&info_path, &info_data).await?;
    }

    Ok(())
}

/// Patch `"data_url"` value in header JSON to `"./data.json"`.
///
/// Uses byte-level search so it works even when the serializer escapes `"/"`.
#[must_use]
pub fn patch_data_url(header_raw: &str) -> String {
    let mut result = header_raw.to_string();
    let key = br#""data_url""#;
    let key_len = key.len();

    if let Some(key_pos) = header_raw.find(r#""data_url""#)
        && let Some(tail) = header_raw.get(key_pos + key_len..)
        && let Some(colon) = tail.bytes().position(|b| b == b':')
        && let Some(after_colon) = header_raw.get(key_pos + key_len + colon + 1..)
        && let Some(quote) = after_colon.bytes().position(|b| b == b'"')
    {
        let content_start = key_pos + key_len + colon + 1 + quote + 1;
        let content = &header_raw[content_start..];

        // Find unescaped closing quote
        let mut content_end = None;
        let mut chars = content.char_indices();
        while let Some((off, ch)) = chars.next() {
            if ch == '\\' {
                chars.next();
                continue;
            }
            if ch == '"' {
                content_end = Some(off);
                break;
            }
        }

        if let Some(end) = content_end {
            result.replace_range(content_start..content_start + end, "./data.json");
        }
    }

    result
}
