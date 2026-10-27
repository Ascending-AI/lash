//! Makes lash's own packages once (FIG-6917) and checks each built
//! dialect's helper names. Workers and parents read the same build
//! artifacts.

#![cfg_attr(
    not(any(feature = "dialect-typescript", feature = "dialect-python")),
    expect(
        dead_code,
        unused_imports,
        reason = "a build with no dialect defines and checks no release"
    )
)]

#[allow(dead_code, reason = "archive readers are shared with release checks")]
#[path = "src/archive.rs"]
mod archive;

#[path = "src/library.rs"]
mod library;
#[path = "src/build/sets.rs"]
mod sets;

use std::collections::BTreeSet;

use lash_kernel_doc::{FunctionDefinition, FunctionRegistry};

use lash_kernel_value::names::*;
use std::collections::BTreeMap;

struct ReleaseUnit {
    path: String,
    definitions: Vec<FunctionDefinition>,
    roots: BTreeSet<FunctionId>,
}
impl ReleaseUnit {
    fn ids(&self) -> Vec<FunctionId> {
        self.definitions
            .iter()
            .map(FunctionDefinition::identity)
            .collect()
    }
}

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
    let mut release_natives = write_package(&out, &kernel, &[])?;
    let mut release_packages = vec![(kernel.path.clone(), kernel.roots.clone())];
    let mut release_definitions = kernel.definitions.clone();
    let mut release_roots = kernel.roots.clone();
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
            Ok(lash_ext_ts_helpers::package::define_helpers(library)?)
        })?;
        for package in [&regex, &date, &url] {
            release_natives.extend(write_package(&out, package, &[&kernel])?);
        }
        release_natives.extend(write_package(&out, &typescript, &requires)?);
        for unit in [&regex, &date, &url, &typescript] {
            release_packages.push((unit.path.clone(), unit.roots.clone()));
            release_definitions.extend(unit.definitions.clone());
            release_roots.extend(&unit.roots);
        }
    }
    #[cfg(feature = "dialect-python")]
    {
        let python = helpers("python", "py", &[&kernel], |library| {
            Ok(lash_ext_py_helpers::package::define_helpers(library)?)
        })?;
        release_natives.extend(write_package(&out, &python, &[&kernel])?);
        release_packages.push((python.path.clone(), python.roots.clone()));
        release_definitions.extend(python.definitions.clone());
        release_roots.extend(&python.roots);
    }
    write_helper_library(
        &out,
        release_definitions,
        release_roots,
        release_packages,
        release_natives,
    )?;
    println!("cargo:rerun-if-changed=archive");
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
    requires: &[&ReleaseUnit],
    define: &mut dyn FnMut(&mut FunctionRegistry) -> Result<(), String>,
) -> Result<ReleaseUnit, BuildError> {
    let mut registry = registry_of(requires)?;
    let before: BTreeSet<_> = registry.iter().map(|(id, _)| *id).collect();
    define(&mut registry)?;
    let definitions = defined_since(&registry, &before);
    if let Some(root) = root {
        for definition in &definitions {
            if !definition.name.as_str().starts_with(&format!("{root}.")) {
                return Err(format!("helper {} is outside {root}", definition.name).into());
            }
        }
    }
    Ok(ReleaseUnit {
        path: path.to_owned(),
        roots: definitions
            .iter()
            .map(FunctionDefinition::identity)
            .collect(),
        definitions,
    })
}

#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn helpers(
    path: &str,
    _root: &str,
    requires: &[&ReleaseUnit],
    define: impl Fn(
        &mut lash_kernel_dialect::NamedLibrary,
    ) -> Result<Vec<FunctionDefinition>, BuildError>,
) -> Result<ReleaseUnit, BuildError> {
    let registry = registry_of(requires)?;
    let roots: BTreeSet<_> = requires
        .iter()
        .flat_map(|unit| unit.roots.iter().copied())
        .collect();
    let mut library =
        lash_kernel_dialect::NamedLibrary::resolving(&registry, |id| roots.contains(id))?;
    let definitions = define(&mut library)?;
    let roots = library
        .iter()
        .map(|(_, id)| id)
        .filter(|id| {
            definitions
                .iter()
                .any(|definition| definition.identity() == *id)
        })
        .collect();
    Ok(ReleaseUnit {
        path: path.to_owned(),
        definitions,
        roots,
    })
}

/// The functions `registry` holds that `before` did not, each after the
/// functions it calls.
fn defined_since(
    registry: &FunctionRegistry,
    before: &BTreeSet<FunctionId>,
) -> Vec<FunctionDefinition> {
    let mut listed = BTreeSet::new();
    let mut functions = Vec::new();
    for (function, _) in registry.iter() {
        in_call_order(*function, registry, &mut listed, &mut functions);
    }
    functions
        .into_iter()
        .filter(|(function, _)| !before.contains(function))
        .map(|(_, definition)| definition)
        .collect()
}

/// Lists `function` after every function it calls.
fn in_call_order(
    function: FunctionId,
    registry: &FunctionRegistry,
    listed: &mut BTreeSet<FunctionId>,
    functions: &mut Vec<(FunctionId, FunctionDefinition)>,
) {
    if listed.contains(&function) {
        return;
    }
    let Some(registered) = registry.get(&function) else {
        return;
    };
    if let Some(body) = registered.definition().body() {
        for called in body.functions.keys() {
            in_call_order(*called, registry, listed, functions);
        }
    }
    listed.insert(function);
    functions.push((function, FunctionDefinition::clone(registered.definition())));
}

/// A declarations registry holding the code of every package of `packages`.
fn registry_of(packages: &[&ReleaseUnit]) -> Result<FunctionRegistry, BuildError> {
    let mut registry = FunctionRegistry::declarations();
    for package in packages {
        for definition in &package.definitions {
            if registry.get(&definition.identity()).is_none() {
                registry.register(definition.clone(), None)?;
            }
        }
    }
    Ok(registry)
}

/// Writes the release image of `package` code,
/// validated against `requires`, the packages it requires.
#[expect(
    clippy::disallowed_methods,
    reason = "a build script writes its OUT_DIR artifacts"
)]
fn write_package(
    out: &std::path::Path,
    package: &ReleaseUnit,
    requires: &[&ReleaseUnit],
) -> Result<BTreeMap<FunctionId, archive::NativeIdentity>, BuildError> {
    let stem = package.path.to_string().replace('.', "_");
    let mut checked = registry_of(requires)?;
    let validated = checked.validate_functions(package.definitions.clone())?;
    std::fs::write(
        out.join(format!("{stem}.bin")),
        validated.to_image(&|definition| lash_kernel_vm::body_census(definition, &checked)),
    )?;
    // Native membership is derived from the source owner's installer once,
    // at build time. Workers consume sorted constructor tables, never harvest
    // a second registry to discover this membership.
    for dependency in requires {
        library::implement(
            &dependency.path.to_string(),
            &mut checked,
            &dependency.ids(),
        )?;
    }
    library::implement(&package.path.to_string(), &mut checked, &package.ids())?;
    let mut rows: Vec<_> = package
        .ids()
        .iter()
        .filter_map(|function| {
            let registered = checked.get(function)?;
            registered.native().map(|_| {
                (
                    *function,
                    registered.name().to_owned(),
                    registered.definition().native_version,
                )
            })
        })
        .collect();
    rows.sort_by_key(|(function, _, _)| *function);
    let mut natives = BTreeMap::new();
    let mut source = String::from("&[\n");
    for (function, name, version) in rows {
        natives.insert(
            function,
            archive::NativeIdentity {
                package: package.path.clone(),
                name: name.clone(),
                native_version: version,
            },
        );
        source.push_str(&format!(
            "(lash_kernel_value::names::FunctionId::from_bytes({:?}), {:?}, {:?}),\n",
            function.as_bytes(),
            name,
            version
        ));
    }
    source.push_str("]\n");
    std::fs::write(out.join(format!("{stem}.natives.rs")), source)?;
    Ok(natives)
}

/// Helpers share one self-contained Document. Pure natives remain declarations
/// in that document; package Imports never dispatch by a global helper name.
#[expect(clippy::disallowed_methods, reason = "build-generated helper document")]
fn write_helper_library(
    out: &std::path::Path,
    definitions: Vec<FunctionDefinition>,
    roots: BTreeSet<FunctionId>,
    packages: Vec<(String, BTreeSet<FunctionId>)>,
    natives: BTreeMap<FunctionId, archive::NativeIdentity>,
) -> Result<(), BuildError> {
    let mut registry = FunctionRegistry::declarations();
    let mut pending: BTreeMap<_, _> = definitions
        .into_iter()
        .map(|definition| (definition.identity(), definition))
        .collect();
    while !pending.is_empty() {
        let next = pending
            .iter()
            .find(|(_, definition)| {
                definition
                    .body()
                    .is_none_or(|body| body.functions.keys().all(|id| registry.get(id).is_some()))
            })
            .map(|(id, _)| *id)
            .ok_or("helper build image is not statically closed")?;
        registry.register(
            pending
                .remove(&next)
                .ok_or("helper definition disappeared")?,
            None,
        )?;
    }
    let helpers = lash_kernel_doc::BuildHelpers::new(registry, roots.iter().copied())?;
    std::fs::write(
        out.join("helpers.identity.rs"),
        format!(
            "lash_kernel_value::HelperLibraryId::from_bytes({:?})",
            helpers.identity().as_bytes()
        ),
    )?;
    std::fs::write(out.join("helpers.roots.json"), serde_json::to_vec(&roots)?)?;
    std::fs::write(
        out.join("helpers.members.json"),
        serde_json::to_vec(&helpers.members().collect::<Vec<_>>())?,
    )?;
    let mut labels = BTreeMap::<FunctionId, serde_json::Value>::new();
    for source in [
        include_str!("labels/kernel.json"),
        include_str!("labels/typescript.json"),
        include_str!("labels/python.json"),
        include_str!("labels/regex.json"),
        include_str!("labels/date.json"),
        include_str!("labels/url.json"),
    ] {
        let source: serde_json::Value = serde_json::from_str(source)?;
        for (name, display) in source["definitions"].as_object().into_iter().flatten() {
            for (id, definition) in helpers
                .functions()
                .iter()
                .filter(|(_, definition)| definition.name() == name)
            {
                let _ = definition;
                labels.insert(*id, display.clone());
            }
        }
    }
    std::fs::write(
        out.join("helpers.labels.json"),
        serde_json::to_vec(&labels)?,
    )?;
    write_archive_candidate(out, &helpers, packages, labels, natives)?;
    Ok(())
}

/// Read only checked-in inputs and append to an explicit build-local candidate.
#[expect(clippy::disallowed_methods, reason = "build outputs stay in OUT_DIR")]
fn write_archive_candidate(
    out: &std::path::Path,
    helpers: &lash_kernel_doc::BuildHelpers,
    packages: Vec<(String, BTreeSet<FunctionId>)>,
    labels: BTreeMap<FunctionId, serde_json::Value>,
    natives: BTreeMap<FunctionId, archive::NativeIdentity>,
) -> Result<(), BuildError> {
    let started = std::time::Instant::now();
    let archive = archive::Archive {
        releases: (include!("src/archive_releases.rs") as &[&str])
            .iter()
            .map(|source| serde_json::from_str(source))
            .collect::<Result<_, _>>()?,
        ledger: serde_json::from_str(include_str!("archive/ledger.json"))?,
    };
    let _ = archive.validate()?;
    let definitions: BTreeMap<_, _> = helpers
        .functions()
        .iter()
        .map(|(id, registered)| (*id, FunctionDefinition::clone(registered.definition())))
        .collect();
    let representations = serde_json::from_str(include_str!("archive/representations.json"))?;
    // Feature-disabled packages retain their last recorded release. A full-feature
    // recorder is the release-cut authority, never a feature subset or synthetic run.
    let mut release = archive.releases.last().cloned().unwrap_or_default();
    release.definitions = definitions.clone();
    release.labels.extend(labels);
    release.representations = representations;
    for (package, roots) in packages {
        release
            .natives
            .retain(|_, identity| identity.package != package);
        release
            .bindings
            .retain(|helper, _| !helper.0.starts_with(&format!("{package}:")));
        let library =
            lash_kernel_doc::BuildHelpers::new(helpers.functions().clone(), roots.iter().copied())?;
        release.libraries.insert(
            package.clone(),
            archive::Library {
                identity: library.identity(),
                members: library.members().copied().collect(),
                roots: roots.clone(),
            },
        );
        for id in roots {
            let definition = &definitions[&id];
            if definition.name.as_str() == "kernel.anonymous" {
                continue;
            }
            let contract = archive::contract(id, &definitions, &release.representations)
                .ok_or("contract reach is not closed")?;
            release.bindings.insert(
                archive::HelperId(format!("{package}:{}", definition.name)),
                archive::Binding {
                    revision: contract.revision(),
                    function: id,
                },
            );
        }
    }
    release.natives.extend(natives);
    release.library = archive::Library {
        members: release
            .libraries
            .values()
            .flat_map(|library| library.members.iter().copied())
            .collect(),
        roots: release
            .libraries
            .values()
            .flat_map(|library| library.roots.iter().copied())
            .collect(),
        identity: lash_kernel_value::HelperLibraryId::of_members(
            release
                .libraries
                .values()
                .flat_map(|library| library.members.iter().copied()),
        ),
    };
    let candidate = archive.append(release)?;
    std::fs::write(
        out.join("archive.candidate.json"),
        serde_json::to_vec(&candidate)?,
    )?;
    // Timing is diagnostic output, never an input to a compiled helper image.
    eprintln!(
        "helper archive assembly and validation: {} us",
        started.elapsed().as_micros()
    );
    Ok(())
}
