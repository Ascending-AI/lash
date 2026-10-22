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

#[path = "src/library.rs"]
mod library;
#[path = "src/build/sets.rs"]
mod sets;

use std::collections::BTreeSet;

use lash_kernel_doc::{Document, Export, FunctionExport, StartConvention};

use lash_kernel_doc::{FunctionDefinition, FunctionRegistry};

use lash_vocabulary::{claims::NodeRequirements, names::*};
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
    write_package(&out, &kernel, &[])?;
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
            Ok(lash_dialect_typescript::define_helpers(library)?)
        })?;
        for package in [&regex, &date, &url] {
            write_package(&out, package, &[&kernel])?;
        }
        write_package(&out, &typescript, &requires)?;
        for unit in [&regex, &date, &url, &typescript] {
            release_definitions.extend(unit.definitions.clone());
            release_roots.extend(&unit.roots);
        }
    }
    #[cfg(feature = "dialect-python")]
    {
        let python = helpers("python", "py", &[&kernel], |library| {
            Ok(lash_dialect_python::define_helpers(library)?)
        })?;
        write_package(&out, &python, &[&kernel])?;
        release_definitions.extend(python.definitions.clone());
        release_roots.extend(&python.roots);
    }
    write_helper_document(&out, release_definitions, release_roots)?;
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
) -> Result<(), BuildError> {
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
            registered
                .native()
                .map(|_| (*function, registered.name().to_owned()))
        })
        .collect();
    rows.sort_by_key(|(function, _)| *function);
    let mut source = String::from("&[\n");
    for (function, name) in rows {
        source.push_str(&format!(
            "(lash_vocabulary::names::FunctionId::from_bytes({:?}), {:?}),\n",
            function.as_bytes(),
            name
        ));
    }
    source.push_str("]\n");
    std::fs::write(out.join(format!("{stem}.natives.rs")), source)?;
    Ok(())
}

/// Helpers share one self-contained Document. Pure natives remain declarations
/// in that document; package Imports never dispatch by a global helper name.
#[expect(clippy::disallowed_methods, reason = "build-generated helper document")]
fn write_helper_document(
    out: &std::path::Path,
    definitions: Vec<FunctionDefinition>,
    roots: BTreeSet<FunctionId>,
) -> Result<(), BuildError> {
    let mut functions = BTreeMap::new();
    let mut exports = BTreeMap::new();
    let mut requires = NodeRequirements::default();
    let mut imports = BTreeMap::new();
    let mut packages: BTreeMap<PackagePath, BTreeMap<Segment, ExportName>> = BTreeMap::new();
    for definition in definitions {
        let function = definition.identity();
        if matches!(
            definition.implementation,
            lash_kernel_doc::Implementation::Native { .. }
        ) {
            requires.natives.insert(function);
        }
        requires.kernels.insert(definition.kernel);
        if let Some(body) = definition.body() {
            for (slot, import) in &body.imports {
                if imports
                    .insert(*slot, import.clone())
                    .is_some_and(|prior| prior != *import)
                {
                    return Err(format!("helper Import slot collision at {slot:?}").into());
                }
            }
        }
        if roots.contains(&function) {
            let name = Name::new(definition.name.as_str());
            let public = lash_kernel_dialect::authoring::helper_name(&definition.name);
            if packages
                .entry(public.package)
                .or_default()
                .insert(
                    public.member,
                    ExportName {
                        name: name.clone(),
                        member: None,
                    },
                )
                .is_some()
            {
                return Err(
                    format!("canonical helper name collision for {}", definition.name).into(),
                );
            }
            if exports
                .insert(
                    name,
                    Export::Function(FunctionExport {
                        function,
                        signature: definition.signature.clone(),
                        convention: StartConvention::Direct,
                        asynchronous: false,
                        rest: false,
                    }),
                )
                .is_some()
            {
                return Err(format!("duplicate helper export {}", definition.name).into());
            }
        }
        functions.insert(function, definition);
    }
    let document = Document {
        imports,
        exports,
        main: None,
        functions,
        requires,
        numbers: lash_kernel_doc::NumberPolicy::BySpelling,
        library: Default::default(),
        private_bindings: BTreeSet::new(),
    };
    let mut labels = BTreeMap::<PackagePath, serde_json::Value>::new();
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
            if let Some((id, definition)) = document
                .functions
                .iter()
                .find(|(_, definition)| definition.name.as_str() == name)
            {
                let public = lash_kernel_dialect::authoring::helper_name(&definition.name);
                let key = if roots.contains(id) {
                    "members"
                } else {
                    "definitions"
                };
                let name = if roots.contains(id) {
                    public.member.to_string()
                } else {
                    name.clone()
                };
                labels
                    .entry(public.package)
                    .or_insert_with(|| serde_json::json!({"members":{}, "definitions":{}}))[key]
                    [name] = display.clone();
            }
        }
    }
    std::fs::write(
        out.join("helpers.labels.json"),
        serde_json::to_vec(&labels)?,
    )?;
    std::fs::write(
        out.join("helpers.document.json"),
        serde_json::to_vec(&document)?,
    )?;
    std::fs::write(
        out.join("helpers.packages.json"),
        serde_json::to_vec(&packages)?,
    )?;
    Ok(())
}
