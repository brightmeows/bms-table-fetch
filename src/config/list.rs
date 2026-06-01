use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

/// A table source definition with a human-readable name and download URL.
#[derive(Debug, Serialize, Deserialize)]
pub struct Source {
    /// Display name for the source.
    pub name: String,
    /// URL to download the table from.
    pub url: Url,
}

/// Configuration listing all table sources to fetch.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListConfig {
    /// List of sources to download.
    pub source: Vec<Source>,
}

/// Load a [`ListConfig`] from a TOML file at the given path.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the TOML content is invalid.
pub async fn load_list_config<P: AsRef<Path>>(path: P) -> anyhow::Result<ListConfig> {
    let content = tokio::fs::read_to_string(path).await?;
    let cfg: ListConfig = toml::from_str(&content)?;
    Ok(cfg)
}
