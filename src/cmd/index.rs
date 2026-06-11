//! Build and write inverted indexes (title/artist/md5/sha256) from fetched table data.

use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;
use log::info;
use url::Url;

use crate::output;
use crate::scan;

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
/// Indexes ALL table directories found on disk, regardless of active status.
///
/// # Errors
///
/// Returns an error if reading table data or writing index files fails.
pub async fn run_index(args: &Args) -> Result<()> {
    let entries = scan::scan_dirs_full(&args.table_dir).await?;
    info!("Loaded {} table entries for indexing", entries.len());

    // Index all entries (no active URL filtering for standalone subcommand)
    let all_urls: HashSet<Url> = entries.iter().map(|e| e.info.url.clone()).collect();

    output::write_indexes(&args.output_dir, &entries, &all_urls).await?;

    info!("Index build completed.");
    Ok(())
}
