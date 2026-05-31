use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub url: Url,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexConfig {
    pub source: Vec<Source>,
}

pub async fn load_index_config<P: AsRef<Path>>(path: P) -> anyhow::Result<IndexConfig> {
    let content = tokio::fs::read_to_string(path).await?;
    let cfg: IndexConfig = toml::from_str(&content)?;
    Ok(cfg)
}
