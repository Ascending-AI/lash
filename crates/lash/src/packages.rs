//! What package code a node holds, and which package set its cells are
//! written under (FIG-6918).
//!
//! A host registers its packages once ([`HeldPackages`]): its own set, and
//! the retained sets of earlier installs it still holds. Before its node
//! starts, [`NodeHold`] installs every artifact and set in the store, keeps
//! each retained artifact a dependent still lists an identity of
//! (RETAIN-SURVEY), and advertises the artifacts it holds: its own set's
//! and the retained ones. "Node N holds set S" is derived from that, S's
//! members being among N's, and no list of sets is stored.
//!
//! [`FleetPackages`] is the fleet gate (FLEET-SET): a cell is written under
//! the first candidate set every live serving node holds, its own set
//! first, then the others it wholly holds, newest first by the store's
//! install sequence. A serving node is one that decodes this build's
//! session set or a session set it carries forward, so a node serving only
//! carried sessions counts. The choice may fall back to an older set.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use lash_core::durable_port::packages::{ArtifactInstall, SetInstall};
use lash_core::durable_port::{
    DurableError, FormatSet, HeldArtifact, NodeCapability, PackageCatalog, PackageSetDigest,
    StoreFailure, StoreFailureKind,
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
/// it holds, by key with its header, and every set they make up.
pub fn install_rows(packages: &HeldPackages) -> (Vec<ArtifactInstall>, Vec<SetInstall>) {
    let artifacts = packages
        .artifacts()
        .iter()
        .map(|(key, package)| ArtifactInstall {
            artifact: held_artifact(key),
            header: serde_json::to_string(&package.header()).unwrap_or_default(),
        })
        .collect();
    let sets = packages
        .sets()
        .iter()
        .map(|(digest, set)| SetInstall {
            digest: *digest,
            members: members(set),
        })
        .collect();
    (artifacts, sets)
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
}

impl NodeHold {
    pub(crate) fn new(packages: Arc<HeldPackages>) -> Self {
        Self {
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

    async fn hold(&self, backend: &crate::Backend) -> Result<BTreeSet<HeldArtifact>, DurableError> {
        let (artifacts, sets) = install_rows(&self.packages);
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
        Ok(held)
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

/// The package set a node of `backend` writes a cell under: the fleet gate.
#[cfg(feature = "codemode")]
pub(crate) struct FleetPackages {
    backend: crate::Backend,
    hold: Arc<dyn lash_execution_contracts::backend::NodePackages>,
}

#[cfg(feature = "codemode")]
impl FleetPackages {
    pub(crate) fn new(
        backend: crate::Backend,
        hold: Arc<dyn lash_execution_contracts::backend::NodePackages>,
    ) -> Self {
        Self { backend, hold }
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
        chosen
            .map(lash_execution_contracts::CellPackages)
            .ok_or_else(|| {
                packages_unavailable(
                    "no package set is held by every live node serving these sessions",
                )
            })
    }
}

#[cfg(test)]
mod tests;
