//! Host Git provenance, exercised against real index and worktree state.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

fn git(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    anyhow::ensure!(output.status.success(), "{args:?}: {output:?}");
    Ok(())
}

fn dirty(root: &Path) -> std::io::Result<bool> {
    let previous = std::env::current_dir()?;
    std::env::set_current_dir(root)?;
    let dirty = lash_perf::perf_support::git::git_dirty();
    std::env::set_current_dir(previous)?;
    Ok(dirty)
}

/// GIT-PROVENANCE-REFRESH: a clean capture refreshes a racily clean index
/// for later processes, without hiding staged or unstaged tracked changes.
#[test]
fn provenance_refreshes_racy_index_without_hiding_tracked_changes() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    git(root, &["init", "-q"])?;
    let tracked = root.join("tracked");
    std::fs::write(&tracked, "first\n")?;
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    std::fs::File::open(&tracked)?.set_modified(old)?;
    git(root, &["add", "tracked"])?;
    let commit = [
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ];
    git(root, &commit)?;
    let index = root.join(".git/index");
    // Git must re-hash an entry whose cached mtime equals the index's
    // timestamp. `diff --quiet` leaves that timestamp unchanged forever.
    std::fs::File::open(&index)?.set_modified(old)?;
    assert!(!dirty(root)?);
    assert!(
        std::fs::metadata(&index)?.modified()? > old,
        "a clean provenance capture must persist the index refresh"
    );
    assert!(!dirty(root)?);
    std::fs::write(&tracked, "second\n")?;
    assert!(dirty(root)?, "unstaged changes must remain dirty");
    git(root, &["add", "tracked"])?;
    assert!(dirty(root)?, "staged changes must remain dirty");
    git(root, &commit)?;
    std::fs::write(root.join("untracked"), "local evidence\n")?;
    assert!(
        !dirty(root)?,
        "untracked evidence is outside source provenance"
    );
    Ok(())
}
