//! Derives the worker identity during both Cargo and Bazel builds.
#![allow(
    clippy::disallowed_methods,
    reason = "build scripts read declared build inputs"
)]

#[path = "build/fingerprint.rs"]
mod fingerprint;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is unset")?,
    );
    let root = std::env::var_os("LASH_VM_WORKER_SOURCE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| manifest.join("../.."))
        .canonicalize()?;
    println!("cargo:rerun-if-env-changed=LASH_VM_WORKER_SOURCE_ROOT");
    let (paths, directories) = fingerprint::inputs(&root)?;
    for path in paths.iter().chain(&directories) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=LASH_VM_WORKER_BUILD_FINGERPRINT={}",
        fingerprint::fingerprint(&root, &paths)?
    );
    Ok(())
}
