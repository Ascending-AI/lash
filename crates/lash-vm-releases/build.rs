//! Defines the standard embedding's TypeScript helpers once, when the crate
//! is built (FIG-5796), defines the helper release the tree builds from
//! them until the cut seals it (FIG-5839), and checks the sealed releases
//! the build holds against what it builds (FIG-5799).
//!
//! Every worker is a process of its own, and defining the helpers (reading
//! their kernel text, validating and identifying each) took most of a
//! worker's startup. The script defines them against the library lash
//! ships, validates them in a registry holding it, and writes the result to
//! `OUT_DIR`. A worker registers them without defining them again.
//!
//! A run pins the identities it was written against, so a build also holds
//! every function of the helper releases it retains that it does not
//! define itself. The script resolves the declared releases
//! (`src/declared.rs`): each sealed one as the repository keeps it
//! (`src/sealed.rs`), and the release the tree builds, while it is unsealed,
//! as the build defines it, with the digest of what each native answers
//! (`src/build/probe.rs`). It checks every release against the build
//! (`src/build/release.rs`), validates the functions it retains in the
//! registry the worker holds, and writes them, the releases, and their
//! index. A parent and a worker read all of it from this one build.

#[path = "src/declared.rs"]
mod declared;
#[path = "src/library.rs"]
mod library;
#[path = "src/build/probe.rs"]
mod probe;
#[path = "src/build/release.rs"]
mod release;
#[expect(
    dead_code,
    reason = "the build writes releases; sealing and decoding the held set are the library's"
)]
#[path = "src/releases.rs"]
mod releases;
#[path = "src/sealed.rs"]
mod sealed;

#[expect(
    dead_code,
    reason = "the build stages manifests; restore admission belongs to the runtime"
)]
#[path = "src/manifest.rs"]
mod manifest;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::FunctionRegistry;

use crate::releases::{HelperRelease, HelperReleaseIndex};

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

    let fingerprints = probe::fingerprints(&registry);
    let held = release::resolve(
        declared::RETAINED_HELPER_RELEASES,
        declared::RETIRING_HELPER_RELEASES,
        sealed::SEALED,
        |name, ordinal, previous| {
            release::freeze(name, ordinal, previous, &registry, &fingerprints)
        },
    )?;
    let retained = release::keep(&held.retained, &registry, &fingerprints)?;
    let retained = registry.validate_functions(retained)?;
    std::fs::write(out.join("retained_functions.json"), retained.to_json()?)?;
    let index: Vec<HelperReleaseIndex> = held.retained.iter().map(HelperRelease::index).collect();
    std::fs::write(
        out.join("helper_releases.json"),
        serde_json::to_vec(&index)?,
    )?;
    std::fs::write(out.join("held_releases.json"), serde_json::to_vec(&held)?)?;
    write_checkpoint_catalog(&out, &held)?;
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    Ok(())
}

/// Encode the immutable build catalog once, rather than in every worker.
#[expect(
    clippy::disallowed_methods,
    reason = "a build script writes its OUT_DIR artifacts"
)]
fn write_checkpoint_catalog(
    out: &std::path::Path,
    held: &releases::HeldReleases,
) -> Result<(), Box<dyn std::error::Error>> {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TypedBody,
    };
    use lash_kernel_doc::graph_tokens::{TokenCodec as _, TokenWriter};

    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut manifests = TokenWriter::new();
    manifests.seq(held.retained.len() as u64);
    for release in &held.retained {
        let (manifest, reference) = manifest::stage_release(release, &mut |kind, logical| {
            staging
                .encode(NodeKind(kind), TypedBody::logical(logical))
                .map(|id| id.child_ref())
        })?;
        manifests.seq(2);
        manifests.child(reference);
        manifest.encode(&mut manifests);
    }
    std::fs::write(out.join("checkpoint_manifests.bin"), manifests.into_bytes())?;
    let mut nodes = TokenWriter::new();
    nodes.seq(staging.nodes().len() as u64);
    for (id, node) in staging.nodes() {
        nodes.seq(2);
        nodes.child(id.child_ref());
        nodes.bytes(&node.bytes);
    }
    std::fs::write(out.join("checkpoint_nodes.bin"), nodes.into_bytes())?;
    Ok(())
}
