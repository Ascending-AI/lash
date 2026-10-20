//! What package code a node runs, and which package surface its cells are
//! written under (FIG-6918, FIG-6991, FIG-6993).
//!
//! A host registers its package set once ([`HeldPackages`]). Before its
//! node starts, [`NodeInstall`] installs every artifact and the set in the
//! store, the set with its surface snapshot and requirements and every
//! definition of it as a stored kind-14 node, and the node advertises
//! what it offers running code: the natives it binds, the kernel versions
//! it interprets and its own set. Snapshots and the definitions they name
//! are never deleted: an install writes them once and nothing else writes
//! or removes them. Work linked under any stored set runs from the store on
//! any node whose capabilities meet the work's hard requirements, so no
//! node retains another build's code.
//!
//! [`FleetPackages`] is the package gate (PROMOTE-BY-CAPABILITY): a cell
//! is written under the newest, by the store's install sequence, of the
//! live serving nodes' own surfaces whose requirements every live serving
//! node meets ([`promoted`]). A serving node is one that decodes this
//! build's session set or a session set it carries forward, so a node
//! serving only carried sessions counts. When no candidate is one every
//! serving node can run, a new cell fails with the typed
//! `NoCompatibleSnapshot` refusal: the gate never answers a partial
//! surface. The gate reads the chosen set's surface as any node reads a
//! stored one: its own from its registration, another's from the store.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::durable_port::packages::{ArtifactInstall, SetInstall};
use lash_core::durable_port::{
    DurableError, FormatSet, NodeCapabilities, NodeCapability, PackageArtifact, PackageCatalog,
    PackageSetDigest, StoreFailure, StoreFailureKind, StoredSet,
};
use lash_kernel_package::{ArtifactKey, HeldPackages, PackageSet};

/// The artifact `key` as the store spells it.
fn package_artifact(key: &ArtifactKey) -> PackageArtifact {
    PackageArtifact {
        digest: key.digest,
        path: key.path.to_string(),
    }
}

fn members(set: &PackageSet) -> BTreeSet<PackageArtifact> {
    set.keys().map(|key| package_artifact(&key)).collect()
}

/// The rows a node of `packages` installs before it serves: every artifact
/// of its set, by key with its header, its definitions and the operations
/// it declares internal, and the set, with its surface and requirements.
pub fn install_rows(packages: &HeldPackages) -> (Vec<ArtifactInstall>, Vec<SetInstall>) {
    let artifacts = packages
        .artifacts()
        .iter()
        .map(|(key, package)| ArtifactInstall {
            artifact: package_artifact(key),
            header: serde_json::to_string(&package.header()).unwrap_or_default(),
            definitions: package.code().ids().iter().copied().collect(),
            internal: package
                .exports()
                .iter()
                .filter_map(|(segment, export)| match export {
                    lash_kernel_package::Export::Operation(operation)
                        if operation.visibility == lash_kernel_package::Visibility::Internal =>
                    {
                        Some(
                            lash_kernel_package::QualifiedOperation {
                                package: package.path().clone(),
                                name: segment.clone(),
                            }
                            .effect_name(),
                        )
                    }
                    _ => None,
                })
                .collect(),
            labels: lash_kernel_package::PackageLabels {
                release: package.release().to_owned(),
                display: package.display().clone(),
            }
            .encode(),
        })
        .collect();
    let set = packages.set();
    let surface = packages.surface();
    let sets = vec![SetInstall {
        digest: set.digest(),
        members: members(set),
        surface: surface.encode(),
        requirements: surface.requirements().clone(),
    }];
    (artifacts, sets)
}

/// The referrer that holds a stored surface's definitions: one pin per set,
/// named by its digest and never released, so the definitions a surface
/// names stay stored as long as the surface does, which is always.
fn surface_pin(packages: &PackageSetDigest) -> lash_core::ReferrerClaim {
    // A pin spells a v4 UUID: the digest's first 16 bytes with the
    // version and variant bits set.
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&packages.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    lash_core::HostArtifactPin::try_from(format!("host-pin:v1:{hex}"))
        .ok()
        .and_then(|pin| {
            lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(pin)).ok()
        })
        .unwrap_or_else(|| unreachable!("a digest's hex is a pin"))
}

/// Stores every definition of `packages`' set as its kind-14 node, under
/// the set's pin, when the store does not hold the set yet: what a node
/// built with another set reads its code from. The set's row is written
/// after its definitions, so a stored set's definitions are stored.
async fn hold_definitions(
    backend: &crate::Backend,
    packages: &HeldPackages,
    catalog: &PackageCatalog,
) -> Result<(), DurableError> {
    let digest = packages.digest();
    if catalog.sets.iter().any(|stored| stored.digest == digest) {
        return Ok(());
    }
    let documents = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts());
    let definitions: Vec<_> = packages
        .artifacts()
        .values()
        .flat_map(|package| package.code().ids().iter().copied())
        .filter_map(|function| {
            let registered = packages.functions().get(&function)?;
            Some((function, registered.image()))
        })
        .collect();
    documents
        .hold_definitions(&surface_pin(&digest), &definitions)
        .await
        .map_err(|error| {
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message: format!("package set {digest}'s definitions are not stored: {error}"),
            })
        })
}

/// The package code one node runs: the host's one registration, which
/// the node installs and advertises before it starts.
pub(crate) struct NodeInstall {
    packages: Arc<HeldPackages>,
    capabilities: NodeCapabilities,
}

impl NodeInstall {
    pub(crate) fn new(packages: Arc<HeldPackages>) -> Self {
        Self {
            capabilities: lash_kernel_package::node_capabilities(&packages),
            packages,
        }
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::backend::NodePackages for NodeInstall {
    fn surface_view(&self) -> Arc<dyn lash_kernel_dialect::ExportView + Send + Sync> {
        Arc::new(lash_kernel_package::SharedPackageView::declared(
            Arc::clone(&self.packages),
        ))
    }
    fn packages(&self) -> &Arc<HeldPackages> {
        &self.packages
    }

    async fn install(&self, backend: &crate::Backend) -> Result<NodeCapabilities, DurableError> {
        let (artifacts, sets) = install_rows(&self.packages);
        let catalog = backend.durable().package_catalog().await?;
        hold_definitions(backend, &self.packages, &catalog).await?;
        backend
            .durable()
            .install_packages(&artifacts, &sets)
            .await?;
        Ok(self.capabilities.clone())
    }
}

/// The live nodes that serve these sessions: each that decodes `session`,
/// this build's session set, or one of `carried`, the session sets it
/// carries forward. A node serving only carried sessions decodes no set of
/// this build and still serves them.
pub(crate) fn serving<'a>(
    live: &'a [NodeCapability],
    session: &FormatSet,
    carried: &[FormatSet],
) -> Vec<&'a NodeCapability> {
    live.iter()
        .filter(|node| {
            node.decodes.contains(session) || carried.iter().any(|set| node.decodes.contains(set))
        })
        .collect()
}

/// PROMOTE-BY-CAPABILITY (FIG-6993): of the stored sets that are the own
/// set of some live node of `serving`, the newest by install sequence
/// whose requirements `writer` and every node of `serving` meet; `None`
/// when there is none.
pub(crate) fn promoted<'a>(
    stored: &'a [StoredSet],
    serving: &[&NodeCapability],
    writer: &NodeCapabilities,
) -> Option<&'a StoredSet> {
    stored
        .iter()
        .filter(|set| {
            serving
                .iter()
                .any(|node| node.capabilities.surface == Some(set.digest))
        })
        .filter(|set| {
            set.requirements.met_by(writer)
                && serving
                    .iter()
                    .all(|node| set.requirements.met_by(&node.capabilities))
        })
        .max_by_key(|set| set.sequence)
}

/// The package surface a node of `backend` writes a cell under: the
/// package gate. Stored surfaces not the node's own are read once each,
/// with the definitions they name that it lacks: both are immutable.
#[cfg(feature = "codemode")]
pub(crate) struct FleetPackages {
    backend: crate::Backend,
    node: Arc<dyn lash_execution_contracts::backend::NodePackages>,
    /// What the node's package code offers: the writer's capabilities the
    /// gate's answer must meet.
    capabilities: NodeCapabilities,
    stored: std::sync::Mutex<
        std::collections::BTreeMap<PackageSetDigest, lash_execution_contracts::CellPackages>,
    >,
}

#[cfg(feature = "codemode")]
impl FleetPackages {
    pub(crate) fn new(
        backend: crate::Backend,
        node: Arc<dyn lash_execution_contracts::backend::NodePackages>,
    ) -> Self {
        Self {
            capabilities: lash_kernel_package::node_capabilities(node.packages()),
            backend,
            node,
            stored: std::sync::Mutex::default(),
        }
    }

    /// The surface of `packages`: the node's own registration when it is
    /// the node's set, else the stored surface over the node's registry and
    /// the stored definitions it lacks.
    async fn surface(
        &self,
        packages: PackageSetDigest,
    ) -> Result<lash_execution_contracts::CellPackages, String> {
        let held = self.node.packages();
        if held.digest() == packages {
            return Ok(lash_execution_contracts::CellPackages::held(held));
        }
        if let Some(cell) = self
            .stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&packages)
        {
            return Ok(cell.clone());
        }
        let bytes = self
            .backend
            .durable()
            .package_surface(&packages)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("package set {packages} has no stored surface"))?;
        let surface = lash_kernel_package::SurfaceSnapshot::decode(packages, &bytes)
            .map_err(|refusal| refusal.to_string())?;
        let artifacts: Vec<_> = surface
            .packages()
            .map(lash_kernel_package::SnapshotPackage::digest)
            .collect();
        let labels = self
            .backend
            .durable()
            .package_labels(&artifacts)
            .await
            .map_err(|error| error.to_string())?;
        // An artifact's newest install's labels: the release it was last
        // installed as.
        let surface = surface.with_labels(|artifact| {
            labels
                .get(artifact)
                .and_then(|labels| labels.last())
                .and_then(|text| lash_kernel_package::PackageLabels::decode(text).ok())
        });
        let missing: Vec<_> = surface
            .functions()
            .into_iter()
            .filter(|function| held.functions().get(function).is_none())
            .collect();
        let stored = lash_vm_runtime::KernelDocuments::new(self.backend.module_artifacts())
            .stored_functions(missing)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|unretained| {
                format!("package set {packages}'s definitions: {unretained:?}")
            })?;
        let catalog = held
            .catalog_with(&stored)
            .map_err(|refusal| refusal.to_string())?;
        let cell =
            lash_execution_contracts::CellPackages::stored(Arc::new(surface), catalog, stored);
        self.stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(packages, cell.clone());
        Ok(cell)
    }
}

#[cfg(feature = "codemode")]
fn packages_unavailable(message: impl Into<String>) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::RunDefinitionUnavailable,
        message,
    )
    .retryable_uncommitted_derivation()
}

/// The candidates the gate considered when it answers none: each live
/// serving node's own surface the store holds, with what
/// some serving node or the writer lacks to run it.
#[cfg(feature = "codemode")]
fn candidates(
    stored: &[StoredSet],
    serving: &[&NodeCapability],
    writer: &NodeCapabilities,
) -> Vec<lash_core_store::runtime_error::SnapshotCandidate> {
    let capabilities = writer;
    stored
        .iter()
        .filter(|set| {
            serving
                .iter()
                .any(|node| node.capabilities.surface == Some(set.digest))
        })
        .map(|set| {
            let mut unmet = set.requirements.unmet_by(capabilities);
            for node in serving {
                let lacking = set.requirements.unmet_by(&node.capabilities);
                unmet.natives.extend(lacking.natives);
                unmet.kernels.extend(lacking.kernels);
            }
            lash_core_store::runtime_error::SnapshotCandidate {
                surface: set.digest.to_string(),
                unmet: unmet
                    .natives
                    .iter()
                    .map(|native| format!("native {native}"))
                    .chain(
                        unmet
                            .kernels
                            .iter()
                            .map(|kernel| format!("kernel {kernel}")),
                    )
                    .collect(),
            }
        })
        .collect()
}

#[cfg(feature = "codemode")]
#[async_trait::async_trait]
impl lash_execution_contracts::PackageGate for FleetPackages {
    async fn writable(
        &self,
    ) -> Result<lash_execution_contracts::CellPackages, lash_core::RuntimeEffectControllerError>
    {
        let durable = self.backend.durable();
        let unavailable = |error: DurableError| {
            tracing::warn!(?error, "package gate read unavailable; lowering deferred");
            packages_unavailable(format!("package gate read unavailable: {error}"))
        };
        let catalog = durable.package_catalog().await.map_err(unavailable)?;
        let live = durable.live_nodes().await.map_err(unavailable)?;
        let formats = self.backend.formats();
        let serving = serving(&live, formats.session(), formats.carried_sessions());
        let chosen = promoted(&catalog.sets, &serving, &self.capabilities);
        tracing::info!(
            serving = serving.len(),
            chosen = ?chosen.map(|set| set.digest.short()),
            "package gate"
        );
        let Some(chosen) = chosen.map(|set| set.digest) else {
            return Err(
                lash_core::RuntimeEffectControllerError::no_compatible_snapshot(candidates(
                    &catalog.sets,
                    &serving,
                    &self.capabilities,
                )),
            );
        };
        self.surface(chosen).await.map_err(|message| {
            tracing::warn!(%message, "stored package surface unavailable; lowering deferred");
            packages_unavailable(format!(
                "the stored package surface {chosen} is unavailable: {message}"
            ))
        })
    }
}

#[cfg(test)]
mod tests;
