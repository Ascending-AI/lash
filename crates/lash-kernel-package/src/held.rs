//! What one node holds: every artifact, the sets they make up, its own
//! set, and the one registry and member index derived from them (V1).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_helper_labels::MemberDisplay;
use lash_kernel_dialect::{Export, MemberPath, OperationExport, PackagePath, QualifiedOperation};
use lash_kernel_doc::{
    FunctionId, FunctionName, FunctionRegistry, NativeFunction, PackageSetDigest,
};

use crate::{ArtifactKey, Origin, Package, PackageRefusal, PackageSet, Root};

/// The artifacts a node holds, registered once by its host and read by
/// the parent's admission and every worker (V1).
#[derive(Clone, Debug)]
pub struct HeldPackages {
    pub(crate) artifacts: BTreeMap<ArtifactKey, Arc<Package>>,
    pub(crate) sets: BTreeMap<PackageSetDigest, PackageSet>,
    pub(crate) own: PackageSetDigest,
    /// Every member each function backs, across the held artifacts.
    pub(crate) index: BTreeMap<FunctionId, Vec<(PackagePath, MemberPath)>>,
    /// The same, with the artifact that declares each member.
    pub(crate) backing: BTreeMap<FunctionId, Vec<(ArtifactKey, MemberPath)>>,
    /// Every artifact's code as declarations, each function once for every
    /// kernel version the build interprets.
    pub(crate) functions: Arc<FunctionRegistry>,
    pub(crate) frame: Arc<[u8]>,
    pub(crate) live_frame: OnceLock<&'static [u8]>,
}

/// A view of a set the node does not hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("package set {packages} is not held by this node")]
pub struct SetNotHeld {
    pub packages: PackageSetDigest,
}

impl HeldPackages {
    /// A node holding exactly `packages`, which are its own set.
    ///
    /// # Errors
    ///
    /// [`HeldPackages::new`]'s, and [`PackageSet::new`]'s.
    pub fn install(packages: impl IntoIterator<Item = Package>) -> Result<Self, PackageRefusal> {
        let packages: Vec<Arc<Package>> = packages.into_iter().map(Arc::new).collect();
        let own = PackageSet::new(packages.iter().map(AsRef::as_ref))?;
        let digest = own.digest();
        Self::new(packages, [own], digest)
    }

    /// A node holding `artifacts` and the `sets` they make up, `own` among
    /// them: what a store's rows or a pool frame reconstruct.
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::OwnNotHeld`]; a set naming an artifact the node is
    /// not given ([`PackageRefusal::ArtifactNotHeld`]) or that is not
    /// dependency-closed ([`PackageRefusal::NotClosed`]); a host package
    /// whose path meets a root a lash package lists
    /// ([`PackageRefusal::ReservedRoot`]); and a definition the registry
    /// refuses ([`PackageRefusal::Registry`]).
    pub fn new(
        artifacts: impl IntoIterator<Item = Arc<Package>>,
        sets: impl IntoIterator<Item = PackageSet>,
        own: PackageSetDigest,
    ) -> Result<Self, PackageRefusal> {
        let artifacts: BTreeMap<ArtifactKey, Arc<Package>> = artifacts
            .into_iter()
            .map(|package| (package.key(), package))
            .collect();
        let sets: BTreeMap<PackageSetDigest, PackageSet> =
            sets.into_iter().map(|set| (set.digest(), set)).collect();
        if !sets.contains_key(&own) {
            return Err(PackageRefusal::OwnNotHeld);
        }
        for set in sets.values() {
            set.check_closed(|key| artifacts.get(key).map(AsRef::as_ref))?;
        }
        check_reserved_roots(artifacts.values().map(AsRef::as_ref))?;
        let (index, backing) = member_index(&artifacts);
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
        let functions = Arc::new(
            interpreted(written).map_err(|message| PackageRefusal::Interpretation { message })?,
        );
        let mut held = Self {
            artifacts,
            sets,
            own,
            index,
            backing,
            functions,
            frame: Arc::from([]),
            live_frame: OnceLock::new(),
        };
        held.frame = crate::frame::build(&held)?;
        Ok(held)
    }

    pub fn artifacts(&self) -> &BTreeMap<ArtifactKey, Arc<Package>> {
        &self.artifacts
    }

    /// The sets the node knows whose members it all holds.
    pub fn sets(&self) -> &BTreeMap<PackageSetDigest, PackageSet> {
        &self.sets
    }

    /// The set this node built at start: the fleet gate's first candidate,
    /// and nothing else.
    pub fn own(&self) -> PackageSetDigest {
        self.own
    }

    /// One set's names, exports and definitions.
    ///
    /// # Errors
    ///
    /// [`SetNotHeld`].
    pub fn view(&self, packages: &PackageSetDigest) -> Result<PackageView<'_>, SetNotHeld> {
        let set = self.sets.get(packages).ok_or(SetNotHeld {
            packages: *packages,
        })?;
        Ok(PackageView { held: self, set })
    }

    /// Every member `function` backs, across the held artifacts, sorted
    /// and once each. A definition that backs none has none.
    pub fn member_index(&self, function: &FunctionId) -> &[(PackagePath, MemberPath)] {
        self.index.get(function).map_or(&[], Vec::as_slice)
    }

    /// Every held artifact's code as declarations: what a parent links,
    /// admits and migrates documents against.
    pub fn functions(&self) -> &Arc<FunctionRegistry> {
        &self.functions
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
/// and owned-path authority read them (P3).
#[derive(Clone, Copy, Debug)]
pub struct PackageView<'a> {
    held: &'a HeldPackages,
    set: &'a PackageSet,
}

impl<'a> PackageView<'a> {
    pub fn digest(&self) -> PackageSetDigest {
        self.set.digest()
    }

    pub fn set(&self) -> &'a PackageSet {
        self.set
    }

    /// The set's packages, in path order.
    pub fn packages(&self) -> impl Iterator<Item = &'a Package> + 'a {
        let held = self.held;
        self.set
            .keys()
            .filter_map(move |key| held.artifacts.get(&key).map(AsRef::as_ref))
    }

    /// The set's package at `path`.
    pub fn package(&self, path: &PackagePath) -> Option<&'a Package> {
        let digest = self.set.members().get(path)?;
        self.held
            .artifacts
            .get(&ArtifactKey {
                path: path.clone(),
                digest: *digest,
            })
            .map(AsRef::as_ref)
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
        let mut members: Vec<_> = self
            .held
            .backing
            .get(function)
            .into_iter()
            .flatten()
            .filter(|(key, _)| self.set.holds(key))
            .map(|(key, member)| (key.path.clone(), member.clone()))
            .collect();
        members.dedup();
        members
    }

    /// The label of `member` of the set's package at `path`.
    pub fn display(&self, path: &PackagePath, member: &MemberPath) -> Option<&'a MemberDisplay> {
        self.package(path)?.display().members.get(member)
    }

    /// The label of `function`: its first member's, else its defining
    /// package's label of its definition.
    pub fn display_for_function(&self, function: &FunctionId) -> Option<&'a MemberDisplay> {
        if let Some((path, member)) = self.members(function).first() {
            return self.display(path, member);
        }
        self.packages().find_map(|package| {
            let at = package.code().ids().iter().position(|id| id == function)?;
            package
                .display()
                .definitions
                .iter()
                .find(|(name, _)| name.as_str() == package.code().name(at))
                .map(|(_, label)| label)
        })
    }

    /// A function's package-owned title, or its document name when this
    /// selected set has no label for it, including a removed definition.
    pub fn title<'b>(&'b self, function: &FunctionId, name: &'b FunctionName) -> &'b str {
        self.display_for_function(function)
            .map_or_else(|| name.as_str(), |display| display.title.as_str())
    }
}

/// The member index: every member each function backs, by the artifact
/// declaring it.
#[allow(clippy::type_complexity)]
pub(crate) fn member_index(
    artifacts: &BTreeMap<ArtifactKey, Arc<Package>>,
) -> (
    BTreeMap<FunctionId, Vec<(PackagePath, MemberPath)>>,
    BTreeMap<FunctionId, Vec<(ArtifactKey, MemberPath)>>,
) {
    let mut backing: BTreeMap<FunctionId, Vec<(ArtifactKey, MemberPath)>> = BTreeMap::new();
    for (key, package) in artifacts {
        for (member, definition) in crate::members(package) {
            if let Some(function) = package.function(definition.as_str()) {
                backing
                    .entry(function)
                    .or_default()
                    .push((key.clone(), member));
            }
        }
    }
    let mut index = BTreeMap::new();
    for (function, members) in &mut backing {
        members.sort();
        let mut paths: Vec<(PackagePath, MemberPath)> = members
            .iter()
            .map(|(key, member)| (key.path.clone(), member.clone()))
            .collect();
        paths.sort();
        paths.dedup();
        index.insert(*function, paths);
    }
    (index, backing)
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
/// version the build interprets.
fn interpreted(written: FunctionRegistry) -> Result<FunctionRegistry, String> {
    let mut registry = written;
    for version in lash_kernel_doc::KernelVersion::ALL {
        if let Some(migration) = lash_kernel_migrate::migration_from(*version) {
            lash_kernel_migrate::migrate_registry(&mut registry, migration)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(registry)
}
