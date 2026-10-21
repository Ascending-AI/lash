//! The libraries the compiler reads: each dialect's helpers as a worker
//! registers them, over the kernel library and the extensions they call.

use std::sync::Arc;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::FunctionRegistry;
use lash_vocabulary::legacy_names::FunctionId;

/// A dialect whose helpers the compiler reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Python,
    TypeScript,
}

impl Dialect {
    pub const ALL: [Self; 2] = [Self::Python, Self::TypeScript];

    pub fn name(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }
}

/// A registry holding a dialect's helpers, and the helpers in the order
/// they were registered: each after every function its body calls.
pub struct Library {
    pub packages: Vec<lash_kernel_package::Package>,
    pub registry: FunctionRegistry,
    pub helpers: Vec<FunctionId>,
}

impl Library {
    /// The admitted canonical definitions of `dialect`'s helpers, as a
    /// worker registers them.
    ///
    /// # Errors
    ///
    /// Any refusal of registration or of the helper sources, as text.
    pub fn of(dialect: Dialect) -> Result<Self, String> {
        let kernel = package("kernel", None, &[], |registry| {
            lash_kernel_vm::register_machine_functions(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_numbers(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_collections(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_text_json(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_brands(registry).map_err(|e| e.to_string())?;
            Ok(())
        })?;
        let mut packages = vec![kernel];
        if dialect == Dialect::TypeScript {
            let kernel = &packages[0];
            let regex = package("regex.ecma", Some("regex.ecma"), &[kernel], |registry| {
                lash_ext_regex_ecma::register(registry)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })?;
            let date = package("date.ecma", Some("date.ecma"), &[kernel], |registry| {
                lash_ext_date_ecma::register(registry)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })?;
            let url = package("url.whatwg", Some("url.whatwg"), &[kernel], |registry| {
                lash_ext_url_whatwg::register(registry)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })?;
            packages.extend([regex, date, url]);
        }
        let path = dialect.name();
        let root = if dialect == Dialect::TypeScript {
            "ts"
        } else {
            "py"
        };
        let requires: Vec<_> = packages.iter().collect();
        let helpers_package = lash_kernel_package::Package::lash(
            lash_vocabulary::names::PackagePath::parse(path).map_err(|e| e.to_string())?,
            "1.0",
            [lash_kernel_package::Root::new(root)],
            &requires,
            |registry| {
                let mut names = NamedLibrary::from_registry(registry).map_err(|e| e.to_string())?;
                match dialect {
                    Dialect::Python => lash_dialect_python::define_helpers(&mut names),
                    Dialect::TypeScript => lash_dialect_typescript::define_helpers(&mut names),
                }
                .map_err(|e| e.to_string())
            },
        )
        .map_err(|e| e.to_string())?;
        let helpers = helpers_package.code().ids().to_vec();
        packages.push(helpers_package);
        let mut registry = FunctionRegistry::declarations();
        for package in &packages {
            package.code().register_into(&mut registry, &|_| None)?;
        }
        lash_kernel_lib::implement_brands(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_vm::implement_machine_functions(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::implement_numbers(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::implement_collections(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::implement_text_json(&mut registry).map_err(|e| e.to_string())?;
        for package in packages.iter().skip(1) {
            match package.path().to_string().as_str() {
                "regex.ecma" => {
                    lash_ext_regex_ecma::implement(
                        &mut registry,
                        &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
                    )
                    .map_err(|e| e.to_string())?;
                }
                "date.ecma" => lash_ext_date_ecma::implement(
                    &mut registry,
                    package.code().ids().iter().copied(),
                )
                .map_err(|e| e.to_string())?,
                "url.whatwg" => lash_ext_url_whatwg::implement(
                    &mut registry,
                    package.code().ids().iter().copied(),
                )
                .map_err(|e| e.to_string())?,
                _ => {}
            }
        }
        Ok(Self {
            packages,
            registry,
            helpers,
        })
    }

    /// The package dependencies, before the dialect definitions:
    /// independent helper-source laws extend this view.
    ///
    /// # Errors
    ///
    /// A registry refusal when the admitted dependencies cannot be registered.
    pub fn base_registry(&self) -> Result<FunctionRegistry, String> {
        let helpers: std::collections::BTreeSet<_> = self.helpers.iter().copied().collect();
        let mut visited = std::collections::BTreeSet::new();
        let mut definitions = vec![];
        for (id, _) in self.registry.iter() {
            if !helpers.contains(id) {
                ordered(*id, &self.registry, &mut visited, &mut definitions);
            }
        }
        let mut base = FunctionRegistry::declarations();
        for definition in definitions {
            let native = self
                .registry
                .get(&definition.identity())
                .and_then(|function| function.native().cloned());
            base.register(definition, native)
                .map_err(|e| e.to_string())?;
        }
        Ok(base)
    }

    pub fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.registry
            .get(function)
            .map(|registered| registered.definition().as_ref())
    }
}

/// Builds a source package in the same dependency order as the release build.
fn package(
    path: &str,
    root: Option<&str>,
    requires: &[&lash_kernel_package::Package],
    mut define: impl FnMut(&mut FunctionRegistry) -> Result<(), String>,
) -> Result<lash_kernel_package::Package, String> {
    use lash_kernel_package::Package;
    use lash_kernel_package::Root;
    use lash_vocabulary::names::PackagePath;
    use std::collections::BTreeSet;
    let mut registered = |registry: &mut FunctionRegistry| {
        let before: BTreeSet<_> = registry.iter().map(|(id, _)| *id).collect();
        define(registry)?;
        let mut visited = before;
        let mut definitions = vec![];
        for (id, _) in registry.iter() {
            ordered(*id, registry, &mut visited, &mut definitions);
        }
        Ok(definitions)
    };
    let roots = if let Some(root) = root {
        vec![Root::new(root)]
    } else {
        let mut probe = FunctionRegistry::declarations();
        registered(&mut probe)?
            .iter()
            .map(|definition| {
                let name = definition.name.as_str();
                Root::new(name.split('.').next().unwrap_or(name))
            })
            .collect()
    };
    Package::lash(
        PackagePath::parse(path).map_err(|e| e.to_string())?,
        "1.0",
        roots,
        requires,
        registered,
    )
    .map_err(|e| e.to_string())
}

fn ordered(
    id: FunctionId,
    registry: &FunctionRegistry,
    visited: &mut std::collections::BTreeSet<FunctionId>,
    definitions: &mut Vec<FunctionDefinition>,
) {
    if !visited.insert(id) {
        return;
    }
    let Some(function) = registry.get(&id) else {
        return;
    };
    if let Some(body) = function.definition().body() {
        for callee in body.functions.keys() {
            ordered(*callee, registry, visited, definitions);
        }
    }
    definitions.push(FunctionDefinition::clone(function.definition()));
}

#[cfg(test)]
mod tests {
    use lash_kernel_package::Package;
    use lash_kernel_package::Root;
    use lash_vocabulary::names::PackagePath;

    use super::*;

    /// A definition no TypeScript helper calls, under `regex.ecma`.
    const UNRELATED_REGEX: &str = "function regex.ecma.unrelated(value: Text) -> Text\nkernel 1\ncharge 1\nbody {\n  return value\n}\n";

    /// A definition no TypeScript helper calls, under the kernel's `text`.
    const UNRELATED_KERNEL: &str = "function text.unrelated(value: Text) -> Text\nkernel 1\ncharge 1\nbody {\n  return value\n}\n";

    /// Registers `extra`, when given, beside a package's own definitions.
    fn with(registry: &mut FunctionRegistry, extra: Option<&str>) -> Result<(), String> {
        if let Some(text) = extra {
            let definition =
                lash_kernel_doc::parse_definition(text).map_err(|error| error.to_string())?;
            registry
                .register(definition, None)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// The `regex.ecma` and `typescript` packages, as the release build
    /// makes them, with `kernel` and `regex` definitions added.
    fn typescript(kernel: Option<&str>, regex: Option<&str>) -> (Package, Package) {
        let kernel = package("kernel", None, &[], |registry| {
            lash_kernel_vm::register_machine_functions(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_numbers(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_collections(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_text_json(registry).map_err(|e| e.to_string())?;
            lash_kernel_lib::register_brands(registry).map_err(|e| e.to_string())?;
            with(registry, kernel)
        })
        .unwrap();
        let regex = package("regex.ecma", Some("regex.ecma"), &[&kernel], |registry| {
            lash_ext_regex_ecma::register(registry).map_err(|e| e.to_string())?;
            with(registry, regex)
        })
        .unwrap();
        let date = package("date.ecma", Some("date.ecma"), &[&kernel], |registry| {
            lash_ext_date_ecma::register(registry)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let url = package("url.whatwg", Some("url.whatwg"), &[&kernel], |registry| {
            lash_ext_url_whatwg::register(registry)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let typescript = Package::lash(
            PackagePath::parse("typescript").unwrap(),
            "1.0",
            [Root::new("ts")],
            &[&kernel, &regex, &date, &url],
            |registry| {
                let mut names = NamedLibrary::from_registry(registry).map_err(|e| e.to_string())?;
                lash_dialect_typescript::define_helpers(&mut names).map_err(|e| e.to_string())
            },
        )
        .unwrap();
        (regex, typescript)
    }

    /// Every function of `package`, by name.
    fn identities(package: &Package) -> std::collections::BTreeMap<String, FunctionId> {
        let code = package.code();
        (0..code.len())
            .map(|at| (code.name(at).to_owned(), code.ids()[at]))
            .collect()
    }

    /// PKG-STABLE (FIG-6987): a release that changes one definition of
    /// `regex.ecma` or of the kernel that no TypeScript helper calls is a
    /// new artifact of each package that requires it, and leaves every
    /// TypeScript constructor's and helper's identity unchanged, so a run
    /// parked on them survives it.
    #[test]
    fn pkg_stable_an_unrelated_release_keeps_every_typescript_helper_identity() {
        let (regex, base) = typescript(None, None);
        let helpers = identities(&base);
        for name in ["ts.promise.start", "ts.for_of.open", "ts.array.from"] {
            assert!(helpers.contains_key(name), "{name} is a helper");
        }
        for (changed_kernel, changed_regex) in [
            (None, Some(UNRELATED_REGEX)),
            (Some(UNRELATED_KERNEL), None),
        ] {
            let (other_regex, other) = typescript(changed_kernel, changed_regex);
            assert_ne!(
                other_regex.digest(),
                regex.digest(),
                "another regex.ecma artifact"
            );
            assert_ne!(other.digest(), base.digest(), "another typescript artifact");
            assert_eq!(
                identities(&other),
                helpers,
                "every helper keeps its identity"
            );
        }
    }
}
