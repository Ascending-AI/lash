//! Makes lash's own packages once (FIG-6917), and from their definitions
//! each built dialect's current helper release. Workers and parents read
//! the same build artifacts.

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

use std::collections::BTreeSet;

use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry};
use lash_kernel_package::{Package, PackagePath, Root};

use crate::releases::{HelperRelease, HelperReleaseIndex};

type BuildError = Box<dyn std::error::Error>;

#[expect(
    clippy::disallowed_methods,
    reason = "a build script reads OUT_DIR and writes its outputs there"
)]
fn main() -> Result<(), BuildError> {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    let kernel = package("kernel", None, &[], &mut |registry| {
        library::define_kernel(registry)
    })?;
    write_package(&out, &kernel, &[])?;
    std::fs::write(
        out.join("kernel_helpers.rs"),
        sets::kernel_source(&registry_of(&[&kernel])?)?,
    )?;
    #[cfg(feature = "dialect-typescript")]
    {
        let regex = package(
            "regex.ecma",
            Some("regex.ecma"),
            &[&kernel],
            &mut |registry| library::define_regex(registry),
        )?;
        let date = package(
            "date.ecma",
            Some("date.ecma"),
            &[&kernel],
            &mut |registry| library::define_date(registry),
        )?;
        let url = package(
            "url.whatwg",
            Some("url.whatwg"),
            &[&kernel],
            &mut |registry| library::define_url(registry),
        )?;
        let requires = [&kernel, &regex, &date, &url];
        let typescript = helpers("typescript", "ts", &requires, |library| {
            Ok(lash_dialect_typescript::define_helpers(library)?)
        })?;
        for package in [&regex, &date, &url] {
            write_package(&out, package, &[&kernel])?;
        }
        write_package(&out, &typescript, &requires)?;
        dialect(
            &out,
            "typescript",
            &[&kernel, &regex, &date, &url, &typescript],
            &typescript,
        )?;
    }
    #[cfg(feature = "dialect-python")]
    {
        let python = helpers("python", "py", &[&kernel], |library| {
            Ok(lash_dialect_python::define_helpers(library)?)
        })?;
        write_package(&out, &python, &[&kernel])?;
        dialect(&out, "python", &[&kernel, &python], &python)?;
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    Ok(())
}

/// The lash package at `path` whose definitions `define` registers over
/// `requires`, under `root`, or, with none, under the first segment of
/// each of its definitions' names (the kernel library's families).
fn package(
    path: &str,
    root: Option<&str>,
    requires: &[&Package],
    define: &mut dyn FnMut(&mut FunctionRegistry) -> Result<(), String>,
) -> Result<Package, BuildError> {
    let mut registered = |registry: &mut FunctionRegistry| {
        let before: BTreeSet<FunctionId> = registry.iter().map(|(function, _)| *function).collect();
        define(registry)?;
        Ok(defined_since(registry, &before))
    };
    let roots: Vec<Root> = match root {
        Some(root) => vec![Root::new(root)],
        None => {
            let mut probe = registry_of(requires)?;
            registered(&mut probe)?
                .iter()
                .map(|definition| {
                    let name = definition.name.as_str();
                    Root::new(name.split('.').next().unwrap_or(name))
                })
                .collect()
        }
    };
    Ok(Package::lash(
        PackagePath::parse(path)?,
        library::RELEASE,
        roots,
        requires,
        false,
        registered,
    )?)
}

/// A dialect's helpers as a package: defined against the names of every
/// function `requires` holds.
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn helpers(
    path: &str,
    root: &str,
    requires: &[&Package],
    define: impl Fn(
        &mut lash_kernel_dialect::NamedLibrary,
    ) -> Result<Vec<FunctionDefinition>, BuildError>,
) -> Result<Package, BuildError> {
    Ok(Package::lash(
        PackagePath::parse(path)?,
        library::RELEASE,
        [Root::new(root)],
        requires,
        false,
        |registry| {
            let mut library = lash_kernel_dialect::NamedLibrary::from_registry(registry)
                .map_err(|error| error.to_string())?;
            define(&mut library).map_err(|error| error.to_string())
        },
    )?)
}

/// The functions `registry` holds that `before` did not, each after the
/// functions it calls.
fn defined_since(
    registry: &FunctionRegistry,
    before: &BTreeSet<FunctionId>,
) -> Vec<FunctionDefinition> {
    release::current(registry)
        .functions
        .into_iter()
        .filter(|function| !before.contains(&function.function))
        .map(|function| function.definition)
        .collect()
}

/// A declarations registry holding the code of every package of `packages`.
fn registry_of(packages: &[&Package]) -> Result<FunctionRegistry, BuildError> {
    let mut registry = FunctionRegistry::declarations();
    for package in packages {
        package.code().register_into(&mut registry, &|_| None)?;
    }
    Ok(registry)
}

/// Writes `package` as its header and the release image of its code,
/// validated against `requires`, the packages it requires.
#[expect(
    clippy::disallowed_methods,
    reason = "a build script writes its OUT_DIR artifacts"
)]
fn write_package(
    out: &std::path::Path,
    package: &Package,
    requires: &[&Package],
) -> Result<(), BuildError> {
    let stem = package.path().to_string().replace('.', "_");
    let mut checked = registry_of(requires)?;
    let mut definitions = Vec::with_capacity(package.code().len());
    for at in 0..package.code().len() {
        definitions.push(FunctionDefinition::clone(
            package.code().definition(at)?.as_ref(),
        ));
    }
    let validated = checked.validate_functions(definitions)?;
    std::fs::write(
        out.join(format!("{stem}.package.json")),
        serde_json::to_vec(&package.header())?,
    )?;
    std::fs::write(
        out.join(format!("{stem}.bin")),
        validated.to_image(&lash_kernel_vm::body_census),
    )?;
    Ok(())
}

/// Resolves and checks `dialect`'s helper releases over `packages`, the
/// packages it ships with, `helpers` its own, and writes them under the
/// dialect's name.
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
#[expect(
    clippy::disallowed_methods,
    reason = "a build script writes its OUT_DIR artifacts"
)]
fn dialect(
    out: &std::path::Path,
    dialect: &str,
    packages: &[&Package],
    helpers: &Package,
) -> Result<(), BuildError> {
    let write =
        |file: &str, bytes: &[u8]| std::fs::write(out.join(format!("{dialect}_{file}")), bytes);
    let registry = registry_of(packages)?;
    let helper_names: Vec<_> = (0..helpers.code().len())
        .map(|at| lash_kernel_doc::FunctionName::new(helpers.code().name(at)))
        .collect::<Result<_, _>>()?;
    sets::validate(&registry, &helper_names, dialect == "typescript")?;
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
