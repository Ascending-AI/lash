//! A host's explicit package and operation attachment for one workflow.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{Export, ExportView, PackagePath, QualifiedOperation, Segment};
use lash_kernel_doc::FunctionId;

use crate::SurfaceSnapshot;

/// Packages whose code a workflow may reach, with the exact operations it
/// may perform in each. An empty operation set admits code only. Dependencies
/// are declared explicitly too; selection never expands a host's declaration.
pub type PackageSelection = BTreeMap<PackagePath, BTreeSet<Segment>>;

/// The selected exports of a stored surface. Its namespace roots remain
/// reserved, so an omitted package cannot turn into a deferred tool.
pub struct SelectedPackages<'a> {
    surface: &'a SurfaceSnapshot,
    selection: &'a PackageSelection,
    functions: BTreeSet<FunctionId>,
    exports: BTreeMap<PackagePath, BTreeMap<Segment, Export>>,
}

impl<'a> SelectedPackages<'a> {
    pub fn new(surface: &'a SurfaceSnapshot, selection: &'a PackageSelection) -> Self {
        let exports = surface
            .packages()
            .filter(|package| {
                package.origin() == crate::Origin::Host && selection.contains_key(package.path())
            })
            .map(|package| {
                let exports = package
                    .exports()
                    .iter()
                    .filter(|(name, export)| {
                        !matches!(export, Export::Operation(_))
                            || selection[package.path()].contains(*name)
                    })
                    .map(|(name, export)| (name.clone(), export.clone()))
                    .collect();
                (package.path().clone(), exports)
            })
            .collect();
        let functions = surface
            .packages()
            .filter(|package| selection.contains_key(package.path()))
            .flat_map(|package| package.ids())
            .collect();
        Self {
            surface,
            selection,
            exports,
            functions,
        }
    }

    pub fn permits_function(&self, function: &FunctionId) -> bool {
        self.functions.contains(function)
    }

    /// Named requirements outside this declaration. Facts come from the
    /// checker's structural reach, including transitively called bodies.
    pub fn undeclared<'b>(
        &self,
        functions: &BTreeMap<FunctionId, lash_kernel_doc::FunctionName>,
        effects: impl Iterator<Item = &'b lash_kernel_doc::EffectName>,
    ) -> Vec<String> {
        let mut missing = BTreeSet::new();
        for (id, name) in functions {
            if !self.permits_function(id) {
                let owners: Vec<_> = self
                    .surface
                    .packages()
                    .filter(|package| package.ids().any(|found| found == *id))
                    .map(|package| package.path().to_string())
                    .collect();
                let members = self.surface.members(id);
                if members.is_empty() {
                    missing.insert(format!("{name} (packages: {})", owners.join(", ")));
                } else {
                    missing.extend(
                        members
                            .iter()
                            .map(|(package, member)| format!("{package}.{member}")),
                    );
                }
            }
        }
        for effect in effects {
            if let Ok(operation) = QualifiedOperation::from_effect(effect)
                && !self.permits_operation(&operation)
            {
                missing.insert(effect.to_string());
            }
        }
        missing.into_iter().collect()
    }

    /// Operations outside installed packages belong to the separately attached
    /// tool environment. Package-owned operations require explicit membership.
    pub fn permits_operation(&self, operation: &QualifiedOperation) -> bool {
        self.surface.package(&operation.package).is_none()
            || self
                .selection
                .get(&operation.package)
                .is_some_and(|names| names.contains(&operation.name))
    }
}

impl ExportView for SelectedPackages<'_> {
    fn namespace_roots(&self) -> BTreeSet<String> {
        self.surface
            .packages()
            .map(|package| package.path().as_slice()[0].as_str().to_owned())
            .collect()
    }
    fn paths(&self) -> Vec<PackagePath> {
        self.exports.keys().cloned().collect()
    }
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        self.exports.get(path)
    }
    fn reserves(&self, root: &str) -> Option<String> {
        use crate::Origin;
        let path = PackagePath::parse(root).ok()?;
        self.surface
            .packages()
            .filter(|package| package.origin() == Origin::Lash)
            .find_map(|package| {
                let reserved = package.roots().iter().find(|root| root.meets(&path))?;
                Some(format!(
                    "the root `{reserved}` of lash's package `{}`",
                    package.path()
                ))
            })
    }
}

/// A catalog restricted to the declared packages, for admission by identity.
pub struct SelectedCatalog<'a> {
    pub catalog: &'a dyn lash_kernel_doc::FunctionCatalog,
    pub packages: &'a SelectedPackages<'a>,
}
impl lash_kernel_doc::FunctionCatalog for SelectedCatalog<'_> {
    fn definition(&self, function: &FunctionId) -> Option<&lash_kernel_doc::FunctionDefinition> {
        self.packages
            .permits_function(function)
            .then(|| self.catalog.definition(function))
            .flatten()
    }
    fn registered(&self, function: &FunctionId) -> Option<&lash_kernel_doc::RegisteredFunction> {
        self.packages
            .permits_function(function)
            .then(|| self.catalog.registered(function))
            .flatten()
    }
}

impl SurfaceSnapshot {
    /// Explicit whole-surface attachment, for hosts choosing every installed
    /// package and operation. Workflow APIs never select this implicitly.
    pub fn selection(&self) -> PackageSelection {
        self.packages()
            .map(|package| {
                (
                    package.path().clone(),
                    package
                        .exports()
                        .iter()
                        .filter_map(|(name, export)| {
                            matches!(export, Export::Operation(_)).then_some(name.clone())
                        })
                        .collect(),
                )
            })
            .collect()
    }
}
