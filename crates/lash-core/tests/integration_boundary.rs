use std::fs;
use std::path::{Path, PathBuf};

#[test]
// Architecture lint: lexical vocabulary guard, not behavior proof. Dependency
// direction is checked behaviorally through the workspace inventory below.
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
fn workspace_inventory_keeps_protocol_crates_out_of_lash_core_dependencies() {
    // `tools/bazel/target-inventory.json` is generated from Cargo's locked
    // workspace metadata by tools/bazel/generate_build_files.py and kept in
    // sync by its `--check` mode (a CI gate). Reading the checked-in fact keeps
    // this dependency-direction proof identical while letting the test run in a
    // hermetic action that has no Cargo.
    let inventory_path = workspace_root().join("tools/bazel/target-inventory.json");
    let inventory: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&inventory_path)
            .unwrap_or_else(|err| panic!("read {}: {err}", inventory_path.display())),
    )
    .expect("parse workspace target inventory JSON");
    let packages = inventory["packages"]
        .as_array()
        .expect("inventory packages array");
    let core = packages
        .iter()
        .find(|package| package["package"].as_str() == Some(env!("CARGO_PKG_NAME")))
        .expect("current package in workspace inventory");
    let dependency_names = core["dependencies"]
        .as_array()
        .expect("current package dependency array")
        .iter()
        .filter_map(|dependency| dependency.as_str())
        .collect::<Vec<_>>();

    let forbidden_library_targets = [
        concat!("lash_protocol_", "r", "lm"),
        concat!("lash_", "lash", "lang_runtime"),
        concat!("lash", "lang"),
        "lash_protocol_standard",
    ];
    let forbidden_package_names = packages
        .iter()
        .filter(|package| {
            package["targets"].as_array().is_some_and(|targets| {
                targets.iter().any(|target| {
                    target["kind"].as_str() == Some("lib")
                        && target["cargo"]
                            .as_str()
                            .is_some_and(|name| forbidden_library_targets.contains(&name))
                })
            })
        })
        .filter_map(|package| package["package"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        forbidden_package_names.len(),
        forbidden_library_targets.len(),
        "every forbidden integration library target must resolve to a workspace package"
    );

    for forbidden in forbidden_package_names {
        assert!(
            !dependency_names.contains(&forbidden),
            "dependency direction violation: core depends on integration package {forbidden}"
        );
    }
}

/// The workspace root, both under Cargo (an absolute path above the crate) and
/// under Bazel, where `CARGO_MANIFEST_DIR` is the runfiles-relative package
/// directory and the root is the working directory.
fn workspace_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root
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
