//! Build and write inverted indexes (title/artist/md5/sha256) from fetched table data.
//!
//! Uses shared scan and helper functions from `sync` module.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::Result;
use log::info;
use serde_json::Value;
use tokio::fs;

use crate::{
    filesystem::{deep_sort_json_value, is_changed, write_atomic},
    sync::{self, maybe_insert, maybe_insert_hash},
};

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
        if is_changed::<Value>(&path, &serialized, deep_sort_json_value).await? {
            write_atomic(&path, &serialized).await?;
            info!("Wrote index: {} ({} entries)", path.display(), map.len());
        } else {
            info!(
                "Index {} unchanged ({} entries) — skipping write",
                path.display(),
                map.len()
            );
        }
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

    let scan = sync::scan_tables(table_dir).await?;

    for entry in &scan.entries {
        let Some(ref data_raw) = entry.data_raw else {
            continue;
        };

        let Some(items) = sync::extract_chart_items(data_raw) else {
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

    let to_vec = |m: BTreeMap<String, BTreeSet<String>>| {
        m.into_iter()
            .map(|(k, v)| (k, v.into_iter().collect::<Vec<_>>()))
            .collect::<BTreeMap<_, _>>()
    };

    Ok((
        to_vec(title_map),
        to_vec(artist_map),
        to_vec(md5_map),
        to_vec(sha256_map),
    ))
}
