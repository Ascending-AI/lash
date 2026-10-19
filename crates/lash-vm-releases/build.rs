//! Defines and validates each built dialect's current helpers once.
//! Workers and parents read the same build artifacts.

#![cfg_attr(
    not(any(feature = "dialect-typescript", feature = "dialect-python")),
    expect(
        dead_code,
        unused_imports,
        reason = "a build with no dialect defines and checks no release"
    )
)]

#[path = "src/library.rs"]
mod library;
#[path = "src/build/release.rs"]
mod release;
#[expect(
    dead_code,
    reason = "the build writes releases; decoding the held set are the library's"
)]
#[path = "src/releases.rs"]
mod releases;
#[path = "src/build/sets.rs"]
mod sets;

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
    let mut kernel = FunctionRegistry::new();
    library::register_kernel_base(&mut kernel)?;
    write_functions(
        &out.join("kernel_base.bin"),
        &FunctionRegistry::new(),
        &kernel,
    )?;
    let basis = kernel.clone();
    lash_kernel_lib::register_collections(&mut kernel)?;
    write_kernel_set(&out, &kernel)?;
    write_functions(&out.join("kernel_collections.bin"), &basis, &kernel)?;
    #[cfg(feature = "dialect-typescript")]
    {
        let mut registry = kernel.clone();
        library::register_extensions(&mut registry)?;
        lash_ext_date_ecma::register(&mut registry)?;
        lash_ext_url_whatwg::register(&mut registry)?;
        write_functions(&out.join("typescript_extensions.bin"), &kernel, &registry)?;
        dialect(
            &out,
            "typescript",
            registry,
            |library| Ok(lash_dialect_typescript::define_helpers(library)?),
            library::implement_typescript,
        )?;
    }
    #[cfg(feature = "dialect-python")]
    {
        dialect(
            &out,
            "python",
            kernel,
            |library| Ok(lash_dialect_python::define_helpers(library)?),
            library::implement_python,
        )?;
    }
    #[cfg(not(any(feature = "dialect-typescript", feature = "dialect-python")))]
    let _ = (&out, "typescript", "python");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    Ok(())
}

/// Defines `dialect`'s helpers against `registry`, the library it ships
/// with, adds the natives of its operations (`implement`), resolves and
/// checks its releases, and writes all of it under the dialect's name.
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
    implement: fn(&mut FunctionRegistry, &[lash_kernel_doc::FunctionId]) -> Result<(), String>,
) -> Result<(), BuildError> {
    let write =
        |file: &str, bytes: &[u8]| std::fs::write(out.join(format!("{dialect}_{file}")), bytes);
    let mut named = NamedLibrary::from_registry(&registry)?;
    let helpers = define(&mut named)?;
    let ty = match dialect {
        "typescript" => "TypeScriptHelper",
        "python" => "PythonHelper",
        _ => return Err("undeclared dialect".into()),
    };
    write(
        "helpers.rs",
        sets::enum_source(ty, helpers.iter().map(|f| f.name.as_str()))?.as_bytes(),
    )?;
    let helper_names: Vec<_> = helpers.iter().map(|helper| helper.name.clone()).collect();
    let validated = registry.validate_functions(helpers)?;
    let ids: Vec<_> = validated.iter().map(|(function, _)| *function).collect();
    implement(&mut registry, &ids)?;
    sets::validate(&registry, &helper_names, dialect == "typescript")?;
    write(
        "helpers.bin",
        &validated.to_image(&lash_kernel_vm::body_census),
    )?;

    let held = releases::HeldReleases {
        retained: vec![release::current(&registry)],
    };
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
    use lash_checkpoint_encoder::{EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TypedBody};
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

#[expect(
    clippy::disallowed_methods,
    reason = "a build script writes its OUT_DIR artifacts"
)]
fn write_kernel_set(out: &std::path::Path, registry: &FunctionRegistry) -> Result<(), BuildError> {
    std::fs::write(
        out.join("kernel_helpers.rs"),
        sets::kernel_source(registry)?,
    )?;
    Ok(())
}

/// Reuses the release's dependency order and the helper boot image.
#[expect(
    clippy::disallowed_methods,
    reason = "the build writes its OUT_DIR artifacts"
)]
fn write_functions(
    path: &std::path::Path,
    basis: &FunctionRegistry,
    registry: &FunctionRegistry,
) -> Result<(), BuildError> {
    let ordered = release::current(registry);
    let (base, definitions): (Vec<_>, Vec<_>) = ordered
        .functions
        .into_iter()
        .partition(|function| basis.get(&function.function).is_some());
    let mut checked = FunctionRegistry::declarations();
    checked.validate_functions(base.into_iter().map(|f| f.definition).collect())?;
    let validated =
        checked.validate_functions(definitions.into_iter().map(|f| f.definition).collect())?;
    std::fs::write(path, validated.to_image(&lash_kernel_vm::body_census))?;
    Ok(())
}
