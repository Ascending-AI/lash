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
    std::process::Command::new("git")
        .args(["diff", "--quiet", "--ignore-submodules", "--"])
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
}
