//! A package set's surface, as the store keeps it (FIG-6991).
//!
//! A surface snapshot is everything a writer reads of a set's packages to
//! write a cell under it, with no package code: each artifact's header
//! (path, origin, requirements, roots and exports, with their types and
//! member conventions and every operation's declaration), the identity of
//! each definition by name, the labels, and the execution requirements the
//! set's code derives. A node writes its set's snapshot when it installs
//! it, keyed by the set's digest, and never changes it. Lowering,
//! namespace composition, discovery, prompts, saved-function rebasing and
//! workflow import all read one snapshot ([`PackageView`]), over a catalog
//! that holds the definitions it names: a node that does not hold the
//! set's artifacts reads them from the store.
//!
//! A snapshot lists every definition of each package, helpers included,
//! and that membership is authority: a package's internal operations are
//! performed only by its own definitions (Ruling 5). So a snapshot is made
//! only from artifacts a host installed ([`crate::HeldPackages`]), and the
//! node's install is its one writer to the store; a stored one is only
//! ever read back ([`SurfaceSnapshot::decode`]), checked against its key.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{Export, MemberPath, PackagePath};
use lash_kernel_doc::{
    ArtifactDigest, ExecutionRequirements, FunctionId, FunctionName, FunctionRegistry,
    NodeCapabilities, PackageSetDigest,
};
use serde::{Deserialize, Serialize};

use crate::{ArtifactKey, Origin, Package, PackageDisplay, PackageSet, Root};

/// An artifact's labels: its release label and its display labels. No
/// digest covers them, so they are not in a snapshot's bytes: two releases
/// of identical content share one snapshot, and the store keeps each
/// release's labels beside it, in install order (FIG-6991).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageLabels {
    pub release: String,
    #[serde(default, skip_serializing_if = "PackageDisplay::is_empty")]
    pub display: PackageDisplay,
}

impl PackageLabels {
    /// The labels' stored JSON.
    pub fn encode(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| unreachable!("labels serialize: string keys, no floats"))
    }

    /// The labels `text` spells.
    ///
    /// # Errors
    ///
    /// Text that is not labels: the store is corrupt.
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// One artifact of a snapshot: its path, origin, exact requirements,
/// roots, exports and digest, and every definition by name in registration
/// order; its labels ride beside it, not in its bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPackage {
    path: PackagePath,
    origin: Origin,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    requires: BTreeMap<PackagePath, ArtifactDigest>,
    roots: BTreeSet<Root>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    exports: BTreeMap<lash_kernel_dialect::Segment, Export>,
    digest: ArtifactDigest,
    functions: Vec<(FunctionName, FunctionId)>,
    #[serde(skip)]
    labels: PackageLabels,
}

impl SnapshotPackage {
    /// The surface of `package`, with its labels.
    pub fn of(package: &Package) -> Self {
        let code = package.code();
        Self {
            path: package.path().clone(),
            origin: package.origin(),
            requires: package.requires().clone(),
            roots: package.roots().clone(),
            exports: package.exports().clone(),
            digest: package.digest(),
            functions: code
                .ids()
                .iter()
                .enumerate()
                .map(|(at, function)| {
                    (
                        FunctionName::new(code.name(at))
                            .unwrap_or_else(|_| unreachable!("a registered definition's name")),
                        *function,
                    )
                })
                .collect(),
            labels: PackageLabels {
                release: package.release().to_owned(),
                display: package.display().clone(),
            },
        }
    }

    pub fn path(&self) -> &PackagePath {
        &self.path
    }

    /// The host's release label: display and messages only.
    pub fn release(&self) -> &str {
        &self.labels.release
    }

    pub fn origin(&self) -> Origin {
        self.origin
    }

    pub fn requires(&self) -> &BTreeMap<PackagePath, ArtifactDigest> {
        &self.requires
    }

    pub fn roots(&self) -> &BTreeSet<Root> {
        &self.roots
    }

    pub fn exports(&self) -> &BTreeMap<lash_kernel_dialect::Segment, Export> {
        &self.exports
    }

    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn key(&self) -> ArtifactKey {
        ArtifactKey {
            path: self.path.clone(),
            digest: self.digest,
        }
    }

    pub fn display(&self) -> &PackageDisplay {
        &self.labels.display
    }

    pub fn labels(&self) -> &PackageLabels {
        &self.labels
    }

    /// Every definition, by name with its identity, in registration order.
    pub fn functions(&self) -> &[(FunctionName, FunctionId)] {
        &self.functions
    }

    /// Every definition's identity, in registration order.
    pub fn ids(&self) -> impl Iterator<Item = FunctionId> + '_ {
        self.functions.iter().map(|(_, function)| *function)
    }

    /// The identity of the definition named `name`.
    pub fn function(&self, name: &str) -> Option<FunctionId> {
        self.functions
            .iter()
            .find(|(defined, _)| defined.as_str() == name)
            .map(|(_, function)| *function)
    }

    /// The kernel error kind of the export `segment` when it is an error:
    /// `<path>.<segment>`, derived and never written.
    pub fn error_kind(&self, segment: &lash_kernel_dialect::Segment) -> Option<String> {
        matches!(self.exports().get(segment), Some(Export::Error))
            .then(|| format!("{}.{segment}", self.path()))
    }
}

/// The stored spelling of a snapshot: its packages in path order and its
/// requirements.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    packages: Vec<SnapshotPackage>,
    requirements: ExecutionRequirements,
}

/// A package set's surface: what every writer of a cell under the set
/// reads, and what running its code requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceSnapshot {
    digest: PackageSetDigest,
    packages: BTreeMap<PackagePath, SnapshotPackage>,
    requirements: ExecutionRequirements,
    /// Every member each function backs, by package and member, sorted.
    backing: BTreeMap<FunctionId, Vec<(PackagePath, MemberPath)>>,
    /// Every definition of each package, helpers included.
    definitions: BTreeMap<PackagePath, BTreeSet<FunctionId>>,
}

/// A stored snapshot that does not read as the snapshot of the set its
/// key names: the store is corrupt.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotRefusal {
    #[error("the stored surface of package set {packages} does not decode: {message}")]
    Undecodable {
        packages: PackageSetDigest,
        message: String,
    },
    #[error("the stored surface keyed {packages} is the surface of package set {found}")]
    Digest {
        packages: PackageSetDigest,
        found: PackageSetDigest,
    },
}

impl SurfaceSnapshot {
    /// The surface of `set`, whose every artifact `artifact` finds, with
    /// the requirements its code derives over `registry`, which holds it.
    pub(crate) fn of<'a>(
        set: &PackageSet,
        artifact: impl Fn(&ArtifactKey) -> Option<&'a Package>,
        registry: &FunctionRegistry,
    ) -> Self {
        let packages: Vec<&Package> = set.keys().filter_map(|key| artifact(&key)).collect();
        Self::assemble(
            packages.iter().map(|package| SnapshotPackage::of(package)),
            requirements(&packages, registry),
        )
    }

    fn assemble(
        packages: impl IntoIterator<Item = SnapshotPackage>,
        requirements: ExecutionRequirements,
    ) -> Self {
        let packages: BTreeMap<PackagePath, SnapshotPackage> = packages
            .into_iter()
            .map(|package| (package.path().clone(), package))
            .collect();
        let digest = PackageSet::from_members(
            packages
                .iter()
                .map(|(path, package)| (path.clone(), package.digest))
                .collect(),
        )
        .digest();
        let mut backing: BTreeMap<FunctionId, Vec<(PackagePath, MemberPath)>> = BTreeMap::new();
        for package in packages.values() {
            for (member, definition) in crate::export_members(package.exports()) {
                if let Some(function) = package.function(definition.as_str()) {
                    backing
                        .entry(function)
                        .or_default()
                        .push((package.path().clone(), member));
                }
            }
        }
        for members in backing.values_mut() {
            members.sort();
            members.dedup();
        }
        let definitions = packages
            .iter()
            .map(|(path, package)| (path.clone(), package.ids().collect()))
            .collect();
        Self {
            digest,
            packages,
            requirements,
            backing,
            definitions,
        }
    }

    /// The stored bytes: canonical JSON, packages in path order.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&Stored {
            packages: self.packages.values().cloned().collect(),
            requirements: self.requirements.clone(),
        })
        .unwrap_or_else(|_| unreachable!("a surface serializes: string keys, no floats"))
    }

    /// The snapshot `bytes` spell, which must be the surface of the set
    /// `packages` names.
    ///
    /// # Errors
    ///
    /// [`SnapshotRefusal`].
    pub fn decode(packages: PackageSetDigest, bytes: &[u8]) -> Result<Self, SnapshotRefusal> {
        let stored: Stored =
            serde_json::from_slice(bytes).map_err(|error| SnapshotRefusal::Undecodable {
                packages,
                message: error.to_string(),
            })?;
        let snapshot = Self::assemble(stored.packages, stored.requirements);
        if snapshot.digest != packages {
            return Err(SnapshotRefusal::Digest {
                packages,
                found: snapshot.digest,
            });
        }
        Ok(snapshot)
    }

    /// This surface with each artifact's labels as `labels` answers them by
    /// artifact digest, and none where it answers none: a stored surface
    /// read back with the labels the store keeps beside it.
    #[must_use]
    pub fn with_labels(
        mut self,
        labels: impl Fn(&ArtifactDigest) -> Option<PackageLabels>,
    ) -> Self {
        for package in self.packages.values_mut() {
            package.labels = labels(&package.digest).unwrap_or_default();
        }
        self
    }

    /// The set's digest, computed from its members.
    pub fn digest(&self) -> PackageSetDigest {
        self.digest
    }

    /// What running the set's code requires.
    pub fn requirements(&self) -> &ExecutionRequirements {
        &self.requirements
    }

    /// The set's packages, in path order.
    pub fn packages(&self) -> impl Iterator<Item = &SnapshotPackage> {
        self.packages.values()
    }

    /// The set's package at `path`.
    pub fn package(&self, path: &PackagePath) -> Option<&SnapshotPackage> {
        self.packages.get(path)
    }

    /// Every member of the set's packages `function` backs.
    pub fn members(&self, function: &FunctionId) -> &[(PackagePath, MemberPath)] {
        self.backing.get(function).map_or(&[], Vec::as_slice)
    }

    /// Every identity the set's definitions have.
    pub fn functions(&self) -> BTreeSet<FunctionId> {
        self.definitions.values().flatten().copied().collect()
    }

    /// Every definition of the set's package at `path`, helpers included:
    /// the bodies that may perform its internal operations.
    pub fn package_definitions(&self, path: &PackagePath) -> Option<&BTreeSet<FunctionId>> {
        self.definitions.get(path)
    }

    /// Whether `function` may perform `operation` by this surface: `None`
    /// when the surface does not declare it internal, else whether
    /// `function` is one of its declaring package's definitions.
    pub fn internal_authority(
        &self,
        operation: &lash_kernel_dialect::QualifiedOperation,
        function: FunctionId,
    ) -> Option<bool> {
        let package = self.package(&operation.package)?;
        match package.exports().get(&operation.name)? {
            Export::Operation(export)
                if export.visibility == lash_kernel_dialect::Visibility::Internal =>
            {
                Some(
                    self.package_definitions(&operation.package)
                        .is_some_and(|definitions| definitions.contains(&function)),
                )
            }
            _ => None,
        }
    }
}

/// What running `packages`' code requires: the native of every definition
/// with no body, and an interpreter of every kernel version they are
/// written in.
fn requirements(packages: &[&Package], registry: &FunctionRegistry) -> ExecutionRequirements {
    let mut requirements = ExecutionRequirements::default();
    for package in packages {
        for function in package.code().ids() {
            if let Some(registered) = registry.get(function) {
                requirements.kernels.insert(registered.kernel());
                if registered.states_native() && !registered.has_body() {
                    requirements.natives.insert(*function);
                }
            }
        }
    }
    requirements
}

/// What a node built with `packages` offers running code: the native of
/// every held definition with no body, which its pool binds at start or
/// refuses to start, every kernel version its build interprets, and its own
/// set. The providers and codecs are its host's, which no package names.
pub fn node_capabilities(packages: &crate::HeldPackages) -> NodeCapabilities {
    let mut capabilities = NodeCapabilities {
        kernels: lash_kernel_doc::KernelVersion::ALL
            .iter()
            .map(|version| version.number())
            .collect(),
        surface: Some(packages.digest()),
        ..NodeCapabilities::default()
    };
    // The pool registers each interpreted kernel's redeclarations. Advertise
    // those same identities so a freshly claimed migrated run can use them.
    for (function, registered) in packages.functions().iter() {
        if registered.states_native() && !registered.has_body() {
            capabilities.natives.insert(*function);
        }
    }
    capabilities
}
