//! Defines each built dialect's helpers once, when the crate is built
//! (FIG-5796), defines the helper release the tree builds from them until
//! the cut seals it (FIG-5839), and checks the sealed releases the build
//! holds against what it builds (FIG-5799), dialect by dialect (FIG-5990).
//!
//! Every worker is a process of its own, and defining the helpers (reading
//! their kernel text, validating and identifying each) took most of a
//! worker's startup. The script defines a dialect's helpers against the
//! library that dialect ships with, validates them in a registry holding
//! it, and writes the result to `OUT_DIR`. A worker registers them without
//! defining them again.
//!
//! A run pins the identities it was written against, so a build also holds
//! every function of the helper releases it retains that it does not
//! define itself. For each dialect the build's features name, the script
//! resolves the declared releases (`src/declared.rs`): each sealed one as
//! the repository keeps it for that dialect (`src/sealed.rs`), and the
//! release the tree builds, while it is unsealed, as the build defines it,
//! with the digest of what each native answers (`src/build/probe.rs`). It
//! checks every release against the build (`src/build/release.rs`),
//! validates the functions it retains in the registry the worker holds, and
//! writes them, the releases, and their index. A parent and a worker read
//! all of it from this one build. A build with no dialect feature holds no
//! release.

#![cfg_attr(
    not(any(feature = "dialect-typescript", feature = "dialect-python")),
    expect(
        dead_code,
        unused_imports,
        reason = "a build with no dialect defines and checks no release"
    )
)]

#[path = "src/build/corpus.rs"]
mod corpus;
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
use lash_kernel_doc::{FunctionDefinition, FunctionRegistry};

use crate::releases::{HelperRelease, HelperReleaseIndex};

type BuildError = Box<dyn std::error::Error>;

#[expect(
    clippy::disallowed_methods,
    reason = "a build script reads OUT_DIR and writes its outputs there"
)]
fn main() -> Result<(), BuildError> {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    for (dialect, _) in sealed::SEALED {
        if !declared::SHIPPED_DIALECTS.contains(dialect) {
            return Err(
                format!("sealed releases are kept for `{dialect}`, which is no dialect").into(),
            );
        }
    }
    #[cfg(feature = "dialect-typescript")]
    {
        let mut registry = FunctionRegistry::new();
        library::register_kernel(&mut registry)?;
        library::register_extensions(&mut registry)?;
        dialect(&out, declared::TYPESCRIPT, registry, |library| {
            Ok(lash_dialect_typescript::define_helpers(library)?)
        })?;
    }
    #[cfg(feature = "dialect-python")]
    {
        let mut registry = FunctionRegistry::new();
        library::register_kernel(&mut registry)?;
        dialect(&out, declared::PYTHON, registry, |library| {
            Ok(lash_dialect_python::define_helpers(library)?)
        })?;
    }
    #[cfg(not(any(feature = "dialect-typescript", feature = "dialect-python")))]
    let _ = (&out, declared::TYPESCRIPT, declared::PYTHON);
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    Ok(())
}

/// Defines `dialect`'s helpers against `registry`, the library it ships
/// with, resolves and checks its releases, and writes all of it under the
/// dialect's name.
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
#[expect(
    clippy::disallowed_methods,
    reason = "a build script writes its OUT_DIR artifacts"
)]
fn dialect(
    out: &std::path::Path,
    dialect: &str,
    mut registry: FunctionRegistry,
    define: impl FnOnce(&mut NamedLibrary) -> Result<Vec<FunctionDefinition>, BuildError>,
) -> Result<(), BuildError> {
    let write =
        |file: &str, bytes: &[u8]| std::fs::write(out.join(format!("{dialect}_{file}")), bytes);
    let mut named = NamedLibrary::from_registry(&registry)?;
    let helpers = define(&mut named)?;
    let validated = registry.validate_functions(helpers)?;
    write("helpers.json", &validated.to_json()?)?;

    let corpus = corpus::Corpus::recorded()?;
    let building = declared::RETAINED_HELPER_RELEASES
        .last()
        .ok_or("no retained release")?
        .0;
    for (release, _) in declared::RETAINED_HELPER_RELEASES {
        if *release != building {
            corpus.verify_release(&registry, dialect, release)?;
        }
    }
    let fingerprints = corpus.certify(&registry, dialect, building)?;
    let sealed = sealed::SEALED
        .iter()
        .find(|(kept, _)| *kept == dialect)
        .map_or(&[][..], |(_, releases)| releases);
    let held = release::resolve(
        declared::RETAINED_HELPER_RELEASES,
        declared::RETIRING_HELPER_RELEASES,
        sealed,
        |name, ordinal, previous| {
            release::freeze(name, ordinal, previous, &registry, &fingerprints)
        },
    )
    .and_then(|held| {
        let retained = release::keep(&held.retained, &registry, &fingerprints)?;
        Ok((held, retained))
    })
    .map_err(|error| format!("dialect `{dialect}`: {error}"))?;
    let (held, retained) = held;
    let retained = registry.validate_functions(retained)?;
    write("retained_functions.json", &retained.to_json()?)?;
    let index: Vec<HelperReleaseIndex> = held.retained.iter().map(HelperRelease::index).collect();
    write("helper_releases.json", &serde_json::to_vec(&index)?)?;
    write("held_releases.json", &serde_json::to_vec(&held)?)?;
    let (manifests, nodes) = checkpoint_catalog(&held)?;
    write("checkpoint_manifests.bin", &manifests)?;
    write("checkpoint_nodes.bin", &nodes)?;
    Ok(())
}

/// Encodes a dialect's immutable build catalog once, rather than in every
/// worker: its release manifests with their references, and every node.
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn checkpoint_catalog(held: &releases::HeldReleases) -> Result<(Vec<u8>, Vec<u8>), BuildError> {
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
    let mut nodes = TokenWriter::new();
    nodes.seq(staging.nodes().len() as u64);
    for (id, node) in staging.nodes() {
        nodes.seq(2);
        nodes.child(id.child_ref());
        nodes.bytes(&node.bytes);
    }
    Ok((manifests.into_bytes(), nodes.into_bytes()))
}
