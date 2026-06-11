//! Fix directory name mismatches with table info.
//!
//! Scans all table directories and renames any whose name doesn't match
//! the `[domain] sanitize(name)` pattern derived from its `info.json`.

use std::path::PathBuf;

use anyhow::Result;
use log::info;

use crate::rename;
use crate::scan;

/// CLI arguments for the reconcile subcommand.
#[derive(clap::Args)]
pub struct Args {
    /// Directory containing fetched table data
    #[arg(long, default_value = "tables")]
    pub table_dir: PathBuf,
}

/// Fix directory name mismatches with table info.
///
/// Uses the disk `info.json` for each directory to compute the expected name.
/// No overlaid (list/config) info is applied — this is a direct disk-based fix.
///
/// # Errors
///
/// Returns an error if reading directories fails unexpectedly.
pub async fn run_reconcile(args: &Args) -> Result<()> {
    let entries = scan::scan_dirs(&args.table_dir).await?;
    let renames = rename::compute_renames(&entries, None::<&std::collections::BTreeMap<_, _>>);

    let executed = rename::execute_renames(&renames, &args.table_dir).await;

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
