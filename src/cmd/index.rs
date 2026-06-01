use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::Result;
use log::{info, warn};
use serde_json::Value;
use tokio::fs;

/// Map from a lookup key (title, artist, md5, sha256) to the list of table directories containing it.
type IndexMap = BTreeMap<String, Vec<String>>;

/// CLI arguments for the index subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Directory containing fetched table data
    #[arg(long, default_value = "tables")]
    pub table_dir: PathBuf,

    /// Output directory for index JSON files
    #[arg(long, default_value = "indexes")]
    pub output_dir: PathBuf,
}

/// Build lookup indexes (title/artist/md5/sha256 -> table names) from fetched table data.
///
/// # Errors
///
/// Returns an error if reading table data or writing index files fails.
pub async fn run_index(args: &Args) -> Result<()> {
    let (title_map, artist_map, md5_map, sha256_map) =
        build_index_from_tables(&args.table_dir).await?;

    fs::create_dir_all(&args.output_dir).await?;

    let output = &args.output_dir;

    let data: [(&str, &IndexMap); 4] = [
        ("title.json", &title_map),
        ("artist.json", &artist_map),
        ("md5.json", &md5_map),
        ("sha256.json", &sha256_map),
    ];

    for (filename, map) in data {
        let path = output.join(filename);
        let serialized = serde_json::to_string_pretty(map)?;
        fs::write(&path, &serialized).await?;
        info!("Wrote index: {} ({} entries)", path.display(), map.len());
    }

    info!("Index build completed.");
    Ok(())
}

/// Build inverted indexes from fetched table data.
///
/// For each table directory under `table_dir`, reads `data.json` and builds
/// maps from (title / artist / md5 / sha256) to the set of table directory names
/// that contain a matching entry.
async fn build_index_from_tables(
    table_dir: &Path,
) -> Result<(IndexMap, IndexMap, IndexMap, IndexMap)> {
    let mut title_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut artist_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut md5_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut sha256_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    match fs::try_exists(table_dir).await {
        Ok(true) => {}
        Ok(false) => {
            warn!(
                "Table directory {} does not exist — building empty index",
                table_dir.display()
            );
            return Ok(convert_sets_to_vecs(
                title_map, artist_map, md5_map, sha256_map,
            ));
        }
        Err(e) => {
            warn!(
                "Failed to check table directory {}: {e} — building empty index",
                table_dir.display()
            );
            return Ok(convert_sets_to_vecs(
                title_map, artist_map, md5_map, sha256_map,
            ));
        }
    }

    let mut dir_entries = fs::read_dir(table_dir).await?;
    while let Some(entry) = dir_entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let table_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        let data_path = path.join("data.json");
        let content = match fs::read_to_string(&data_path).await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Failed to read {} (table: {table_name}): {e}",
                    data_path.display()
                );
                continue;
            }
        };

        let items: Vec<Value> = if let Some(v) = extract_chart_items(&content) {
            v
        } else {
            warn!(
                "data.json format unrecognized in {} (table: {table_name}): expected array or object with charts array",
                data_path.display(),
            );
            continue;
        };

        for item in &items {
            maybe_insert(&mut title_map, item, "title", &table_name);
            maybe_insert(&mut artist_map, item, "artist", &table_name);
            maybe_insert_hash(&mut md5_map, item, "md5", 32, &table_name);
            maybe_insert_hash(&mut sha256_map, item, "sha256", 64, &table_name);
        }
    }

    Ok(convert_sets_to_vecs(
        title_map, artist_map, md5_map, sha256_map,
    ))
}

/// Parse `content` as a JSON array of chart entries, supporting two formats:
/// - Plain array: `[...]`
/// - Object with array field: `{"charts": [...]}` or `{"data": [...]}`
fn extract_chart_items(content: &str) -> Option<Vec<Value>> {
    // Fast path: plain array
    if let Ok(v) = serde_json::from_str::<Vec<Value>>(content) {
        return Some(v);
    }

    // Fallback: object with a container key
    let root: Value = serde_json::from_str(content).ok()?;
    let obj = root.as_object()?;
    for key in &["charts", "data", "songs"] {
        if let Some(arr) = obj.get(*key).and_then(|v| v.as_array()) {
            return Some(arr.clone());
        }
    }
    None
}

/// If `item[key]` is a non-empty string, insert `value` into the map under that key.
fn maybe_insert(
    map: &mut BTreeMap<String, BTreeSet<String>>,
    item: &Value,
    key: &str,
    value: &str,
) {
    if let Some(s) = item
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        map.entry(s.to_string())
            .or_default()
            .insert(value.to_string());
    }
}

/// Insert a validated hash value into the index map.
///
/// Only accepts hex strings of exactly `expected_len` characters.
/// If the string is hex but longer than `expected_len`, warns and skips.
/// Non-hex or wrong-length strings are silently skipped.
fn maybe_insert_hash(
    map: &mut BTreeMap<String, BTreeSet<String>>,
    item: &Value,
    key: &str,
    expected_len: usize,
    table_name: &str,
) {
    let raw = match item.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    let is_all_hex = raw.chars().all(|c| c.is_ascii_hexdigit());

    if raw.len() == expected_len && is_all_hex {
        map.entry(raw.to_string())
            .or_default()
            .insert(table_name.to_string());
    } else if raw.len() > expected_len && is_all_hex {
        let truncated = if raw.len() > 64 {
            format!("{}… ({} chars total)", &raw[..64], raw.len())
        } else {
            raw.to_string()
        };
        warn!(
            "Suspiciously long {key} hash (expected {expected_len}) in table {table_name}: {truncated}",
        );
    }
    // Otherwise silently skip (non-hex garbage, too short, etc.)
}

/// Convert `BTreeSet` values to sorted Vec for JSON serialization.
fn convert_sets_to_vecs(
    title_map: BTreeMap<String, BTreeSet<String>>,
    artist_map: BTreeMap<String, BTreeSet<String>>,
    md5_map: BTreeMap<String, BTreeSet<String>>,
    sha256_map: BTreeMap<String, BTreeSet<String>>,
) -> (IndexMap, IndexMap, IndexMap, IndexMap) {
    let to_vec = |m: BTreeMap<String, BTreeSet<String>>| {
        m.into_iter()
            .map(|(k, v)| (k, v.into_iter().collect::<Vec<_>>()))
            .collect::<BTreeMap<_, _>>()
    };
    (
        to_vec(title_map),
        to_vec(artist_map),
        to_vec(md5_map),
        to_vec(sha256_map),
    )
}
