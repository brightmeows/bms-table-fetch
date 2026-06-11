//! Output generation: write `tables/tables.json` and `indexes/*.json`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use anyhow::Result;
use bms_table::BmsTableInfo;
use log::info;
use serde_json::Value;
use tokio::fs;
use url::Url;

use crate::filesystem::{deep_sort_json_value, is_changed, write_atomic};
use crate::index::{extract_chart_items, maybe_insert, maybe_insert_hash};
use crate::scan::FullDirEntry;

/// Write `tables/tables.json` — combined list of all active table infos.
///
/// Each entry gets a `dir_name` field added to its `extra`.
/// Entries are sorted by URL for deterministic output.
///
/// # Errors
///
/// Returns an error if writing the output file fails.
#[expect(
    clippy::implicit_hasher,
    reason = "HashSet<Url> is the canonical type for active URLs"
)]
pub async fn write_tables_json(
    output_path: &Path,
    entries: &[FullDirEntry],
    active_urls: &HashSet<Url>,
) -> Result<()> {
    let mut table_infos: Vec<BmsTableInfo> = entries
        .iter()
        .filter(|e| active_urls.contains(&e.info.url))
        .map(|e| {
            let mut info = e.info.clone();
            info.extra
                .insert("dir_name".to_string(), Value::String(e.dir_name.clone()));
            info
        })
        .collect();
    // Sort by URL for deterministic output order
    table_infos.sort_by(|a, b| a.url.cmp(&b.url));

    let serialized = serde_json::to_string_pretty(&table_infos)?;
    let new_count = table_infos.len();

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    if is_changed::<Value>(output_path, &serialized, deep_sort_json_value).await? {
        write_atomic(output_path, &serialized).await?;
        info!(
            "Wrote combined table list: {} ({} entries)",
            output_path.display(),
            new_count
        );
    } else {
        info!("tables.json is consistent ({new_count} entries) — no update needed");
    }

    Ok(())
}

/// Write `indexes/{title,artist,md5,sha256}.json` — inverted indexes from table data.
///
/// # Errors
///
/// Returns an error if writing index files fails.
#[expect(
    clippy::implicit_hasher,
    reason = "HashSet<Url> is the canonical type for active URLs"
)]
pub async fn write_indexes(
    index_dir: &Path,
    entries: &[FullDirEntry],
    active_urls: &HashSet<Url>,
) -> Result<()> {
    let mut title_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut artist_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut md5_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut sha256_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for entry in entries {
        if !active_urls.contains(&entry.info.url) {
            continue;
        }

        let Some(ref data_raw) = entry.data_raw else {
            continue;
        };

        let Some(items) = extract_chart_items(data_raw) else {
            info!(
                "data.json format unrecognized in {} — skipping",
                entry.dir_name
            );
            continue;
        };

        for item in &items {
            maybe_insert(&mut title_map, item, "title", &entry.dir_name);
            maybe_insert(&mut artist_map, item, "artist", &entry.dir_name);
            maybe_insert_hash(&mut md5_map, item, "md5", 32, &entry.dir_name);
            maybe_insert_hash(&mut sha256_map, item, "sha256", 64, &entry.dir_name);
        }
    }

    let to_index_map = |m: BTreeMap<String, BTreeSet<String>>| -> BTreeMap<String, Vec<String>> {
        m.into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    };

    let indexes: [(&str, BTreeMap<String, Vec<String>>); 4] = [
        ("title.json", to_index_map(title_map)),
        ("artist.json", to_index_map(artist_map)),
        ("md5.json", to_index_map(md5_map)),
        ("sha256.json", to_index_map(sha256_map)),
    ];

    fs::create_dir_all(index_dir).await?;

    for (filename, map) in &indexes {
        let path = index_dir.join(filename);
        let serialized = serde_json::to_string_pretty(map)?;
        if is_changed::<Value>(&path, &serialized, deep_sort_json_value).await? {
            write_atomic(&path, &serialized).await?;
            info!("Wrote index: {} ({} entries)", path.display(), map.len());
        } else {
            info!(
                "Index {filename} unchanged ({} entries) — skipping write",
                map.len()
            );
        }
    }

    Ok(())
}
