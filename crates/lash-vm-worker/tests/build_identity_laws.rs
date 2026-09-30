#![expect(
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "identity laws construct isolated source trees and inspect declared build inputs"
)]

#[path = "../build/fingerprint.rs"]
mod fingerprint;

use std::fs;
use std::path::Path;

fn fixture() -> tempfile::TempDir {
    let tree = tempfile::tempdir().expect("tree");
    for (path, content) in [
        (
            "Cargo.toml",
            "[workspace.dependencies]\nlocal = { path = 'crates/local' }\n",
        ),
        ("Cargo.lock", "locked dependencies"),
        ("rust-toolchain.toml", "compiler pin"),
        (
            "crates/lash-vm-worker/Cargo.toml",
            "[dependencies]\nlocal = { workspace = true }\n[target.'cfg(unix)'.dependencies]\nleaf = { path = '../leaf' }\n[dev-dependencies]\nunrelated = { path = '../unrelated' }\n",
        ),
        ("crates/lash-vm-worker/src/lib.rs", "worker source"),
        ("crates/lash-vm-worker/build.rs", "build script"),
        ("crates/lash-vm-worker/build/fingerprint.rs", "build module"),
        (
            "crates/local/Cargo.toml",
            "[dependencies]\nleaf = { path = '../leaf' }\n",
        ),
        ("crates/local/src/lib.rs", "local source"),
        (
            "crates/leaf/Cargo.toml",
            "[dependencies]\nlocal = { path = '../local' }\n",
        ),
        ("crates/leaf/src/lib.rs", "leaf source"),
        ("crates/unrelated/Cargo.toml", "[dependencies]\n"),
        ("crates/unrelated/src/lib.rs", "unrelated source"),
    ] {
        write(tree.path(), path, content);
    }
    tree
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    fs::write(path, content).expect("source");
}

fn identity(root: &Path) -> String {
    let (paths, _) = fingerprint::inputs(root).expect("closure");
    fingerprint::fingerprint(root, &paths).expect("fingerprint")
}

#[test]
fn identical_sources_at_different_paths_have_identical_identities() {
    let first = fixture();
    let second = fixture();
    assert_eq!(identity(first.path()), identity(second.path()));
}

#[test]
fn each_closure_input_changes_the_identity() {
    let tree = fixture();
    let original = identity(tree.path());
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "crates/lash-vm-worker/Cargo.toml",
        "crates/lash-vm-worker/src/lib.rs",
        "crates/lash-vm-worker/build.rs",
        "crates/lash-vm-worker/build/fingerprint.rs",
        "crates/local/Cargo.toml",
        "crates/local/src/lib.rs",
        "crates/leaf/Cargo.toml",
        "crates/leaf/src/lib.rs",
    ] {
        let path = tree.path().join(relative);
        let content = fs::read(&path).expect("source");
        let mut changed = content.clone();
        changed.extend_from_slice(b"\n# changed\n");
        fs::write(&path, changed).expect("change");
        assert_ne!(identity(tree.path()), original, "{}", path.display());
        fs::write(path, content).expect("restore");
    }
}

#[test]
fn adding_and_removing_transitive_sources_changes_the_identity() {
    let tree = fixture();
    let original = identity(tree.path());
    write(tree.path(), "crates/leaf/src/nested/new.rs", "new source");
    assert_ne!(identity(tree.path()), original);
    fs::remove_file(tree.path().join("crates/leaf/src/nested/new.rs")).expect("remove");
    assert_eq!(identity(tree.path()), original);
}

#[test]
fn unrelated_sources_and_tests_do_not_change_the_identity() {
    let tree = fixture();
    let original = identity(tree.path());
    write(tree.path(), "crates/unrelated/src/lib.rs", "unrelated edit");
    write(tree.path(), "crates/leaf/tests/law.rs", "test edit");
    write(tree.path(), "crates/leaf/README.md", "documentation edit");
    assert_eq!(identity(tree.path()), original);
}

#[test]
fn compiled_identity_matches_current_sources_without_inventory_regeneration() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .expect("crates")
        .parent()
        .expect("workspace");
    let expected = format!(
        "lash-worker/{}/{}/{}/debug-{}/testing-{}",
        identity(root),
        std::env::consts::ARCH,
        std::env::consts::OS,
        cfg!(debug_assertions),
        cfg!(feature = "testing")
    );
    println!("compiled identity: {expected}");
    assert_eq!(
        lash_vm_worker::build_identity(),
        lash_vm_protocol::BuildIdentity::new(expected)
    );
}
