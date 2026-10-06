//! Crash-atomic file replacement for the persisted files two workbench
//! generations share during a handover.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `bytes` atomically: stage into a per-write unique
/// sibling, then `rename` into place. The draining host and its successor
/// share one data dir across a handover, so the staging name must be unique
/// to the writer — a fixed `<name>.tmp` lets the peer's rename move the file
/// out from under this write and fail it with ENOENT.
pub(crate) fn replace_file(path: &Path, bytes: &[u8], what: &str) {
    let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut staging_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    staging_name.push(format!(".staging.{}.{counter}.tmp", std::process::id()));
    let temporary = path
        .parent()
        .map(|parent| parent.join(&staging_name))
        .unwrap_or_else(|| PathBuf::from(&staging_name));
    std::fs::write(&temporary, bytes)
        .unwrap_or_else(|err| panic!("write {what} `{}`: {err}", temporary.display()));
    std::fs::rename(&temporary, path).unwrap_or_else(|err| {
        panic!(
            "replace {what} `{}` from `{}`: {err}",
            path.display(),
            temporary.display()
        )
    });
}
