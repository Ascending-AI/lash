//! Refuse a data directory an earlier build laid out.
//!
//! The example hosts used to open each durable store at its own path in the
//! data directory: the process registry, the triggers, the process
//! environments, the artifacts and the attachments. They now run on one
//! SQLite backend, which keeps all of them under its own root. Nothing reads
//! the old files, so booting over them would silently start from empty stores
//! beside the old ones. The host refuses instead, before it opens anything,
//! and names the file.

use std::fmt;
use std::path::{Path, PathBuf};

/// A data directory that still holds a store at a path this build no longer
/// opens.
#[derive(Debug)]
pub(crate) struct PriorStoreLayout {
    pub(crate) path: PathBuf,
}

impl fmt::Display for PriorStoreLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is a store from an earlier data layout, which this build does not read; \
             start from an empty data directory",
            self.path.display()
        )
    }
}

impl std::error::Error for PriorStoreLayout {}

/// Refuse `data_dir` when any of `entries` (file or directory names directly
/// under it) exists.
pub(crate) fn refuse_prior_store_layout(
    data_dir: &Path,
    entries: &[&str],
) -> Result<(), PriorStoreLayout> {
    for entry in entries {
        let path = data_dir.join(entry);
        if path.exists() {
            return Err(PriorStoreLayout { path });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_at_a_prior_path_is_refused_and_named() {
        let dir = tempfile::tempdir().expect("data directory");
        refuse_prior_store_layout(dir.path(), &["processes.db"])
            .expect("an empty data directory is not a prior layout");
        std::fs::write(dir.path().join("processes.db"), b"").expect("write a prior store");

        let refusal = refuse_prior_store_layout(dir.path(), &["triggers.db", "processes.db"])
            .expect_err("a prior store must be refused");
        assert_eq!(refusal.path, dir.path().join("processes.db"));
    }
}
