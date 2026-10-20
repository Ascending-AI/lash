//! What package code a node holds, and which package surface its cells
//! are written under (FIG-6918, FIG-6991).
//!
//! A host registers its packages once ([`HeldPackages`]): its own set, and
//! the retained sets of earlier installs it still holds. Before its node
//! starts, [`NodeHold`] installs every artifact and set in the store, each
//! set with its surface snapshot and requirements and every definition of
//! it as a stored kind-14 node, keeps each retained artifact a dependent
//! still lists an identity of (RETAIN-SURVEY), and advertises the
//! artifacts it holds, what it offers running code and its own set.
//! Snapshots and the definitions they name are never deleted: an install
//! writes them once and nothing else writes or removes them.
//!
//! [`FleetPackages`] is the fleet gate (FLEET-SET): a cell is written under
//! the first candidate set every live serving node holds, its own set
//! first, then the others it wholly holds, newest first by the store's
//! install sequence. A serving node is one that decodes this build's
//! session set or a session set it carries forward, so a node serving only
//! carried sessions counts. The choice may fall back to an older set. The
//! gate reads the chosen set's surface as any node reads a stored one: a
//! held set's from its own registration, another's from the store.
//! [`promoted`] is the selector it switches to with requirements-based
//! claims (FIG-6993): the newest live serving node's own surface whose
//! requirements every live serving node meets.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use lash_core::durable_port::packages::{ArtifactInstall, SetInstall};
use lash_core::durable_port::{
    DurableError, FormatSet, HeldArtifact, NodeCapabilities, NodeCapability, NodeHolding,
    PackageCatalog, PackageSetDigest, StoreFailure, StoreFailureKind, StoredSet,
};
use lash_kernel_package::{ArtifactKey, HeldPackages, PackageSet};

/// The artifact `key` as the store and a node's advertisement spell it.
fn held_artifact(key: &ArtifactKey) -> HeldArtifact {
    HeldArtifact {
        digest: key.digest,
        path: key.path.to_string(),
    }
}

fn members(set: &PackageSet) -> BTreeSet<HeldArtifact> {
    set.keys().map(|key| held_artifact(&key)).collect()
}

/// The rows a node of `packages` installs before it serves: every artifact
/// it holds, by key with its header, its definitions and the operations it
/// declares internal, and every set they make up, with its surface and
/// requirements.
pub fn install_rows(packages: &HeldPackages) -> (Vec<ArtifactInstall>, Vec<SetInstall>) {
    let artifacts = packages
        .artifacts()
        .iter()
        .map(|(key, package)| ArtifactInstall {
            artifact: held_artifact(key),
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
    let sets = packages
        .sets()
        .iter()
        .filter_map(|(digest, set)| {
            let surface = packages.surface(digest)?;
            Some(SetInstall {
                digest: *digest,
                members: members(set),
                surface: surface.encode(),
                requirements: surface.requirements().clone(),
            })
        })
        .collect();
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

/// Stores every definition of each of `packages`' sets the store does not
/// hold yet as its kind-14 node, under the set's pin: what a node that does
/// not hold a set's artifacts reads its code from. A set's row is written
/// after its definitions, so a stored set's definitions are stored.
async fn hold_definitions(
    backend: &crate::Backend,
    packages: &HeldPackages,
    catalog: &PackageCatalog,
) -> Result<(), DurableError> {
    let documents = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts());
    for (digest, set) in packages.sets() {
        if catalog.sets.iter().any(|stored| stored.digest == *digest) {
            continue;
        }
        let definitions: Vec<_> = set
            .keys()
            .filter_map(|key| packages.artifacts().get(&key))
            .flat_map(|package| package.code().ids().iter().copied())
            .filter_map(|function| {
                let registered = packages.functions().get(&function)?;
                Some((function, registered.image()))
            })
            .collect();
        documents
            .hold_definitions(&surface_pin(digest), &definitions)
            .await
            .map_err(|error| {
                DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: format!("package set {digest}'s definitions are not stored: {error}"),
                })
            })?;
    }
    Ok(())
}

/// The artifacts of `packages`' own set: what a node holding no retained
/// artifact advertises.
pub fn own_artifacts(packages: &HeldPackages) -> BTreeSet<HeldArtifact> {
    packages
        .sets()
        .get(&packages.own())
        .map(members)
        .unwrap_or_default()
}

/// The package code one node holds: the host's one registration and,
/// once the node's start surveyed it, what the node advertises.
pub(crate) struct NodeHold {
    packages: Arc<HeldPackages>,
    advertised: RwLock<Option<BTreeSet<HeldArtifact>>>,
    capabilities: NodeCapabilities,
}

impl NodeHold {
    pub(crate) fn new(packages: Arc<HeldPackages>) -> Self {
        Self {
            capabilities: lash_kernel_package::node_capabilities(&packages),
            packages,
            advertised: RwLock::new(None),
        }
    }

    /// What the node advertises: before its start surveyed it, its own
    /// set's artifacts alone.
    fn held(&self) -> BTreeSet<HeldArtifact> {
        let advertised = self
            .advertised
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        advertised.unwrap_or_else(|| own_artifacts(&self.packages))
    }

    /// The node's candidate sets, in the order the gate tries them: its own
    /// set first, then each other set the store holds that the node holds
    /// wholly, newest first by install sequence.
    fn candidates(
        &self,
        catalog: &PackageCatalog,
    ) -> Vec<(PackageSetDigest, BTreeSet<HeldArtifact>)> {
        let own = self.packages.own();
        let held = self.held();
        let mut candidates: Vec<_> = self
            .packages
            .sets()
            .get(&own)
            .map(|set| (own, members(set)))
            .into_iter()
            .collect();
        let mut stored: Vec<_> = catalog.sets.iter().collect();
        stored.sort_by_key(|set| std::cmp::Reverse(set.sequence));
        for set in stored {
            if set.digest != own
                && self.packages.sets().contains_key(&set.digest)
                && set.members.is_subset(&held)
            {
                candidates.push((set.digest, set.members.clone()));
            }
        }
        candidates
    }

    /// What `backend`'s dependents list of the artifacts this node holds.
    async fn survey(
        &self,
        backend: &crate::Backend,
    ) -> Result<lash_vm_runtime::RetainSurvey, DurableError> {
        let own = own_artifacts(&self.packages);
        let held: BTreeSet<HeldArtifact> = self
            .packages
            .artifacts()
            .keys()
            .map(held_artifact)
            .collect();
        let stored = backend.durable().package_catalog().await?;
        // A dependent lists identities of artifacts some node installed. When
        // the node holds nothing beside its own set and the store holds
        // nothing beside it, nothing is retained and nothing is stranded.
        if held == own
            && stored
                .artifacts
                .iter()
                .all(|artifact| own.contains(&artifact.artifact))
        {
            return Ok(lash_vm_runtime::RetainSurvey::default());
        }
        let mut dependents = lash_vm_runtime::retained_processes(backend)
            .await
            .map_err(survey_failed)?;
        #[cfg(feature = "codemode")]
        dependents.extend(parked_cells(backend).await?);
        Ok(lash_vm_runtime::RetainSurvey::of(
            &self.packages,
            dependents,
        ))
    }
}

fn survey_failed(error: lash_vm_runtime::KernelMigrationSurveyError) -> DurableError {
    match error {
        lash_vm_runtime::KernelMigrationSurveyError::State(error) => error,
        other => DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Unavailable,
            message: format!("the retained-package survey did not finish: {other}"),
        }),
    }
}

/// Every cell of every open turn of `backend`'s sessions, with the
/// identities its document lists: a parked root pins them.
#[cfg(feature = "codemode")]
async fn parked_cells(
    backend: &crate::Backend,
) -> Result<
    Vec<(
        lash_vm_runtime::RetainDependent,
        BTreeSet<lash_kernel_doc::FunctionId>,
    )>,
    DurableError,
> {
    const PAGE: usize = 100;
    let reads = backend.durable();
    let mut dependents = Vec::new();
    for set in backend.formats().session_decodes() {
        let mut after = None;
        loop {
            let page = reads.actors_in(&set, after.as_ref(), PAGE).await?;
            for actor in &page {
                let Ok(session) = lash_core::SessionId::parse(actor.id()) else {
                    continue;
                };
                let Some(turn) = reads.turn(&session).await? else {
                    continue;
                };
                for cell in crate::kernel_migration::stored_cells(
                    &**reads,
                    &session,
                    &turn.run,
                    backend.config().settings().graph_reads,
                )
                .await?
                {
                    let lash_core::durable_port::domain::ExecKey::Cell(_, _, id) = &cell.exec
                    else {
                        continue;
                    };
                    if let Some(listed) = lash_protocol_rlm::cell_snapshot_functions(&cell) {
                        dependents.push((
                            lash_vm_runtime::RetainDependent::ParkedCell {
                                session: session.clone(),
                                cell: id.as_str().to_owned(),
                            },
                            listed,
                        ));
                    }
                }
            }
            if page.len() < PAGE {
                break;
            }
            after = page.last().cloned();
        }
    }
    Ok(dependents)
}

#[async_trait::async_trait]
impl lash_execution_contracts::backend::NodePackages for NodeHold {
    fn candidates(
        &self,
        catalog: &PackageCatalog,
    ) -> Vec<(PackageSetDigest, BTreeSet<HeldArtifact>)> {
        NodeHold::candidates(self, catalog)
    }
    fn surface_view(&self) -> Arc<dyn lash_kernel_dialect::ExportView + Send + Sync> {
        Arc::new(lash_kernel_package::SharedPackageView::declared(
            Arc::clone(&self.packages),
        ))
    }
    fn packages(&self) -> &Arc<HeldPackages> {
        &self.packages
    }

    async fn hold(&self, backend: &crate::Backend) -> Result<NodeHolding, DurableError> {
        let (artifacts, sets) = install_rows(&self.packages);
        let catalog = backend.durable().package_catalog().await?;
        hold_definitions(backend, &self.packages, &catalog).await?;
        backend
            .durable()
            .install_packages(&artifacts, &sets)
            .await?;
        let survey = self.survey(backend).await?;
        if let Some(stranded) = survey.stranded.first() {
            let dependents: BTreeSet<_> = survey
                .stranded
                .iter()
                .filter(|other| other.function == stranded.function)
                .map(|other| &other.dependent)
                .collect();
            return Err(DurableError::RetiredDependents {
                retired: format!("function {}", stranded.function),
                dependents: dependents.len() as u64,
                command: "lashctl packages survey".to_owned(),
            });
        }
        let held: BTreeSet<HeldArtifact> = survey
            .held(&self.packages)
            .iter()
            .map(held_artifact)
            .collect();
        *self
            .advertised
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(held.clone());
        Ok(NodeHolding {
            held,
            capabilities: self.capabilities.clone(),
        })
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

/// FLEET-SET: the first of `candidates` that every node of `serving` holds
/// whole; `None` when there is none.
pub(crate) fn fleet_package_set(
    candidates: &[(PackageSetDigest, BTreeSet<HeldArtifact>)],
    serving: &[&NodeCapability],
) -> Option<PackageSetDigest> {
    candidates
        .iter()
        .find(|(_, members)| serving.iter().all(|node| node.holds(members)))
        .map(|(digest, _)| *digest)
}

/// PROMOTE-BY-CAPABILITY, the selector the fleet gate switches to with
/// requirements-based claims (FIG-6993): of the stored sets that are the own set of
/// `writer`, the node writing the cell, or of some node of `serving`, the
/// newest by install sequence whose requirements `writer` and every node of
/// `serving` meet; `None` when there is none.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "pk-claims (FIG-6993) wires it into the gate")
)]
pub(crate) fn promoted<'a>(
    stored: &'a [StoredSet],
    serving: &[&NodeCapability],
    writer: (&NodeCapabilities, PackageSetDigest),
) -> Option<&'a StoredSet> {
    let (writer, own) = writer;
    stored
        .iter()
        .filter(|set| {
            set.digest == own
                || serving
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

/// The package surface a node of `backend` writes a cell under: the fleet
/// gate. Stored surfaces it does not hold are read once each, with the
/// definitions they name that it lacks: both are immutable.
#[cfg(feature = "codemode")]
pub(crate) struct FleetPackages {
    backend: crate::Backend,
    hold: Arc<dyn lash_execution_contracts::backend::NodePackages>,
    stored: std::sync::Mutex<
        std::collections::BTreeMap<PackageSetDigest, lash_execution_contracts::CellPackages>,
    >,
}

#[cfg(feature = "codemode")]
impl FleetPackages {
    pub(crate) fn new(
        backend: crate::Backend,
        hold: Arc<dyn lash_execution_contracts::backend::NodePackages>,
    ) -> Self {
        Self {
            backend,
            hold,
            stored: std::sync::Mutex::default(),
        }
    }

    /// The surface of `packages`: the node's own registration of it when it
    /// holds the set, else the stored surface over the node's registry and
    /// the stored definitions it lacks.
    async fn surface(
        &self,
        packages: PackageSetDigest,
    ) -> Result<lash_execution_contracts::CellPackages, String> {
        let held = self.hold.packages();
        if let Some(cell) = lash_execution_contracts::CellPackages::held(held, &packages) {
            return Ok(cell);
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

#[cfg(feature = "codemode")]
#[async_trait::async_trait]
impl lash_execution_contracts::PackageGate for FleetPackages {
    async fn writable(
        &self,
    ) -> Result<lash_execution_contracts::CellPackages, lash_core::RuntimeEffectControllerError>
    {
        let durable = self.backend.durable();
        let unavailable = |error: DurableError| {
            tracing::warn!(
                ?error,
                "package fleet survey unavailable; lowering deferred"
            );
            packages_unavailable(format!("package fleet survey unavailable: {error}"))
        };
        let catalog = durable.package_catalog().await.map_err(unavailable)?;
        let live = durable.live_nodes().await.map_err(unavailable)?;
        let formats = self.backend.formats();
        let candidates = self.hold.candidates(&catalog);
        let serving = serving(&live, formats.session(), formats.carried_sessions());
        let chosen = fleet_package_set(&candidates, &serving);
        tracing::info!(
            candidates = ?candidates.iter().map(|(digest, _)| digest.short()).collect::<Vec<_>>(),
            serving = serving.len(),
            chosen = ?chosen.map(|digest| digest.short()),
            "package set fleet gate"
        );
        let chosen = chosen.ok_or_else(|| {
            packages_unavailable("no package set is held by every live node serving these sessions")
        })?;
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
