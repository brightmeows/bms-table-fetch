use std::collections::BTreeMap;
use std::path::Path;

use bms_table::BmsTableInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct TableListSource {
    pub name: String,
    pub url: Url,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AddTableInfo {
    #[serde(default)]
    pub name: String,
    pub url: Url,
    #[serde(default)]
    pub symbol: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl From<AddTableInfo> for BmsTableInfo {
    fn from(v: AddTableInfo) -> Self {
        Self {
            name: v.name,
            url: v.url,
            symbol: v.symbol,
            extra: v.extra,
        }
    }
}

impl From<BmsTableInfo> for AddTableInfo {
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
pub struct UrlReplaceRule {
    pub from: Url,
    pub to: Url,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TableConfig {
    pub table_list: Vec<TableListSource>,
    #[serde(default)]
    pub add_table: Vec<AddTableInfo>,
    #[serde(default)]
    pub disable_table_url: Vec<Url>,
    #[serde(default)]
    pub replace_table_url: Vec<UrlReplaceRule>,
}

pub async fn load_table_config<P: AsRef<Path>>(path: P) -> anyhow::Result<TableConfig> {
    let content = tokio::fs::read_to_string(path).await?;
    let cfg: TableConfig = toml::from_str(&content)?;
    Ok(cfg)
}
