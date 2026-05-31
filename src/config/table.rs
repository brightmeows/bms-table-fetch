use std::{collections::BTreeMap, path::Path};

use bms_table::BmsTableInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct ReplaceRule {
    pub from: Url,
    pub to: Url,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TableEntry {
    #[serde(default)]
    pub name: String,
    pub url: Url,
    #[serde(default)]
    pub symbol: String,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct DisableEntry {
    pub url: Url,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TableConfig {
    #[serde(default)]
    pub table: Vec<TableEntry>,
    #[serde(default)]
    pub disable: Vec<DisableEntry>,
    #[serde(default)]
    pub replace: Vec<ReplaceRule>,
}

pub async fn load_table_config<P: AsRef<Path>>(path: P) -> anyhow::Result<TableConfig> {
    let content = tokio::fs::read_to_string(path).await?;
    let cfg: TableConfig = toml::from_str(&content)?;
    Ok(cfg)
}
