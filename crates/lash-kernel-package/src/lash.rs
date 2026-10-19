//! Making an artifact from definitions already in hand: lash's own six
//! code-only packages, defined by the crates that implement them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::PackagePath;
use lash_kernel_doc::{
    ArtifactDigest, FunctionDefinition, FunctionId, FunctionRegistry, parse_definition,
};

use crate::package::{Code, PackageDisplay, built_function, digest_of};
use crate::{Origin, Package, PackageRefusal, Root};

impl Package {
    /// One of lash's code-only packages: its definitions are what `define`
    /// returns, each after the functions it calls, given a declarations
    /// registry that holds every artifact of `requires`. With `brands`, the
    /// registry also holds the package's own `<path>.__built() -> Bytes`,
    /// which a constructor stamps its values with: `define` runs once with
    /// it returning no bytes, which fixes the digest, and once more with it
    /// returning that digest.
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::Registry`] when a definition is refused, and
    /// [`PackageRefusal::OutsideRoots`].
    pub fn lash(
        path: PackagePath,
        release: &str,
        roots: impl IntoIterator<Item = Root>,
        requires: &[&Package],
        brands: bool,
        mut define: impl FnMut(&mut FunctionRegistry) -> Result<Vec<FunctionDefinition>, String>,
    ) -> Result<Self, PackageRefusal> {
        let roots: BTreeSet<Root> = roots.into_iter().collect();
        let requirements: BTreeMap<PackagePath, ArtifactDigest> = requires
            .iter()
            .map(|package| (package.path().clone(), package.digest()))
            .collect();
        let refuse = |message: String| PackageRefusal::Registry {
            package: path.clone(),
            message,
        };
        let mut pass = |built: Option<&[u8]>| -> Result<Vec<(FunctionId, Arc<FunctionDefinition>)>, PackageRefusal> {
            let mut registry = FunctionRegistry::declarations();
            for package in requires {
                package
                    .code()
                    .register_into(&mut registry, &|_| None)
                    .map_err(refuse)?;
            }
            let mut code = Vec::new();
            if let Some(bytes) = built {
                let definition = built_definition(&path, bytes);
                let function = registry
                    .register(definition.clone(), None)
                    .map_err(|error| refuse(error.to_string()))?;
                code.push((function, Arc::new(definition)));
            }
            for definition in define(&mut registry).map_err(refuse)? {
                let function = definition.identity();
                if registry.get(&function).is_none() {
                    registry
                        .register(definition.clone(), None)
                        .map_err(|error| refuse(error.to_string()))?;
                }
                code.push((function, Arc::new(definition)));
            }
            Ok(code)
        };
        let first = pass(brands.then_some(&[][..]))?;
        let ids: Vec<FunctionId> = first.iter().map(|(function, _)| *function).collect();
        let exports = BTreeMap::new();
        let digest = digest_of(&path, Origin::Lash, &requirements, &roots, &exports, &ids);
        let code = if brands {
            pass(Some(digest.as_bytes()))?
        } else {
            first
        };
        let package = Self {
            path,
            release: release.to_owned(),
            origin: Origin::Lash,
            requires: requirements,
            roots,
            exports,
            code: Code::defined(code),
            display: PackageDisplay::default(),
            digest,
        };
        package.check_roots()?;
        Ok(package)
    }

    /// PKG-ROOTS: every definition sits under one of the package's roots;
    /// its building-artifact function sits at its path.
    pub(crate) fn check_roots(&self) -> Result<(), PackageRefusal> {
        let built = built_function(&self.path);
        for at in 0..self.code.len() {
            let name = self.code.name(at);
            if name != built.as_str() && !self.roots.iter().any(|root| root.holds(name)) {
                return Err(PackageRefusal::OutsideRoots {
                    package: self.path.clone(),
                    definition: lash_kernel_doc::FunctionName::new(name).unwrap_or_else(|_| {
                        unreachable!("a registered definition's name is a function name")
                    }),
                });
            }
        }
        Ok(())
    }
}

/// `<path>.__built() -> Bytes`, returning `bytes`.
pub(crate) fn built_definition(path: &PackagePath, bytes: &[u8]) -> FunctionDefinition {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    parse_definition(&format!(
        "function {}() -> Bytes\nkernel 1\ncharge 1\nbody {{\n  return b\"{hex}\"\n}}\n",
        built_function(path)
    ))
    .unwrap_or_else(|error| unreachable!("the building-artifact function parses: {error}"))
}
