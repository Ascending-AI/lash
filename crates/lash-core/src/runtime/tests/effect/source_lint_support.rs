use std::path::{Path, PathBuf};

pub(super) fn effect_module_sources(manifest_dir: &Path) -> Vec<PathBuf> {
    rust_sources_in(manifest_dir.join("src/runtime/effect"))
}

/// The turn loop's phase modules, so the cutover lint keeps inspecting the
/// implementation after FIG-1028 moved it out of the single `turn_loop.rs`.
pub(super) fn turn_loop_module_sources(manifest_dir: &Path) -> Vec<PathBuf> {
    rust_sources_in(manifest_dir.join("src/runtime/turn_loop"))
}

fn rust_sources_in(dir: PathBuf) -> Vec<PathBuf> {
    let mut pending = vec![dir];
    let mut paths = Vec::new();

    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read module directory") {
            let entry = entry.expect("read module directory entry");
            let file_type = entry.file_type().expect("read module entry type");
            let path = entry.path();

            assert!(
                !file_type.is_symlink(),
                "module source traversal does not follow symlink {}",
                path.display()
            );
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            {
                paths.push(path);
            }
        }
    }

    paths.sort();
    paths
}

pub(super) fn unique_trace_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lash-{prefix}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}
