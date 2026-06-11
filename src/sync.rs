//! Unified sync engine — orchestrates the full default pipeline with a single directory scan.
//!
//! ## Architecture
//!
//! The default pipeline (no subcommand) runs in four phases:
//!
//! 1. **list** — fetch remote table lists → `lists/*.json`
//! 2. **overlay** — scan `tables/`, rename misnamed directories, then compute
//!    active table set (base + lists + config) → [`ActiveSet`]
//! 3. **fetch** — concurrently fetch all active tables, write to `tables/*/`
//! 4. **post_process** — second `read_dir` scan of `tables/` (first scan happens
//!    during Phase 2 overlay); reconcile (safety net), cleanup, generate `tables/tables.json`
//!    and `indexes/*.json` in parallel
//!
//! Standalone subcommands also use the pure computation functions defined here.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use bms_table::{BmsTableData, BmsTableHeader, BmsTableInfo, BmsTableRaw, fetch::reqwest::Fetcher};
use log::{info, warn};
use serde_json::Value;
use tokio::fs;
use url::Url;

use crate::{
    cmd::tables,
    config::table::load_table_config,
    filesystem::{
        clean_tmp_files, deep_sort_json_value, is_changed, sanitize_filename, write_atomic,
    },
};

// ──────────────────────────────────────────────
//  Data types
// ──────────────────────────────────────────────

/// A single table directory discovered during [`scan_tables`].
pub struct TableDirEntry {
    /// Actual directory name on disk.
    pub dir_name: String,
    /// Parsed `info.json` content.
    pub info: BmsTableInfo,
    /// Raw `data.json` content, if it exists.
    pub data_raw: Option<String>,
}

/// Result of scanning every directory under a table directory.
pub struct TableScanResult {
    /// All discovered table entries.
    pub entries: Vec<TableDirEntry>,
}

/// A pending directory rename (old name → new name).
pub struct RenameAction {
    /// Current (stale) path, relative to table base dir.
    pub old_path: PathBuf,
    /// Desired (canonical) path, relative to table base dir.
    pub new_path: PathBuf,
}

/// A pending orphan-move to `_orphaned/`.
pub struct OrphanAction {
    /// Current path, relative to table base dir.
    pub src_path: PathBuf,
}

/// Active table set after the three-layer overlay (base × lists × config).
pub struct ActiveSet {
    /// URLs that are considered "active" (should be kept, not orphaned).
    pub active_urls: HashSet<Url>,
    /// Full table info map (base + overlay + override).
    pub table_info_map: BTreeMap<Url, BmsTableInfo>,
    /// Maps each URL to its directory name from the *previous* run (for rename detection).
    pub old_dir_map: HashMap<Url, String>,
}

// ──────────────────────────────────────────────
//  Pure computations (no I/O, testable)
// ──────────────────────────────────────────────

/// Given a scan result, determine which directories need renaming to match
/// `[domain] sanitize(info.name)`.
///
/// The domain is extracted from `info.extra.url_header_json` first, falling back
/// to parsing the `[domain]` prefix from the current directory name.
#[must_use]
pub fn compute_renames(entries: &[TableDirEntry]) -> Vec<RenameAction> {
    let mut actions = Vec::new();

    for entry in entries {
        let domain: String = entry
            .info
            .extra
            .get("url_header_json")
            .and_then(|v| v.as_str())
            .and_then(|s| Url::parse(s).ok())
            .and_then(|u| u.domain().map(String::from))
            .or_else(|| {
                entry
                    .dir_name
                    .strip_prefix('[')
                    .and_then(|s| s.split_once(']'))
                    .map(|(d, _)| d.to_string())
            })
            .unwrap_or_else(|| "unknown.domain".to_string());

        let expected = sanitize_filename(&format!("[{domain}] {}", entry.info.name));

        if entry.dir_name != expected {
            // Paths are relative to base_dir; caller prepends the table root.
            actions.push(RenameAction {
                old_path: PathBuf::from(&entry.dir_name),
                new_path: PathBuf::from(&expected),
            });
        }
    }

    actions
}

/// Given a scan result and the active URL set, determine which directories
/// should be moved to `_orphaned/`.
#[must_use]
#[expect(
    clippy::implicit_hasher,
    reason = "HashSet<Url> is the canonical type for active URLs"
)]
pub fn compute_orphans(entries: &[TableDirEntry], active_urls: &HashSet<Url>) -> Vec<OrphanAction> {
    entries
        .iter()
        .filter(|e| !active_urls.contains(&e.info.url))
        .map(|e| OrphanAction {
            src_path: PathBuf::from(&e.dir_name),
        })
        .collect()
}

// ──────────────────────────────────────────────
//  I/O: shared scan and directory operations
// ──────────────────────────────────────────────

/// Scan all directories under `table_dir`, reading `info.json` and `data.json`.
///
/// Skips `_orphaned/` and any directory without a valid `info.json`.
///
/// # Errors
///
/// Returns unexpected I/O errors from reading directory entries.
pub async fn scan_tables(table_dir: &Path) -> Result<TableScanResult> {
    let mut entries = Vec::new();

    let Ok(mut dir_entries) = fs::read_dir(table_dir).await else {
        return Ok(TableScanResult { entries });
    };

    while let Some(entry) = dir_entries.next_entry().await? {
        if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        if dir_name == "_orphaned" {
            continue;
        }

        let Ok(info_content) = fs::read_to_string(path.join("info.json")).await else {
            continue;
        };
        let Ok(info) = serde_json::from_str::<BmsTableInfo>(&info_content) else {
            continue;
        };

        // data.json is optional — read if present
        let data_raw = fs::read_to_string(path.join("data.json")).await.ok();

        entries.push(TableDirEntry {
            dir_name,
            info,
            data_raw,
        });
    }

    Ok(TableScanResult { entries })
}

/// Build the three-layer overlay: base (disk) → lists → config rules.
///
/// This is the same logic as `tables::run_tables` phases 1-3, extracted so it
/// can be shared between `SyncEngine` and standalone `cleanup`.
///
/// # Errors
///
/// Returns an error if reading the table directory, list files, or config fails.
pub async fn build_active_set(
    table_dir: &Path,
    list_dir: &Path,
    list_names: &[String],
    config_path: &Path,
) -> Result<ActiveSet> {
    let (mut table_info_map, mut old_dir_map) =
        tables::load_existing_table_infos(table_dir).await?;

    let list_map = tables::load_list_files(list_dir, list_names).await?;
    if !list_map.is_empty() {
        table_info_map.extend(list_map);
    }

    if let Ok(cfg) = load_table_config(config_path).await {
        tables::apply_config(&mut table_info_map, &cfg, Some(&mut old_dir_map));
    }

    let active_urls: HashSet<Url> = table_info_map.keys().cloned().collect();
    info!("Active tables after overlay: {}", active_urls.len());

    Ok(ActiveSet {
        active_urls,
        table_info_map,
        old_dir_map,
    })
}

// ──────────────────────────────────────────────
//  SyncEngine orchestrator
// ──────────────────────────────────────────────

/// Unified pipeline orchestrator.
///
/// Construct from CLI defaults and call [`run`](Self::run) to execute the full
/// default pipeline. Each phase is also callable individually.
pub struct SyncEngine {
    // Config paths
    list_config_path: PathBuf,
    table_config_path: PathBuf,
    list_dir: PathBuf,
    table_dir: PathBuf,
    index_dir: PathBuf,
    list_names: Vec<String>,

    // Runtime
    fetcher: Arc<Fetcher>,
}

impl SyncEngine {
    /// Create a new engine with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP fetcher cannot be initialized.
    pub fn new(
        list_config_path: PathBuf,
        table_config_path: PathBuf,
        list_dir: PathBuf,
        table_dir: PathBuf,
        index_dir: PathBuf,
        list_names: Vec<String>,
    ) -> Result<Self> {
        Ok(Self {
            list_config_path,
            table_config_path,
            list_dir,
            table_dir,
            index_dir,
            list_names,
            fetcher: Arc::new(Fetcher::lenient()?),
        })
    }

    // ── Public phase methods ──

    /// Run the full default pipeline (list → fetch → post-process).
    ///
    /// # Errors
    ///
    /// Returns an error if any pipeline phase fails. Individual table fetch failures
    /// are logged as warnings and do not abort the pipeline.
    pub async fn run(&self) -> Result<()> {
        self.fetch_lists().await?;
        let active = self.build_active_set().await?;
        self.fetch_tables(&active).await?;
        self.post_process(&active).await?;
        Ok(())
    }

    /// Phase 1: fetch remote table lists and write `lists/*.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if reading the list config or writing list files fails.
    pub async fn fetch_lists(&self) -> Result<()> {
        // Delegate to existing list command logic
        let args = crate::cmd::list::Args {
            config: self.list_config_path.clone(),
            output_dir: self.list_dir.clone(),
        };
        crate::cmd::list::run_list(&args).await
    }

    /// Phase 2: compute the active table set (three-layer overlay).
    ///
    /// Scans `tables/` to discover existing tables (reading each `info.json`),
    /// renames any misnamed directories *before* building the active set, then
    /// applies the list overlay and config rules to produce the final set.
    ///
    /// Also cleans any stale `.tmp` files from previous runs.
    ///
    /// # Errors
    ///
    /// Returns an error if reading the table directory, list files, or config fails.
    ///
    /// # Panics
    ///
    /// Panics if a successfully renamed directory's new path is not a valid
    /// file name (should never happen — all directory names come from
    /// `sanitize_filename`).
    pub async fn build_active_set(&self) -> Result<ActiveSet> {
        // Clean any stale .tmp files before starting
        clean_tmp_files(&self.table_dir).await.ok();

        // ── Scan: discover existing table directories ──────────────
        let mut scan = scan_tables(&self.table_dir).await?;
        info!("Scanned {} table directories for overlay", scan.entries.len());

        // ── Rename misnamed directories before fetch ───────────────
        // This ensures that fetch writes to correctly named directories,
        // rather than fixing names only during post_process.
        let renames = compute_renames(&scan.entries);
        let executed = execute_renames(&renames, &self.table_dir).await;
        if !executed.is_empty() {
            info!(
                "Renamed {} misnamed director(ies) before fetch",
                executed.len()
            );
        }

        // Update dir_names in scan entries to reflect executed renames
        for entry in &mut scan.entries {
            if let Some(rename) = executed
                .iter()
                .find(|r| r.old_path == Path::new(&entry.dir_name))
            {
                entry.dir_name = rename
                    .new_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .expect("rename new_path is always a plain file name")
                    .to_string();
            }
        }

        // ── Build base layer from (now correctly named) scan ──────
        let mut table_info_map: BTreeMap<Url, BmsTableInfo> = BTreeMap::new();
        let mut old_dir_map: HashMap<Url, String> = HashMap::new();
        for entry in &scan.entries {
            old_dir_map.insert(entry.info.url.clone(), entry.dir_name.clone());
            table_info_map.insert(entry.info.url.clone(), entry.info.clone());
        }

        // ── Overlay lists and config (same logic as free function) ─
        let list_map = tables::load_list_files(&self.list_dir, &self.list_names).await?;
        if !list_map.is_empty() {
            table_info_map.extend(list_map);
        }

        if let Ok(cfg) = load_table_config(&self.table_config_path).await {
            tables::apply_config(&mut table_info_map, &cfg, Some(&mut old_dir_map));
        }

        let active_urls: HashSet<Url> = table_info_map.keys().cloned().collect();
        info!("Active tables after overlay: {}", active_urls.len());

        Ok(ActiveSet {
            active_urls,
            table_info_map,
            old_dir_map,
        })
    }

    /// Phase 3: concurrently fetch all active tables.
    ///
    /// # Errors
    ///
    /// Returns an error if the table output directory cannot be created.
    pub async fn fetch_tables(&self, active: &ActiveSet) -> Result<()> {
        fs::create_dir_all(&self.table_dir).await?;

        let mut join_set = tokio::task::JoinSet::new();

        for (url, info) in &active.table_info_map {
            let fetcher = Arc::clone(&self.fetcher);
            let info = info.clone();
            let url = url.clone();
            let base_dir = self.table_dir.clone();
            let old_dir = active.old_dir_map.get(&url).cloned();
            let name = info.name.clone();

            join_set.spawn(async move {
                if let Err(e) = fetch_and_save_table(&fetcher, info, &base_dir, old_dir).await {
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

    /// Phase 4: single directory scan → reconcile → cleanup → outputs.
    ///
    /// # Errors
    ///
    /// Returns an error if writing output files fails.
    ///
    /// # Panics
    ///
    /// Panics if a successfully renamed directory's new path is not a valid
    /// file name (should never happen — all directory names come from
    /// `sanitize_filename`).
    pub async fn post_process(&self, active: &ActiveSet) -> Result<()> {
        // Step 4a: single scan
        let mut scan = scan_tables(&self.table_dir).await?;
        info!("Scanned {} table directories", scan.entries.len());
        let active_urls = &active.active_urls;

        // Step 4b: reconcile (rename directories in place)
        let renames = compute_renames(&scan.entries);
        let executed = execute_renames(&renames, &self.table_dir).await;
        info!("Reconciled {} directory name(s)", executed.len());

        // Update scan entry dir_names to reflect only successfully executed renames
        for entry in &mut scan.entries {
            if let Some(rename) = executed
                .iter()
                .find(|r| r.old_path == Path::new(&entry.dir_name))
            {
                entry.dir_name = rename
                    .new_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .expect("rename new_path is always a plain file name")
                    .to_string();
            }
        }

        // Step 4c: cleanup (move orphans)
        let orphans = compute_orphans(&scan.entries, active_urls);
        let moved = execute_orphans(&orphans, &self.table_dir).await;
        if moved > 0 {
            info!("Moved {moved} orphaned director(ies) to _orphaned/");
        } else {
            info!("No orphaned directories found");
        }

        // Step 4d: generate outputs (parallel)
        let (list_result, index_result) = tokio::join!(
            self.write_combined_list(&scan, active_urls),
            self.write_indexes(&scan, active_urls),
        );
        list_result?;
        index_result?;

        // Step 4e: compute and write sync state (SHA3-256 hashes for audit)
        crate::state::compute_and_write_state(&self.table_dir, active_urls, &scan).await?;

        Ok(())
    }

    // ── Internal helpers ──

    /// Build and write `tables/tables.json` from scan results.
    async fn write_combined_list(
        &self,
        scan: &TableScanResult,
        active_urls: &HashSet<Url>,
    ) -> Result<()> {
        let output = self.table_dir.join("tables.json");

        let mut table_infos: Vec<BmsTableInfo> = scan
            .entries
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

        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).await?;
        }

        let needs_update = is_changed::<Value>(&output, &serialized, deep_sort_json_value).await?;
        if needs_update {
            write_atomic(&output, &serialized).await?;
            info!(
                "Wrote combined table list: {} ({} entries)",
                output.display(),
                new_count
            );
        } else {
            info!("tables.json is consistent ({new_count} entries) — no update needed");
        }

        Ok(())
    }

    /// Build and write `indexes/{title,artist,md5,sha256}.json` from scan results.
    async fn write_indexes(
        &self,
        scan: &TableScanResult,
        active_urls: &HashSet<Url>,
    ) -> Result<()> {
        let mut title_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut artist_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut md5_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut sha256_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for entry in &scan.entries {
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

        let to_index_map =
            |m: BTreeMap<String, BTreeSet<String>>| -> BTreeMap<String, Vec<String>> {
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

        fs::create_dir_all(&self.index_dir).await?;

        for (filename, map) in &indexes {
            let path = self.index_dir.join(filename);
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
}

// ──────────────────────────────────────────────
//  Fetch helpers
// ──────────────────────────────────────────────

pub(crate) async fn fetch_and_save_table(
    fetcher: &Fetcher,
    mut info: BmsTableInfo,
    base_dir: &Path,
    old_dir_name: Option<String>,
) -> Result<()> {
    let response = fetcher.fetch_table(info.url.as_str()).await?;
    let bms_table::BmsTable { header, data } = response.table;
    let BmsTableRaw {
        header_raw,
        data_raw,
        header_json_url,
        data_json_url,
    } = response.raw;

    let dir_name = sanitize_filename(&format!(
        "[{}] {}",
        header_json_url.domain().unwrap_or("unknown.domain"),
        header.name
    ));
    let out_dir = base_dir.join(&dir_name);

    maybe_rename_old_dir(base_dir, &dir_name, old_dir_name).await?;

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
pub(crate) fn patch_data_url(header_raw: &str) -> String {
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

pub(crate) async fn maybe_rename_old_dir(
    base_dir: &Path,
    new_dir_name: &str,
    old_dir_name: Option<String>,
) -> Result<()> {
    let Some(ref old_name) = old_dir_name else {
        return Ok(());
    };
    if old_name == new_dir_name {
        return Ok(());
    }

    let old_path = base_dir.join(old_name);
    if !fs::try_exists(&old_path).await.unwrap_or(false) {
        return Ok(());
    }

    let new_path = base_dir.join(new_dir_name);
    if fs::try_exists(&new_path).await.unwrap_or(false) {
        warn!("Removing stale directory {old_name} after name change (new: {new_dir_name})");
        fs::remove_dir_all(&old_path).await?;
    } else {
        info!("Renaming directory {old_name} -> {new_dir_name} (table name changed)");
        fs::rename(&old_path, &new_path).await?;
    }

    Ok(())
}

// ──────────────────────────────────────────────
//  Directory execution helpers
// ──────────────────────────────────────────────

/// Returns a list of actions that were actually executed (successful renames).
pub(crate) async fn execute_renames(
    actions: &[RenameAction],
    base_dir: &Path,
) -> Vec<RenameAction> {
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

pub(crate) async fn execute_orphans(actions: &[OrphanAction], base_dir: &Path) -> usize {
    if actions.is_empty() {
        return 0;
    }

    let orphan_dir = base_dir.join("_orphaned");
    if let Err(e) = fs::create_dir_all(&orphan_dir).await {
        warn!("Failed to create _orphaned/ directory: {e}");
        return 0;
    }

    let mut moved = 0;
    for action in actions {
        let src = base_dir.join(&action.src_path);
        let dst = orphan_dir.join(&action.src_path);

        if !fs::try_exists(&src).await.unwrap_or(false) {
            continue;
        }

        // Remove stale orphan
        if fs::try_exists(&dst).await.unwrap_or(false) {
            fs::remove_dir_all(&dst).await.ok();
        }

        match fs::rename(&src, &dst).await {
            Ok(()) => {
                info!(
                    "Moved orphan: {} -> _orphaned/{}",
                    action.src_path.display(),
                    action.src_path.display()
                );
                moved += 1;
            }
            Err(e) => warn!("Failed to move orphan {}: {e}", action.src_path.display()),
        }
    }
    moved
}

// ──────────────────────────────────────────────
//  Index helpers (adapted from cmd::index)
// ──────────────────────────────────────────────

/// Parse chart items from a `data.json` string, supporting two formats:
/// - Plain array: `[...]`
/// - Object with array field: `{"charts": [...]}`
pub(crate) fn extract_chart_items(content: &str) -> Option<Vec<Value>> {
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

/// Insert a value into an index map under `item[key]` if the key exists and is non-empty.
pub(crate) fn maybe_insert(
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
pub(crate) fn maybe_insert_hash(
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
}
