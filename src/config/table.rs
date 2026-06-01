//! Configuration types for the table pipeline: replacement rules, add/disable rules, and loading.

use std::{collections::BTreeMap, path::Path};

use bms_table::BmsTableInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// A URL replacement rule: replace all occurrences of `from` with `to`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplaceRule {
    /// Original URL pattern to replace.
    pub from: Url,
    /// Replacement URL.
    pub to: Url,
}

/// A single table entry with metadata and a download URL.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TableEntry {
    /// Human-readable table name (optional).
    #[serde(default)]
    pub name: String,
    /// URL to download the table from.
    pub url: Url,
    /// Optional short symbol for the table (e.g. "LR2").
    #[serde(default)]
    pub symbol: String,
    /// Additional arbitrary fields flattened into the TOML/JSON structure.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl From<TableEntry> for BmsTableInfo {
    fn from(v: TableEntry) -> Self {
        Self {
            name: v.name,
            url: v.url,
            symbol: v.symbol,
            extra: v.extra,
        }
    }
}

impl From<BmsTableInfo> for TableEntry {
    fn from(v: BmsTableInfo) -> Self {
        Self {
            name: v.name,
            url: v.url,
            symbol: v.symbol,
            extra: v.extra,
        }
    }
}

/// A table to disable, identified by its URL.
#[derive(Debug, Serialize, Deserialize)]
pub struct DisableEntry {
    /// URL of the table to disable.
    pub url: Url,
}

/// Complete table configuration: active entries, disabled entries, and replace rules.
#[derive(Debug, Serialize, Deserialize)]
pub struct TableConfig {
    /// Active table entries to fetch.
    #[serde(default)]
    pub table: Vec<TableEntry>,
    /// Entries to disable (will not be fetched).
    #[serde(default)]
    pub disable: Vec<DisableEntry>,
    /// URL replacement rules applied during fetching.
    #[serde(default)]
    pub replace: Vec<ReplaceRule>,
}

/// Load a [`TableConfig`] from a TOML file at the given path.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the TOML content is invalid.
pub async fn load_table_config<P: AsRef<Path>>(path: P) -> anyhow::Result<TableConfig> {
    let content = tokio::fs::read_to_string(path).await?;
    let cfg: TableConfig = toml::from_str(&content)?;
    Ok(cfg)
}
