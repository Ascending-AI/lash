//! Run provenance stamped onto every result row, so a JSONL record can be
//! attributed to the lash revision and binary that produced it (FIG-3527).

use serde::Serialize;
use serde_json::Value;
use std::process::Command;
use std::sync::OnceLock;

/// What produced a run: the lash source revision, whether the checkout was
/// dirty, and the digest of the binary that ran.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Provenance {
    /// `git rev-parse HEAD` of the lash checkout, or "unknown" when no work
    /// tree is reachable (for example inside a Bazel sandbox).
    pub lash_revision: String,
    /// Whether the checkout carried uncommitted or untracked changes;
    /// `null` when the tree state cannot be determined. A dirty or unknown
    /// tree means the row is not attributable to a committed revision.
    pub lash_dirty: Option<bool>,
    /// SHA-256 of the running binary image, or "unknown".
    pub binary_sha256: String,
}

/// The process-wide provenance, collected once on first use.
pub(crate) fn current() -> &'static Provenance {
    static PROVENANCE: OnceLock<Provenance> = OnceLock::new();
    PROVENANCE.get_or_init(collect)
}

fn collect() -> Provenance {
    let (lash_revision, lash_dirty) = match git_state() {
        Some((revision, dirty)) => (revision, Some(dirty)),
        None => ("unknown".to_string(), None),
    };
    Provenance {
        lash_revision,
        lash_dirty,
        binary_sha256: binary_digest().unwrap_or_else(|| "unknown".to_string()),
    }
}

/// Stamps the provenance object onto a JSONL row about to be written.
pub(crate) fn stamp(row: &mut Value) -> serde_json::Result<()> {
    if let Some(object) = row.as_object_mut() {
        object.insert("provenance".to_string(), serde_json::to_value(current())?);
    }
    Ok(())
}

fn git_state() -> Option<(String, bool)> {
    let dir = env!("CARGO_MANIFEST_DIR");
    let revision = git(dir, &["rev-parse", "HEAD"])?;
    let dirty = !git(dir, &["status", "--porcelain"])?.is_empty();
    Some((revision, dirty))
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn binary_digest() -> Option<String> {
    use sha2::Digest as _;
    let bytes = std::fs::read(std::env::current_exe().ok()?).ok()?;
    Some(format!("{:x}", sha2::Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbench_row_carries_provenance() {
        let mut row = serde_json::json!({"kind": "task_result", "task": "kv-read"});
        stamp(&mut row).unwrap();
        let provenance = &row["provenance"];
        assert!(
            provenance["lash_revision"]
                .as_str()
                .is_some_and(|revision| !revision.is_empty())
        );
        assert!(provenance["lash_dirty"].is_boolean() || provenance["lash_dirty"].is_null());
        assert!(
            provenance["binary_sha256"]
                .as_str()
                .is_some_and(|digest| !digest.is_empty())
        );
    }

    #[test]
    fn collect_never_returns_empty_fields() {
        let provenance = collect();
        assert!(!provenance.lash_revision.is_empty());
        assert!(!provenance.binary_sha256.is_empty());
    }
}
