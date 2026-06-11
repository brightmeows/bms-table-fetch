//! Sync state tracking — records per-table SHA3-256 hashes and timestamps
//! to `tables/state.toml` for change audit.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::warn;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use tokio::fs;
use url::Url;

use crate::filesystem::write_atomic;
use crate::scan::FullDirEntry;

/// A SHA3-256 hash stored as raw bytes, serialized as a lowercase hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha3Hash([u8; 32]);

impl Sha3Hash {
    /// Compute the SHA3-256 hash of `data`.
    #[must_use]
    pub fn new(data: &[u8]) -> Self {
        let mut hasher = Sha3_256::new();
        hasher.update(data);
        let bytes: [u8; 32] = hasher.finalize().into();
        Self(bytes)
    }

    /// Parse a 64-character lowercase hex string into a SHA3-256 hash.
    ///
    /// # Errors
    ///
    /// Returns an error string if the input is not 64 hex characters.
    fn from_hex(s: &str) -> Result<Self, String> {
        if s.len() != 64 {
            return Err(format!("expected 64 hex characters, got {}", s.len()));
        }
        let mut bytes = [0u8; 32];
        for (byte, chunk) in bytes.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(chunk)
                .map_err(|_| "invalid UTF-8 in hex string".to_string())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| format!("invalid hex: {pair}"))?;
        }
        Ok(Self(bytes))
    }
}

impl std::fmt::Display for Sha3Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Sha3Hash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

struct Sha3HashVisitor;

impl serde::de::Visitor<'_> for Sha3HashVisitor {
    type Value = Sha3Hash;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a 64-character lowercase hex string (SHA3-256)")
    }

    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Sha3Hash, E> {
        Sha3Hash::from_hex(s).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for Sha3Hash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(Sha3HashVisitor)
    }
}

/// SHA3-256 hashes of the three tracked files, grouped for ergonomic access.
///
/// Flattened into the parent `TableState` via `#[serde(flatten)]`, so TOML
/// keys remain flat (`sha3_256_info`, `sha3_256_header`, `sha3_256_data`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Sha3Hashes {
    /// SHA3-256 hash of `info.json`.
    #[serde(rename = "sha3_256_info")]
    pub info: Sha3Hash,
    /// SHA3-256 hash of `header.json`.
    #[serde(rename = "sha3_256_header")]
    pub header: Sha3Hash,
    /// SHA3-256 hash of `data.json`.
    #[serde(rename = "sha3_256_data")]
    pub data: Sha3Hash,
}

/// Top-level sync state written to `tables/state.toml`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SyncState {
    /// Global sync metadata.
    pub global: GlobalState,
    /// Per-table state, keyed by table URL.
    pub tables: BTreeMap<String, TableState>,
}

/// Global sync metadata.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GlobalState {
    /// ISO 8601 timestamp of the last completed sync.
    pub last_sync: DateTime<Utc>,
}

/// Per-table state: SHA3-256 hashes of the three tracked files and timestamps.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TableState {
    /// ISO 8601 timestamp when this table was last checked (this sync run).
    pub last_check: DateTime<Utc>,
    /// ISO 8601 timestamp when any of the three files last changed.
    pub last_change: DateTime<Utc>,
    /// Grouped SHA3-256 hashes (flattened into `sha3_256_*` TOML keys).
    #[serde(flatten)]
    pub sha3_256: Sha3Hashes,
}

/// Read the full content of a file, returning `None` on any I/O error.
#[must_use]
async fn try_read_string(path: &Path) -> Option<String> {
    fs::read_to_string(path).await.ok()
}

/// Compute and atomically write `tables/state.toml`.
///
/// Reads the old state (if it exists) to determine `last_change`:
/// if all three file hashes match the previous run, `last_change` is preserved;
/// otherwise it is updated to the current time.
///
/// Only tables whose URL is in `active_urls` are included.
/// Tables whose files cannot be read are skipped with a warning.
///
/// # Errors
///
/// Returns an error if the state cannot be serialized to TOML or written atomically.
/// Individual table failures are logged and do not abort the process.
#[expect(
    clippy::implicit_hasher,
    reason = "HashSet<Url> is the canonical type for active URLs"
)]
pub async fn compute_and_write_state(
    table_dir: &Path,
    active_urls: &HashSet<Url>,
    entries: &[FullDirEntry],
) -> Result<()> {
    let path = table_dir.join("state.toml");

    // Read old state — silently treat missing/unreadable as fresh start.
    let old_state: Option<SyncState> = fs::read_to_string(&path)
        .await
        .ok()
        .and_then(|content| toml::from_str(&content).ok());

    let now = Utc::now();
    let mut tables = BTreeMap::new();

    for entry in entries {
        if !active_urls.contains(&entry.info.url) {
            continue;
        }

        let dir = table_dir.join(&entry.dir_name);

        let Some(info_content) = try_read_string(&dir.join("info.json")).await else {
            warn!("state: skipped {} — info.json not readable", entry.dir_name);
            continue;
        };
        let Some(header_content) = try_read_string(&dir.join("header.json")).await else {
            warn!(
                "state: skipped {} — header.json not readable",
                entry.dir_name
            );
            continue;
        };
        let Some(data_content) = try_read_string(&dir.join("data.json")).await else {
            warn!("state: skipped {} — data.json not readable", entry.dir_name);
            continue;
        };

        let sha3_256 = Sha3Hashes {
            info: Sha3Hash::new(info_content.as_bytes()),
            header: Sha3Hash::new(header_content.as_bytes()),
            data: Sha3Hash::new(data_content.as_bytes()),
        };

        // Preserve last_change if all three file hashes are unchanged.
        let last_change = old_state
            .as_ref()
            .and_then(|s| s.tables.get(entry.info.url.as_str()))
            .filter(|old_ts| old_ts.sha3_256 == sha3_256)
            .map_or(now, |old_ts| old_ts.last_change);

        tables.insert(
            entry.info.url.to_string(),
            TableState {
                last_check: now,
                last_change,
                sha3_256,
            },
        );
    }

    let state = SyncState {
        global: GlobalState { last_sync: now },
        tables,
    };

    let content = toml::to_string(&state).context("failed to serialize sync state")?;

    // Ensure parent directory exists before writing.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    write_atomic(&path, &content).await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use url::Url;

    use crate::scan::FullDirEntry;

    use super::*;

    /// Create a minimal `BmsTableInfo` with the given URL.
    #[must_use]
    fn make_info(url: &str) -> bms_table::BmsTableInfo {
        bms_table::BmsTableInfo {
            name: "test".to_string(),
            symbol: "★".to_string(),
            url: Url::parse(url).unwrap(),
            extra: BTreeMap::new(),
        }
    }

    /// Write content to a file.
    async fn write_test_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        write_atomic(&path, content).await.unwrap();
    }

    /// Create a table directory with three files, returning a `FullDirEntry`.
    async fn create_table_dir(
        base: &Path,
        dir_name: &str,
        info: &str,
        header: &str,
        data: &str,
    ) -> FullDirEntry {
        let dir = base.join(dir_name);
        tokio::fs::create_dir_all(&dir).await.unwrap();

        write_test_file(&dir, "info.json", info).await;
        write_test_file(&dir, "header.json", header).await;
        write_test_file(&dir, "data.json", data).await;

        FullDirEntry {
            dir_name: dir_name.to_string(),
            info: make_info(&format!("https://example.com/{dir_name}")),
            data_raw: Some(data.to_string()),
        }
    }

    /// Read the content of `state.toml` under `base`.
    async fn read_state(base: &Path) -> SyncState {
        let content = tokio::fs::read_to_string(base.join("state.toml"))
            .await
            .unwrap();
        toml::from_str(&content).unwrap()
    }

    /// Build the URL string that `make_info` produces for a given path.
    fn test_url(path: &str) -> String {
        Url::parse(&format!("https://example.com/{path}"))
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn test_creates_state_toml() {
        let dir = PathBuf::from("/tmp/opencode/test_creates_state_toml");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let url_str = test_url("foo");
        let entry = create_table_dir(&dir, "foo", r#"{"v":1}"#, r"[]", r"{}").await;
        let active = {
            let mut s = HashSet::new();
            s.insert(Url::parse(&url_str).unwrap());
            s
        };

        compute_and_write_state(&dir, &active, &[entry])
            .await
            .unwrap();

        let state = read_state(&dir).await;
        assert_eq!(state.tables.len(), 1);

        let ts = state.tables.get(&url_str).unwrap();
        // Sha3Hash stores 32 raw bytes, which serialize to 64 hex chars.
        let hex = ts.sha3_256.info.to_string();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(ts.last_check, ts.last_change); // first run → change = check

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn test_tracks_change() {
        let dir = PathBuf::from("/tmp/opencode/test_tracks_change");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let url_str = test_url("bar");
        let entry = create_table_dir(&dir, "bar", r#"{"k":"a"}"#, r"[]", r"{}").await;
        let entries = [entry];
        let active = {
            let mut s = HashSet::new();
            s.insert(Url::parse(&url_str).unwrap());
            s
        };

        // First run
        compute_and_write_state(&dir, &active, &entries)
            .await
            .unwrap();
        let state1 = read_state(&dir).await;
        let ts1 = state1.tables.get(&url_str).unwrap();

        // Small delay to ensure timestamp advances between runs
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        // Second run — same content, last_change should be preserved
        compute_and_write_state(&dir, &active, &entries)
            .await
            .unwrap();
        let state2 = read_state(&dir).await;
        let ts2 = state2.tables.get(&url_str).unwrap();

        assert_eq!(ts1.last_change, ts2.last_change);
        assert!(ts2.last_check > ts1.last_check);

        // Third run — change info.json content
        let entry = create_table_dir(&dir, "bar", r#"{"k":"b"}"#, r"[]", r"{}").await;

        // Small delay to ensure timestamp advances
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        compute_and_write_state(&dir, &active, &[entry])
            .await
            .unwrap();
        let state3 = read_state(&dir).await;
        let ts3 = state3.tables.get(&url_str).unwrap();

        assert!(ts3.last_change > ts2.last_change);
        assert_eq!(ts3.last_change, ts3.last_check);

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn test_skips_inactive() {
        let dir = PathBuf::from("/tmp/opencode/test_skips_inactive");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let url_active = test_url("active");
        let entry = create_table_dir(&dir, "active", r"{}", r"[]", r"{}").await;
        let entry2 = create_table_dir(&dir, "inactive", r"{}", r"[]", r"{}").await;
        let mut active = HashSet::new();
        active.insert(Url::parse(&url_active).unwrap());

        compute_and_write_state(&dir, &active, &[entry, entry2])
            .await
            .unwrap();

        let state = read_state(&dir).await;
        assert_eq!(state.tables.len(), 1);
        assert!(state.tables.contains_key(&url_active));
        assert!(!state.tables.contains_key(&test_url("inactive")));

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn test_handles_missing_files() {
        let dir = PathBuf::from("/tmp/opencode/test_handles_missing_files");
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let url_partial = test_url("partial");
        let url_complete = test_url("complete");

        // Create directory but only info.json
        let table_dir = dir.join("partial");
        tokio::fs::create_dir_all(&table_dir).await.unwrap();
        write_test_file(&table_dir, "info.json", r"{}").await;

        // Also create a complete one
        let entry2 = create_table_dir(&dir, "complete", r"{}", r"[]", r"{}").await;

        let entry1 = FullDirEntry {
            dir_name: "partial".to_string(),
            info: make_info(&url_partial),
            // `compute_and_write_state` does not read `data_raw`;
            // the actual skip is driven by missing files on disk.
            data_raw: None,
        };

        let mut active = HashSet::new();
        active.insert(Url::parse(&url_partial).unwrap());
        active.insert(Url::parse(&url_complete).unwrap());

        compute_and_write_state(&dir, &active, &[entry1, entry2])
            .await
            .unwrap();

        let state = read_state(&dir).await;
        assert_eq!(state.tables.len(), 1);
        assert!(state.tables.contains_key(&url_complete));

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
