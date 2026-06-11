//! Orphan directory handling: compute and move directories not in the active set.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use log::{info, warn};
use tokio::fs;
use url::Url;

use crate::rename::DirInfo;

/// A pending orphan-move to `_orphaned/`.
pub struct OrphanAction {
    /// Current path, relative to table base dir.
    pub src_path: PathBuf,
}

/// Compute which directories should be moved to `_orphaned/`.
///
/// A directory is orphaned if its URL is not in `active_urls`.
#[must_use]
#[expect(
    clippy::implicit_hasher,
    reason = "HashSet<Url> is the canonical type for active URLs"
)]
pub fn compute_orphans(entries: &[impl DirInfo], active_urls: &HashSet<Url>) -> Vec<OrphanAction> {
    entries
        .iter()
        .filter(|e| !active_urls.contains(&e.info().url))
        .map(|e| OrphanAction {
            src_path: PathBuf::from(e.dir_name()),
        })
        .collect()
}

/// Execute orphan moves, returning the count of successfully moved directories.
pub async fn execute_orphans(actions: &[OrphanAction], base_dir: &Path) -> usize {
    if actions.is_empty() {
        return 0;
    }

    let orphan_dir = base_dir.join("_orphaned");
    if let Err(e) = fs::create_dir_all(&orphan_dir).await {
        warn!("Failed to create _orphaned/ directory: {e}");
        return 0;
    }

    let mut moved = 0;
    for action in actions {
        let src = base_dir.join(&action.src_path);
        let dst = orphan_dir.join(&action.src_path);

        if !fs::try_exists(&src).await.unwrap_or(false) {
            continue;
        }

        // Remove stale orphan
        if fs::try_exists(&dst).await.unwrap_or(false) {
            fs::remove_dir_all(&dst).await.ok();
        }

        match fs::rename(&src, &dst).await {
            Ok(()) => {
                info!(
                    "Moved orphan: {} -> _orphaned/{}",
                    action.src_path.display(),
                    action.src_path.display()
                );
                moved += 1;
            }
            Err(e) => warn!("Failed to move orphan {}: {e}", action.src_path.display()),
        }
    }

    moved
}
