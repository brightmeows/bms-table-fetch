//! Library crate for fetching BMS table data, building lookup indexes, and managing
//! configuration-driven data pipelines.

/// CLI subcommand implementations (list, tables, index).
pub mod cmd;
/// Configuration loading and validation for list and table pipelines.
pub mod config;
/// Filesystem utilities (deep sort, change detection, filename sanitization).
pub mod filesystem;
/// Logger initialization.
pub mod logger;
/// Sync state tracking — records per-table SHA3-256 hashes and timestamps
/// to `tables/state.toml` for change audit.
pub mod state;
/// Unified sync engine orchestrator for the default pipeline.
pub mod sync;
