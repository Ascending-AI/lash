use std::fs;
use std::path::{Path, PathBuf};

#[test]
// Architecture lint: lexical vocabulary guard, not behavior proof. Dependency
// direction is checked behaviorally through Cargo metadata below.
fn lint_crate_sources_do_not_name_integration_protocols() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();

    for root in [
        crate_dir.join("Cargo.toml"),
        crate_dir.join("src"),
        crate_dir.join("tests"),
    ] {
        scan_path(&root, &mut failures);
    }

    assert!(
        failures.is_empty(),
        "core crate must stay integration-agnostic:\n{}",
        failures.join("\n")
    );
}

#[test]
fn cargo_metadata_keeps_protocol_crates_out_of_lash_core_dependencies() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(&workspace)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata JSON");
    let packages = metadata["packages"]
        .as_array()
        .expect("metadata packages array");
    let core = packages
        .iter()
        .find(|package| package["name"].as_str() == Some("lash-core"))
        .expect("lash-core package in workspace metadata");
    let dependency_names = core["dependencies"]
        .as_array()
        .expect("lash-core dependency array")
        .iter()
        .filter_map(|dependency| dependency["name"].as_str())
        .collect::<Vec<_>>();

    for forbidden in [
        concat!("lash-protocol-", "r", "lm"),
        concat!("lash-", "lash", "lang-runtime"),
        concat!("lash", "lang"),
        "lash-protocol-standard",
    ] {
        assert!(
            !dependency_names.contains(&forbidden),
            "dependency direction violation: lash-core depends on integration crate {forbidden}"
        );
    }
}

fn scan_path(path: &Path, failures: &mut Vec<String>) {
    if path.is_dir() {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
            .map(|entry| {
                entry
                    .unwrap_or_else(|err| {
                        panic!("failed to read entry under {}: {err}", path.display())
                    })
                    .path()
            })
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            scan_path(&entry, failures);
        }
        return;
    }

    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let lower = text.to_ascii_lowercase();
    for needle in [concat!("lash", "lang"), concat!("r", "lm")] {
        if !lower.contains(needle) {
            continue;
        }
        for (index, line) in text.lines().enumerate() {
            if line.to_ascii_lowercase().contains(needle) {
                failures.push(format!(
                    "{}:{} contains `{needle}`",
                    path.display(),
                    index + 1
                ));
            }
        }
    }
}
