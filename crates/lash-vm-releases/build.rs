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

use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry};
use lash_kernel_package::{Package, PackagePath, Root};

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
        dialect(&[&kernel, &regex, &date, &url, &typescript])?;
    }
    #[cfg(feature = "dialect-python")]
    {
        let python = helpers("python", "py", &[&kernel], |library| {
            Ok(lash_dialect_python::define_helpers(library)?)
        })?;
        write_package(&out, &python, &[&kernel])?;
        dialect(&[&kernel, &python])?;
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
    // Native membership is derived from the source owner's installer once,
    // at build time. Workers consume sorted constructor tables, never harvest
    // a second registry to discover this membership.
    for dependency in requires {
        library::implement(
            &dependency.path().to_string(),
            &mut checked,
            dependency.code().ids(),
        )?;
    }
    library::implement(
        &package.path().to_string(),
        &mut checked,
        package.code().ids(),
    )?;
    let mut rows: Vec<_> = package
        .code()
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
            "(lash_kernel_doc::FunctionId::from_bytes({:?}), {:?}),\n",
            function.as_bytes(),
            name
        ));
    }
    source.push_str("]\n");
    std::fs::write(out.join(format!("{stem}.natives.rs")), source)?;
    Ok(())
}

/// Checks a dialect's helper names over `packages`, the packages it ships
/// with.
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn dialect(packages: &[&Package]) -> Result<(), BuildError> {
    let registry = registry_of(packages)?;
    sets::validate(&registry, packages)?;
    Ok(())
}
