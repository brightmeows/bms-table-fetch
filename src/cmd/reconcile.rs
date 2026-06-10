//! Fix directory name mismatches with table info.
//!
//! Scans all table directories and renames any whose name doesn't match
//! the `[domain] sanitize(name)` pattern derived from its `info.json`.
//!
//! Uses shared scan + pure computation from `sync` module.

use std::path::PathBuf;

use anyhow::Result;
use log::info;

use crate::sync::{self, execute_renames};

/// CLI arguments for the reconcile subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Directory containing fetched table data
    #[arg(long, default_value = "tables")]
    pub table_dir: PathBuf,
}

/// Fix directory name mismatches with table info.
///
/// # Errors
///
/// Returns an error if reading directories fails unexpectedly.
pub async fn run_reconcile(args: &Args) -> Result<()> {
    let scan = sync::scan_tables(&args.table_dir).await?;
    let renames = sync::compute_renames(&scan.entries);

    let executed = execute_renames(&renames, &args.table_dir).await;

    if executed.is_empty() {
        info!("All directory names are consistent — no rename needed");
    } else {
        info!(
            "Reconciled {} directory name(s) with table info",
            executed.len()
        );
    }

    Ok(())
}
