//! CLI entry point for bms-table-fetch.

use anyhow::Result;
use bms_table_fetch::cmd::{
    self, cleanup::Args as CleanupArgs, index::Args as IndexArgs, list::Args as ListArgs,
    reconcile::Args as ReconcileArgs, tables::Args as TablesArgs,
    tables_list::Args as TablesListArgs,
};
use clap::{Parser, Subcommand};
use log::info;

/// Fetch table lists and/or table data from BMS table sources.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch table lists from configured sources and save as unified JSON.
    List(ListArgs),
    /// Fetch table header/data from list results.
    Tables(TablesArgs),
    /// Fix directory name mismatches with table info (rename to match info.json).
    Reconcile(ReconcileArgs),
    /// Move orphaned table directories (no longer in any list) to _orphaned/.
    Cleanup(CleanupArgs),
    /// Build lookup indexes (title/artist/md5/sha256 -> table names) from fetched table data.
    Index(IndexArgs),
    /// Generate a combined table list from fetched table info.json files.
    TablesList(TablesListArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    bms_table_fetch::logger::init_logger();

    let cli = Cli::parse();

    match cli.command {
        None => {
            // No subcommand: run full pipeline with defaults
            cmd::list::run_list(&ListArgs {
                config: "config/list.toml".into(),
                output_dir: "lists".into(),
            })
            .await?;

            cmd::reconcile::run_reconcile(&ReconcileArgs {
                table_dir: "tables".into(),
            })
            .await?;

            cmd::tables::run_tables(&TablesArgs {
                config: "config/table.toml".into(),
                list_dir: "lists".into(),
                list_names: vec![],
                output_dir: "tables".into(),
            })
            .await?;

            cmd::cleanup::run_cleanup(&CleanupArgs {
                config: "config/table.toml".into(),
                list_dir: "lists".into(),
                list_names: vec![],
                table_dir: "tables".into(),
            })
            .await?;

            cmd::tables_list::run_tables_list(&TablesListArgs {
                table_dir: "tables".into(),
                output: "tables/tables.json".into(),
            })
            .await?;

            cmd::index::run_index(&IndexArgs {
                table_dir: "tables".into(),
                output_dir: "indexes".into(),
            })
            .await?;
        }
        Some(Command::List(args)) => {
            cmd::list::run_list(&args).await?;
        }
        Some(Command::Tables(args)) => {
            cmd::tables::run_tables(&args).await?;
        }
        Some(Command::Reconcile(args)) => {
            cmd::reconcile::run_reconcile(&args).await?;
        }
        Some(Command::Cleanup(args)) => {
            cmd::cleanup::run_cleanup(&args).await?;
        }
        Some(Command::Index(args)) => {
            cmd::index::run_index(&args).await?;
        }
        Some(Command::TablesList(args)) => {
            cmd::tables_list::run_tables_list(&args).await?;
        }
    }

    info!("All commands completed.");
    Ok(())
}
