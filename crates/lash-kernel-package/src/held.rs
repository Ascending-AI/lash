//! What one node was built with: its package set, every artifact of it,
//! and the one registry and member index derived from them (V1). A node
//! holds no other set: work linked under another set runs from that set's
//! stored definitions (FIG-6985, FIG-6993).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_helper_labels::MemberDisplay;
use lash_kernel_dialect::{
    Export, ExportView, MemberPath, OperationExport, PackagePath, QualifiedOperation, Segment,
};
use lash_kernel_doc::{
    FunctionId, FunctionName, FunctionRegistry, NativeFunction, PackageSetDigest,
};

use crate::{
    ArtifactKey, Origin, Package, PackageRefusal, PackageSet, Root, SnapshotPackage,
    SurfaceSnapshot,
};

/// The package set a node was built with, registered once by its host and
/// read by the parent's admission and every worker (V1).
#[derive(Clone, Debug)]
pub struct HeldPackages {
    pub(crate) artifacts: BTreeMap<ArtifactKey, Arc<Package>>,
    pub(crate) set: PackageSet,
    /// The set's surface, as the node stores it at install.
    pub(crate) surface: Arc<SurfaceSnapshot>,
    /// Every artifact's code as declarations, each function once for every
    /// kernel version the build interprets.
    pub(crate) functions: Arc<FunctionRegistry>,
    pub(crate) frame: Arc<[u8]>,
    pub(crate) live_frame: OnceLock<&'static [u8]>,
}

impl HeldPackages {
    /// A node built with exactly `packages`, as one set.
    ///
    /// # Errors
    ///
    /// [`HeldPackages::new`]'s, and [`PackageSet::new`]'s.
    pub fn install(packages: impl IntoIterator<Item = Package>) -> Result<Self, PackageRefusal> {
        let packages: Vec<Arc<Package>> = packages.into_iter().map(Arc::new).collect();
        let set = PackageSet::new(packages.iter().map(AsRef::as_ref))?;
        Self::new(packages, set)
    }

    /// A node built with `set`, whose members are `artifacts`: what a pool
    /// frame reconstructs.
    ///
    /// # Errors
    ///
    /// An artifact `set` does not name ([`PackageRefusal::NotMember`]); a
    /// member the node is not given ([`PackageRefusal::ArtifactNotHeld`])
    /// or a set that is not dependency-closed
    /// ([`PackageRefusal::NotClosed`]); a host package whose path meets a
    /// root a lash package lists ([`PackageRefusal::ReservedRoot`]); and a
    /// definition the registry refuses ([`PackageRefusal::Registry`]).
    pub fn new(
        artifacts: impl IntoIterator<Item = Arc<Package>>,
        set: PackageSet,
    ) -> Result<Self, PackageRefusal> {
        let artifacts: BTreeMap<ArtifactKey, Arc<Package>> = artifacts
            .into_iter()
            .map(|package| (package.key(), package))
            .collect();
        if let Some(key) = artifacts
            .keys()
            .find(|key| set.members().get(&key.path) != Some(&key.digest))
        {
            return Err(PackageRefusal::NotMember { key: key.clone() });
        }
        set.check_closed(|key| artifacts.get(key).map(AsRef::as_ref))?;
        check_reserved_roots(artifacts.values().map(AsRef::as_ref))?;
        let mut written = FunctionRegistry::declarations();
        for package in registration_order(&artifacts) {
            package
                .code()
                .register_into(&mut written, &|_| None)
                .map_err(|message| PackageRefusal::Registry {
                    package: package.path().clone(),
                    message,
                })?;
        }
        let (functions, roots) =
            interpreted(written).map_err(|message| PackageRefusal::Interpretation { message })?;
        let functions = Arc::new(functions);
        let surface = surface(&artifacts, &set, &functions);
        let mut held = Self {
            artifacts,
            set,
            surface,
            functions,
            frame: Arc::from([]),
            live_frame: OnceLock::new(),
        };
        held.frame = crate::frame::build(&held, &roots)?;
        Ok(held)
    }

    pub fn artifacts(&self) -> &BTreeMap<ArtifactKey, Arc<Package>> {
        &self.artifacts
    }

    /// The node's set.
    pub fn set(&self) -> &PackageSet {
        &self.set
    }

    /// The node's set's digest: the surface it advertises as its candidate
    /// for new cells.
    pub fn digest(&self) -> PackageSetDigest {
        self.set.digest()
    }

    /// The node's set's names, exports and definitions.
    pub fn view(&self) -> PackageView<'_> {
        PackageView::new(&self.surface, &self.functions)
    }

    /// The node's set's surface: what the node stores for it at install.
    pub fn surface(&self) -> &Arc<SurfaceSnapshot> {
        &self.surface
    }

    /// Every held artifact's code as declarations: what a parent links,
    /// admits and migrates documents against.
    pub fn functions(&self) -> &Arc<FunctionRegistry> {
        &self.functions
    }

    /// The node's registry with `stored` registered over it: the
    /// definitions a stored surface names that the node does not hold, read
    /// back from the store, each checked against what it calls and
    /// registered after it (FIG-6991). What a writer of a cell under that
    /// surface reads it over.
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::Registry`] for a definition the registry refuses.
    pub fn catalog_with(
        &self,
        stored: &BTreeMap<FunctionId, Arc<lash_kernel_doc::FunctionDefinition>>,
    ) -> Result<Arc<FunctionRegistry>, PackageRefusal> {
        let mut registry = FunctionRegistry::clone(&self.functions);
        crate::Code::defined(
            stored
                .iter()
                .map(|(function, definition)| (*function, Arc::clone(definition)))
                .collect(),
        )
        .register_into(&mut registry, &|_| None)
        .map_err(|message| PackageRefusal::StoredDefinition { message })?;
        Ok(Arc::new(registry))
    }

    /// The node's registry with every definition of `others` it lacks
    /// registered over it: one catalog that reads each of their surfaces,
    /// as a node reads other builds' stored sets over the code the store
    /// holds for them (FIG-6993).
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::StoredDefinition`] for a definition the registry
    /// refuses.
    pub fn catalog_over<'a>(
        &self,
        others: impl IntoIterator<Item = &'a HeldPackages>,
    ) -> Result<Arc<FunctionRegistry>, PackageRefusal> {
        let stored = others
            .into_iter()
            .flat_map(|other| other.functions().iter())
            .filter(|(function, _)| self.functions.get(function).is_none())
            .map(|(function, registered)| (*function, Arc::clone(registered.definition())))
            .collect();
        self.catalog_with(&stored)
    }

    /// The roots lash's held packages list: no host package path meets one.
    pub fn reserved_roots(&self) -> BTreeSet<&Root> {
        reserved(self.artifacts.values().map(AsRef::as_ref))
            .into_keys()
            .collect()
    }

    /// Every held artifact, each after every held artifact it requires.
    pub fn in_order(&self) -> Vec<&Arc<Package>> {
        registration_order(&self.artifacts)
    }

    /// Registers every held artifact's code in `registry`, requirements
    /// first, each function with the native `native` gives it: how a worker
    /// derives its registry from this one registration.
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::Registry`].
    pub fn register_into(
        &self,
        registry: &mut FunctionRegistry,
        native: &dyn Fn(&FunctionId) -> Option<Arc<dyn NativeFunction>>,
    ) -> Result<(), PackageRefusal> {
        for package in registration_order(&self.artifacts) {
            package
                .code()
                .register_into(registry, native)
                .map_err(|message| PackageRefusal::Registry {
                    package: package.path().clone(),
                    message,
                })?;
        }
        Ok(())
    }
}

/// One set's packages, as lowering, relink, prompts, namespace composition
/// and owned-path authority read them (P3): the set's surface snapshot over
/// a catalog that holds every definition it names.
#[derive(Clone, Copy, Debug)]
pub struct PackageView<'a> {
    surface: &'a SurfaceSnapshot,
    catalog: &'a Arc<FunctionRegistry>,
}

impl<'a> PackageView<'a> {
    /// `surface` read over `catalog`, which holds every definition it
    /// names: a held set's over the node's registry, a stored one's over
    /// that registry and the definitions the store holds for it.
    pub fn new(surface: &'a SurfaceSnapshot, catalog: &'a Arc<FunctionRegistry>) -> Self {
        Self { surface, catalog }
    }

    pub fn digest(&self) -> PackageSetDigest {
        self.surface.digest()
    }

    pub fn surface(&self) -> &'a SurfaceSnapshot {
        self.surface
    }

    /// The set's packages, in path order.
    pub fn packages(&self) -> impl Iterator<Item = &'a SnapshotPackage> + 'a {
        self.surface.packages()
    }

    /// Every definition of the set's packages, by name with its identity:
    /// the one name table relink and lowering resolve the cell's library
    /// references in. A definition of another set is never in it.
    pub fn definitions(&self) -> impl Iterator<Item = (&'a str, FunctionId)> + 'a {
        self.packages().flat_map(|package| {
            package
                .functions()
                .iter()
                .map(|(name, function)| (name.as_str(), *function))
        })
    }

    /// Every function the catalog holds by identity: the set's, and every
    /// other the node holds, so a document written under another set reads
    /// as written.
    pub fn catalog(&self) -> &'a Arc<FunctionRegistry> {
        self.catalog
    }

    /// The set's functions by name over `registry`'s by identity: what a
    /// front end lowers a cell of this set against and what a carried
    /// saved function relinks to (K-LIB-009).
    ///
    /// # Errors
    ///
    /// Two of the set's definitions under one name.
    pub fn library(
        &self,
        registry: &FunctionRegistry,
    ) -> Result<lash_kernel_dialect::NamedLibrary, lash_kernel_dialect::LibraryError> {
        let functions = self.surface.functions();
        lash_kernel_dialect::NamedLibrary::resolving(registry, |function| {
            functions.contains(function)
        })
    }

    /// The set's package at `path`.
    pub fn package(&self, path: &PackagePath) -> Option<&'a SnapshotPackage> {
        self.surface.package(path)
    }

    /// The operation `operation` names, when the set's package at its path
    /// exports one.
    pub fn operation(&self, operation: &QualifiedOperation) -> Option<&'a OperationExport> {
        match self
            .package(&operation.package)?
            .exports()
            .get(&operation.name)?
        {
            Export::Operation(export) => Some(export),
            _ => None,
        }
    }

    /// Every member of the set's packages `function` backs.
    pub fn members(&self, function: &FunctionId) -> Vec<(PackagePath, MemberPath)> {
        self.surface.members(function).to_vec()
    }

    /// The label of `member` of the set's package at `path`.
    pub fn display(&self, path: &PackagePath, member: &MemberPath) -> Option<&'a MemberDisplay> {
        self.package(path)?.display().members.get(member)
    }

    /// The label of `function`: its first member's, else its defining
    /// package's label of its definition.
    pub fn display_for_function(&self, function: &FunctionId) -> Option<&'a MemberDisplay> {
        if let Some((path, member)) = self.surface.members(function).first() {
            return self.display(path, member);
        }
        self.packages().find_map(|package| {
            let (name, _) = package
                .functions()
                .iter()
                .find(|(_, defined)| defined == function)?;
            package.display().definitions.get(name)
        })
    }

    /// A function's package-owned title, or its document name when this
    /// selected set has no label for it, including a removed definition.
    pub fn title<'b>(&'b self, function: &FunctionId, name: &'b FunctionName) -> &'b str {
        self.display_for_function(function)
            .map_or_else(|| name.as_str(), |display| display.title.as_str())
    }
}

/// The set's host packages and their code exports, as namespace
/// composition and member dispatch read them. lash's own packages export
/// nothing to user code; their reserved roots still belong to the snapshot.
impl ExportView for PackageView<'_> {
    fn namespace_roots(&self) -> BTreeSet<String> {
        self.packages()
            .map(|package| package.path().as_slice()[0].as_str().to_owned())
            .collect()
    }

    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        self.package(path)
            .filter(|package| package.origin() == Origin::Host)
            .map(SnapshotPackage::exports)
    }

    fn paths(&self) -> Vec<PackagePath> {
        self.packages()
            .filter(|package| package.origin() == Origin::Host)
            .map(|package| package.path().clone())
            .collect()
    }

    fn reserves(&self, root: &str) -> Option<String> {
        let path = PackagePath::parse(root).ok()?;
        self.packages()
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

/// `set`'s surface, over `registry`, which holds its code.
pub(crate) fn surface(
    artifacts: &BTreeMap<ArtifactKey, Arc<Package>>,
    set: &PackageSet,
    registry: &FunctionRegistry,
) -> Arc<SurfaceSnapshot> {
    Arc::new(SurfaceSnapshot::of(
        set,
        |key| artifacts.get(key).map(AsRef::as_ref),
        registry,
    ))
}

/// Every root a lash package lists, with that package.
fn reserved<'a>(
    artifacts: impl Iterator<Item = &'a Package>,
) -> BTreeMap<&'a Root, &'a PackagePath> {
    artifacts
        .filter(|package| package.origin() == Origin::Lash)
        .flat_map(|package| {
            package
                .roots()
                .iter()
                .map(move |root| (root, package.path()))
        })
        .collect()
}

/// PKG-ROOTS: refuses a host package whose path meets a lash package's
/// root.
pub(crate) fn check_reserved_roots<'a>(
    artifacts: impl Iterator<Item = &'a Package> + Clone,
) -> Result<(), PackageRefusal> {
    let reserved = reserved(artifacts.clone());
    for package in artifacts.filter(|package| package.origin() == Origin::Host) {
        if let Some((root, owner)) = reserved.iter().find(|(root, _)| root.meets(package.path())) {
            return Err(PackageRefusal::ReservedRoot {
                package: package.path().clone(),
                root: root.to_string(),
                owner: (*owner).clone(),
            });
        }
    }
    Ok(())
}

/// The artifacts, each after every held artifact it requires.
fn registration_order(artifacts: &BTreeMap<ArtifactKey, Arc<Package>>) -> Vec<&Arc<Package>> {
    fn place<'a>(
        key: &ArtifactKey,
        artifacts: &'a BTreeMap<ArtifactKey, Arc<Package>>,
        placed: &mut BTreeSet<ArtifactKey>,
        order: &mut Vec<&'a Arc<Package>>,
    ) {
        let Some(package) = artifacts.get(key) else {
            return;
        };
        if !placed.insert(key.clone()) {
            return;
        }
        for (path, digest) in package.requires() {
            let required = ArtifactKey {
                path: path.clone(),
                digest: *digest,
            };
            place(&required, artifacts, placed, order);
        }
        order.push(package);
    }
    let mut placed = BTreeSet::new();
    let mut order = Vec::with_capacity(artifacts.len());
    for key in artifacts.keys() {
        place(key, artifacts, &mut placed, &mut order);
    }
    order
}

/// `written` with each function redeclared for every successor kernel
/// version the build interprets, and each redeclaration's root: the
/// function as written, whose native it holds.
fn interpreted(
    written: FunctionRegistry,
) -> Result<(FunctionRegistry, BTreeMap<FunctionId, FunctionId>), String> {
    let mut registry = written;
    let mut roots = BTreeMap::new();
    for version in lash_kernel_doc::KernelVersion::ALL {
        if let Some(migration) = lash_kernel_migrate::migration_from(*version) {
            for (from, to) in lash_kernel_migrate::migrate_registry(&mut registry, migration)
                .map_err(|error| error.to_string())?
            {
                let root = roots.get(&from).copied().unwrap_or(from);
                roots.insert(to, root);
            }
        }
    }
    Ok((registry, roots))
}

/// An owned handle to a package surface, for runtime service boundaries:
/// the node's own set, or a stored surface over the node's registry.
#[derive(Clone, Debug)]
pub struct SharedPackageView {
    surface: Arc<SurfaceSnapshot>,
    catalog: Arc<FunctionRegistry>,
}

impl SharedPackageView {
    /// The set this producer registered at node construction.
    pub fn declared(held: Arc<HeldPackages>) -> Self {
        Self {
            surface: Arc::clone(held.surface()),
            catalog: Arc::clone(held.functions()),
        }
    }

    /// The stored `surface`, read over `catalog` (FIG-6993): a set that is
    /// not the node's own, whose names and roots it shows.
    pub fn stored(surface: Arc<SurfaceSnapshot>, catalog: Arc<FunctionRegistry>) -> Self {
        Self { surface, catalog }
    }

    fn view(&self) -> PackageView<'_> {
        PackageView::new(&self.surface, &self.catalog)
    }
}

impl ExportView for SharedPackageView {
    fn namespace_roots(&self) -> BTreeSet<String> {
        self.view().namespace_roots()
    }

    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        self.surface
            .package(path)
            .filter(|package| package.origin() == Origin::Host)
            .map(crate::SnapshotPackage::exports)
    }
    fn reserves(&self, root: &str) -> Option<String> {
        self.view().reserves(root)
    }
    fn paths(&self) -> Vec<PackagePath> {
        self.view().paths()
    }
}
