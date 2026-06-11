//! Index building helpers: parse chart data and build inverted indexes.

use std::collections::{BTreeMap, BTreeSet};

use log::warn;
use serde_json::Value;

/// Parse chart items from a `data.json` string, supporting two formats:
/// - Plain array: `[...]`
/// - Object with array field: `{"charts": [...]}`
#[must_use]
pub fn extract_chart_items(content: &str) -> Option<Vec<Value>> {
    // Fast path: plain array
    if let Ok(v) = serde_json::from_str::<Vec<Value>>(content) {
        return Some(v);
    }

    // Fallback: object with a container key
    let root: Value = serde_json::from_str(content).ok()?;
    let obj = root.as_object()?;

    for key in &["charts", "data", "songs"] {
        if let Some(arr) = obj.get(*key).and_then(|v| v.as_array()) {
            return Some(arr.clone());
        }
    }

    None
}

/// Insert a value into an index map under `item[key]` if the key exists and is non-empty.
pub fn maybe_insert(
    map: &mut BTreeMap<String, BTreeSet<String>>,
    item: &Value,
    key: &str,
    value: &str,
) {
    if let Some(s) = item
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        map.entry(s.to_string())
            .or_default()
            .insert(value.to_string());
    }
}

/// Insert a validated hash value into the index map.
///
/// Only inserts if the hash is exactly `expected_len` hex characters.
/// Logs a warning for suspiciously long hashes.
pub fn maybe_insert_hash(
    map: &mut BTreeMap<String, BTreeSet<String>>,
    item: &Value,
    key: &str,
    expected_len: usize,
    table_name: &str,
) {
    let raw = match item.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };

    let is_all_hex = raw.chars().all(|c| c.is_ascii_hexdigit());

    if raw.len() == expected_len && is_all_hex {
        map.entry(raw.to_string())
            .or_default()
            .insert(table_name.to_string());
    } else if raw.len() > expected_len && is_all_hex {
        let truncated = if raw.len() > 64 {
            format!("{}… ({} chars total)", &raw[..64], raw.len())
        } else {
            raw.to_string()
        };
        warn!(
            "Suspiciously long {key} hash (expected {expected_len}) in table {table_name}: {truncated}",
        );
    }
}
