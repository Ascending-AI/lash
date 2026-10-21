//! Making an artifact from definitions already in hand: lash's own six
//! code-only packages, defined by the crates that implement them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::ArtifactDigest;
use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::FunctionRegistry;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::names::PackagePath;

use crate::package::{Code, PackageDisplay, digest_of};
use crate::{Origin, Package, PackageRefusal, Root};

impl Package {
    /// One of lash's code-only packages: its definitions are what `define`
    /// returns, each after the functions it calls, given a declarations
    /// registry that holds every artifact of `requires`. A constructor
    /// stamps its values with its own identity (`brand.built`), so no
    /// definition depends on the artifact's digest.
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
        define: impl FnOnce(&mut FunctionRegistry) -> Result<Vec<FunctionDefinition>, String>,
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
        let mut registry = FunctionRegistry::declarations();
        for package in requires {
            package
                .code()
                .register_into(&mut registry, &|_| None)
                .map_err(refuse)?;
        }
        let mut code: Vec<(FunctionId, Arc<FunctionDefinition>)> = Vec::new();
        for definition in define(&mut registry).map_err(refuse)? {
            let function = definition.identity();
            if registry.get(&function).is_none() {
                registry
                    .register(definition.clone(), None)
                    .map_err(|error| refuse(error.to_string()))?;
            }
            code.push((function, Arc::new(definition)));
        }
        let ids: Vec<FunctionId> = code.iter().map(|(function, _)| *function).collect();
        let exports = BTreeMap::new();
        let digest = digest_of(&path, Origin::Lash, &requirements, &roots, &exports, &ids);
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

    /// PKG-ROOTS: every definition sits under one of the package's roots.
    pub(crate) fn check_roots(&self) -> Result<(), PackageRefusal> {
        for at in 0..self.code.len() {
            let name = self.code.name(at);
            if !self.roots.iter().any(|root| root.holds(name)) {
                return Err(PackageRefusal::OutsideRoots {
                    package: self.path.clone(),
                    definition: lash_vocabulary::legacy_names::FunctionName::new(name)
                        .unwrap_or_else(|_| {
                            unreachable!("a registered definition's name is a function name")
                        }),
                });
            }
        }
        Ok(())
    }
}
