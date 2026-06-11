//! Directory scanning: discover table directories and read their metadata.

use std::path::Path;

use anyhow::Result;
use bms_table::BmsTableInfo;
use tokio::fs;

use crate::rename::DirInfo;

/// A table directory discovered during a lightweight scan (info.json only).
#[derive(Clone)]
pub struct DirEntry {
    /// Actual directory name on disk.
    pub dir_name: String,
    /// Parsed `info.json` content.
    pub info: BmsTableInfo,
}

impl DirInfo for DirEntry {
    fn dir_name(&self) -> &str {
        &self.dir_name
    }

    fn info(&self) -> &BmsTableInfo {
        &self.info
    }
}

/// A table directory discovered during a full scan (info.json + data.json).
#[derive(Clone)]
pub struct FullDirEntry {
    /// Actual directory name on disk.
    pub dir_name: String,
    /// Parsed `info.json` content.
    pub info: BmsTableInfo,
    /// Raw `data.json` content, if it exists.
    pub data_raw: Option<String>,
}

impl DirInfo for FullDirEntry {
    fn dir_name(&self) -> &str {
        &self.dir_name
    }

    fn info(&self) -> &BmsTableInfo {
        &self.info
    }
}

/// Scan table directories, reading only `info.json`.
///
/// Skips `_orphaned/` and directories without valid `info.json`.
/// Returns an empty vector if the directory does not exist.
///
/// # Errors
///
/// Returns unexpected I/O errors from reading directory entries.
pub async fn scan_dirs(table_dir: &Path) -> Result<Vec<DirEntry>> {
    let mut entries = Vec::new();

    let Ok(mut dir_entries) = fs::read_dir(table_dir).await else {
        return Ok(entries);
    };

    while let Some(entry) = dir_entries.next_entry().await? {
        if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        if dir_name == "_orphaned" {
            continue;
        }

        let Ok(info_content) = fs::read_to_string(path.join("info.json")).await else {
            continue;
        };
        let Ok(info) = serde_json::from_str::<BmsTableInfo>(&info_content) else {
            continue;
        };

        entries.push(DirEntry { dir_name, info });
    }

    Ok(entries)
}

/// Scan table directories, reading both `info.json` and `data.json`.
///
/// Skips `_orphaned/` and directories without valid `info.json`.
/// Returns an empty vector if the directory does not exist.
///
/// # Errors
///
/// Returns unexpected I/O errors from reading directory entries.
pub async fn scan_dirs_full(table_dir: &Path) -> Result<Vec<FullDirEntry>> {
    let mut entries = Vec::new();

    let Ok(mut dir_entries) = fs::read_dir(table_dir).await else {
        return Ok(entries);
    };

    while let Some(entry) = dir_entries.next_entry().await? {
        if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        if dir_name == "_orphaned" {
            continue;
        }

        let Ok(info_content) = fs::read_to_string(path.join("info.json")).await else {
            continue;
        };
        let Ok(info) = serde_json::from_str::<BmsTableInfo>(&info_content) else {
            continue;
        };

        // data.json is optional — read if present
        let data_raw = fs::read_to_string(path.join("data.json")).await.ok();

        entries.push(FullDirEntry {
            dir_name,
            info,
            data_raw,
        });
    }

    Ok(entries)
}
