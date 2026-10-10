//! Defines the standard embedding's TypeScript helpers once, when the crate
//! is built (FIG-5796), and checks the released helper sets it retains
//! against what it builds (FIG-5799).
//!
//! Every worker is a process of its own, and defining the helpers (reading
//! their kernel text, validating and identifying each) took most of a
//! worker's startup. The script defines them against the library lash
//! ships, validates them in a registry holding it, and writes the result to
//! `OUT_DIR`. A worker registers them without defining them again.
//!
//! A run pins the identities it was written against, so a build also holds
//! every function of the helper releases it retains that it does not
//! define itself (`lash-vm-library`, which a parent holds the library
//! from). The script checks each release against the build
//! (`src/build/release.rs`, `src/build/probe.rs`), validates the functions
//! it retains in the registry the worker holds, and writes them, the
//! releases' index, where the release the tree is still building differs
//! from the build, and that release frozen for the tree as it stands.

#[path = "src/library.rs"]
mod library;
#[path = "src/build/probe.rs"]
mod probe;
#[path = "src/build/release.rs"]
mod release;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::FunctionRegistry;
use lash_vm_library::{HelperRelease, HelperReleaseIndex};

#[expect(
    clippy::disallowed_methods,
    reason = "a build script reads OUT_DIR and writes its outputs there"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = FunctionRegistry::new();
    library::register_kernel(&mut registry)?;
    library::register_extensions(&mut registry)?;
    let mut named = NamedLibrary::from_registry(&registry)?;
    let helpers = lash_dialect_typescript::define_helpers(&mut named)?;
    let validated = registry.validate_functions(helpers)?;
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    std::fs::write(out.join("typescript_helpers.json"), validated.to_json()?)?;

    let releases = lash_vm_library::helper_releases()?;
    let fingerprints = probe::fingerprints(&registry);
    let building = releases.last().ok_or("no helper release")?;
    let frozen = release::freeze(
        &building.release,
        building.ordinal,
        Some(building),
        &registry,
        &fingerprints,
    );
    std::fs::write(out.join("helper_release.rs"), frozen.source()?)?;
    let kept = release::keep(&releases, &registry, &fingerprints)?;
    let retained = registry.validate_functions(kept.retained)?;
    std::fs::write(out.join("retained_functions.json"), retained.to_json()?)?;
    let index: Vec<HelperReleaseIndex> = releases.iter().map(HelperRelease::index).collect();
    std::fs::write(
        out.join("helper_releases.json"),
        serde_json::to_vec(&index)?,
    )?;
    std::fs::write(
        out.join("helper_release_divergence.json"),
        serde_json::to_vec(&kept.divergence)?,
    )?;
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    Ok(())
}
