//! Sync engine orchestrator — coordinates the four-phase default pipeline.
//!
//! ## Pipeline phases
//!
//! 1. **list** — fetch remote table lists → `lists/*.json`
//! 2. **overlay** — scan `tables/` for rename mapping, compute active set (lists + config)
//! 3. **fetch** — concurrently fetch all active tables, write to `tables/*/`
//! 4. **post_process** — full scan → reconcile → cleanup → outputs

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bms_table::fetch::reqwest::Fetcher;
use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::fetch::{fetch_and_save_table, fetch_list_sources};
use crate::filesystem::clean_tmp_files;
use crate::orphan::{compute_orphans, execute_orphans};
use crate::output::{write_indexes, write_tables_json};
use crate::overlay::ActiveSet;
use crate::rename::{compute_renames, execute_renames};
use crate::scan;

/// Unified pipeline orchestrator.
///
/// Construct with configuration paths and call [`run`](Self::run) to execute
/// the full default pipeline.
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

    /// Run the full default pipeline (list → overlay → fetch → post-process).
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
    /// Individual source failures are logged and do not abort the phase.
    /// Old list files are preserved when a source fails (cache-on-failure).
    ///
    /// # Errors
    ///
    /// Returns an error if reading the list config fails.
    pub async fn fetch_lists(&self) -> Result<()> {
        let config = crate::config::list::load_list_config(&self.list_config_path).await?;
        fetch_list_sources(&config.source, &self.list_dir).await
    }

    /// Phase 2: compute the active table set and pre-rename directories.
    ///
    /// Scans `tables/` once for rename mapping, builds the active set from
    /// lists + config, then renames any misnamed directories.
    ///
    /// # Errors
    ///
    /// Returns an error if reading the table directory, list files, or config fails.
    pub async fn build_active_set(&self) -> Result<ActiveSet> {
        // Clean any stale .tmp files before starting
        clean_tmp_files(&self.table_dir).await.ok();

        // Single scan for old_dir_map and base info layer
        let entries = scan::scan_dirs(&self.table_dir).await?;
        info!("Scanned {} table directories for overlay", entries.len());

        let base_info_map: std::collections::BTreeMap<Url, bms_table::BmsTableInfo> = entries
            .iter()
            .map(|e| (e.info.url.clone(), e.info.clone()))
            .collect();
        let old_dir_map: std::collections::HashMap<Url, String> = entries
            .iter()
            .map(|e| (e.info.url.clone(), e.dir_name.clone()))
            .collect();

        // Build active set from base + lists + config
        let active = crate::overlay::build_active_set(
            base_info_map,
            old_dir_map,
            &self.list_dir,
            &self.list_names,
            &self.table_config_path,
        )
        .await?;

        // Pre-fetch rename using scan entries + overlaid info
        let renames = compute_renames(&entries, Some(&active.table_info_map));
        let executed = execute_renames(&renames, &self.table_dir).await;
        if !executed.is_empty() {
            info!(
                "Renamed {} misnamed director(ies) before fetch",
                executed.len()
            );
        }

        Ok(active)
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

    /// Phase 4: post-process — reconcile, cleanup, generate outputs.
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
        // Full scan
        let mut entries = scan::scan_dirs_full(&self.table_dir).await?;
        info!("Scanned {} table directories", entries.len());

        // Reconcile (safety net rename using disk info.json as truth)
        let renames = compute_renames(&entries, None::<&std::collections::BTreeMap<_, _>>);
        let executed = execute_renames(&renames, &self.table_dir).await;
        info!("Reconciled {} directory name(s)", executed.len());

        // Update scan entry dir_names to reflect executed renames
        for entry in &mut entries {
            if let Some(rename) = executed
                .iter()
                .find(|r| r.old_path == std::path::Path::new(&entry.dir_name))
            {
                entry.dir_name = rename
                    .new_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .expect("rename new_path is always a plain file name")
                    .to_string();
            }
        }

        // Cleanup (move orphans)
        let orphans = compute_orphans(&entries, &active.active_urls);
        let moved = execute_orphans(&orphans, &self.table_dir).await;
        if moved > 0 {
            info!("Moved {moved} orphaned director(ies) to _orphaned/");
        } else {
            info!("No orphaned directories found");
        }

        // Generate outputs (parallel)
        let tables_json_path = self.table_dir.join("tables.json");
        let (list_result, index_result) = tokio::join!(
            write_tables_json(&tables_json_path, &entries, &active.active_urls),
            write_indexes(&self.index_dir, &entries, &active.active_urls),
        );
        list_result?;
        index_result?;

        // Compute and write sync state (SHA3-256 hashes for audit)
        crate::state::compute_and_write_state(&self.table_dir, &active.active_urls, &entries)
            .await?;

        Ok(())
    }
}
