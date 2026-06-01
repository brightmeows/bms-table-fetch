use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub url: Url,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListConfig {
    pub source: Vec<Source>,
}

pub async fn load_list_config<P: AsRef<Path>>(path: P) -> anyhow::Result<ListConfig> {
    let content = tokio::fs::read_to_string(path).await?;
    let cfg: ListConfig = toml::from_str(&content)?;
    Ok(cfg)
}
