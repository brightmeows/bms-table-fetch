//! Library crate for fetching BMS table data, building lookup indexes, and managing
//! configuration-driven data pipelines.

/// CLI subcommand implementations (list, tables, index).
pub mod cmd;
/// Configuration loading and validation for list and table pipelines.
pub mod config;
/// Unified sync engine orchestrator for the default pipeline.
pub mod engine;
/// HTTP fetch and persistence: fetch table data and list data from remote, save to disk.
pub mod fetch;
/// Filesystem utilities (deep sort, change detection, filename sanitization).
pub mod filesystem;
/// Index building helpers: parse chart data and build inverted indexes.
pub mod index;
/// Logger initialization.
pub mod logger;
/// Orphan directory handling: compute and move directories not in the active set.
pub mod orphan;
/// Output generation: write `tables/tables.json` and `indexes/*.json`.
pub mod output;
/// Active set computation: build the list of tables to fetch from lists + config.
pub mod overlay;
/// Directory rename logic: compute expected directory names and execute renames.
pub mod rename;
/// Directory scanning: discover table directories and read their metadata.
pub mod scan;
/// Sync state tracking — records per-table SHA3-256 hashes and timestamps
/// to `tables/state.toml` for change audit.
pub mod state;
