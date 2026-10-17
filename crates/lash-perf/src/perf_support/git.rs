/// The checked-out commit, or `None` outside a working tree (CI passes the sha
/// in the environment instead, so this is the local-run fallback).
pub fn head_commit() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!commit.is_empty()).then_some(commit)
}

pub fn git_dirty() -> bool {
    // Status persists the refresh of a racily clean index. Diff's quiet
    // shortcut leaves it racy, re-hashing unchanged sources in every process.
    std::process::Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "--untracked-files=no",
            "--ignore-submodules",
        ])
        .output()
        .map(|output| !output.status.success() || !output.stdout.is_empty())
        .unwrap_or(true)
}

/// Digest an actual executable outside measurement windows. Unreadable artifacts
/// remain unknown and cannot qualify a comparison.
pub fn artifact_sha256(path: &std::path::Path) -> Option<String> {
    use sha2::Digest as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hash = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hash).ok()?;
    Some(format!("{:x}", hash.finalize()))
}
