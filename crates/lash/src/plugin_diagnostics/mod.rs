//! Host projections over frozen registrations and retained records.
use crate::plugins::{ContributionKey, PluginHost};
use lash_core::RuntimeOwner;
use std::collections::BTreeSet;
use std::sync::Arc;
mod attribution;
mod claim;
mod types;
pub(crate) use lash_core::plugin::prompt::read_prompt_plan;
pub use types::*;

#[derive(Clone)]
pub struct PluginDiagnostics {
    pub(crate) core: crate::LashCore,
}

pub(crate) fn read_prompt_catalog(
    host: &PluginHost,
) -> Result<PromptCatalogSnapshot, crate::plugins::PluginError> {
    host.prompt_catalog_snapshot()
}
pub(crate) async fn read_prompt_snapshot(
    backend: &lash_execution_contracts::backend::Backend,
    call: &PromptCallKey,
) -> Result<Option<LoadedPromptSnapshot>, crate::prompt::AdmittedCallLoadError> {
    lash_core::plugin::prompt::load_prompt_snapshot(backend.durable().as_ref(), call).await
}

impl PluginDiagnostics {
    /// This core's own registered boot, observed now. Before registration the
    /// owner is absent. Store validation also detects a stopped/replaced boot.
    pub fn local_node(&self) -> NodeServingSnapshot {
        let lease = self.core.node.registration.snapshot();
        NodeServingSnapshot {
            owner: lease.as_ref().map(|lease| lease.owner.clone()),
            observed_at: DurableInstant(
                i64::try_from(self.core.env.core.clock.timestamp_ms()).unwrap_or(i64::MAX),
            ),
            decodes: lease
                .as_ref()
                .map_or_else(Vec::new, |lease| lease.decodes.clone()),
            serves: lease.map_or_else(BTreeSet::new, |lease| lease.serves.into_iter().collect()),
            // One inventory has one registry scope; installed() supplies each
            // scope separately rather than mislabeling a node-wide union.
            inventory: None,
        }
    }
    /// Pure declared inventory or an already materialized owner's snapshot.
    /// Weak observations never retain or construct an owner.
    pub fn installed(&self, scope: &InventoryScope) -> InstalledObservation {
        let host = self.core.env.plugin_host.as_ref();
        match scope {
            InventoryScope::Deployment(crate::plugins::RegistryKind::Session) => host
                .and_then(|host| host.installed_inventory().ok())
                .map_or(
                    InstalledObservation::Unavailable {
                        reason: EvidenceAbsence::OwnerEvidenceUnavailable,
                    },
                    InstalledObservation::Present,
                ),
            InventoryScope::Deployment(crate::plugins::RegistryKind::Process) => {
                InstalledObservation::Present(
                    self.core
                        .env
                        .core
                        .process_engines
                        .components()
                        .installed_inventory(),
                )
            }
            InventoryScope::Owner(owner @ RuntimeOwner::Process(_)) => {
                let registry = self.core.env.core.process_engines.components();
                registry.materialized_inventories().get(owner).map_or_else(
                    || InstalledObservation::NotMaterialized {
                        declared: registry.installed_inventory().entries,
                    },
                    InstalledObservation::Present,
                )
            }
            InventoryScope::Owner(owner) => {
                let mut hosts = self
                    .core
                    .diagnostic_hosts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                hosts.retain(|host| host.upgrade().is_some());
                for observed in hosts
                    .iter()
                    .rev()
                    .filter_map(crate::plugins::WeakMaterializedInventoryReader::upgrade)
                {
                    if let Some(inventory) = observed.get(owner) {
                        return InstalledObservation::Present(inventory);
                    }
                }
                match owner {
                    RuntimeOwner::Session(_) => {
                        match host.and_then(|host| host.installed_inventory().ok()) {
                            Some(inventory) => InstalledObservation::NotMaterialized {
                                declared: inventory.entries,
                            },
                            None => InstalledObservation::Unavailable {
                                reason: EvidenceAbsence::OwnerEvidenceUnavailable,
                            },
                        }
                    }
                    RuntimeOwner::Process(_) => InstalledObservation::Unavailable {
                        reason: EvidenceAbsence::OwnerEvidenceUnavailable,
                    },
                }
            }
        }
    }
    pub async fn explain_claimability(
        &self,
        node: Option<&NodeServingSnapshot>,
        session: &crate::SessionId,
    ) -> Result<ClaimExplanation, DiagnosticError> {
        self.explain_actor_claimability(node, &ActorKey::session(session.as_str())?)
            .await
    }
    /// The process arm uses the same store predicates, including its broader
    /// cancel exception. No claim or activation is attempted.
    pub async fn explain_actor_claimability(
        &self,
        node: Option<&NodeServingSnapshot>,
        actor: &ActorKey,
    ) -> Result<ClaimExplanation, DiagnosticError> {
        let facts = self
            .core
            .backend
            .durable()
            .claim_facts(node.and_then(|node| node.owner.as_ref()), actor)
            .await?;
        Ok(claim::explain(node, actor.clone(), facts))
    }
    /// Retained plan; no session activation or writer access.
    pub async fn prompt_plan(
        &self,
        session: &crate::SessionId,
    ) -> Result<Option<crate::prompt::PromptPlan>, DiagnosticError> {
        let store = lash_core::store::SessionStore::new(
            Arc::clone(&self.core.store_factory) as Arc<dyn lash_core::store::RuntimeStore>,
            session.clone(),
        )?;
        Ok(read_prompt_plan(&store).await?)
    }
    pub async fn prompt_snapshot(
        &self,
        call: &PromptCallKey,
    ) -> Result<Option<LoadedPromptSnapshot>, DiagnosticError> {
        Ok(read_prompt_snapshot(&self.core.backend, call).await?)
    }
    /// Optional detailed evidence is paged separately from scheduling truth.
    /// A changed actor revision invalidates the projection, without retrying.
    pub async fn obligation_evidence(
        &self,
        session: &crate::SessionId,
        expected_revision: StateRevision,
        after: usize,
        limit: std::num::NonZeroUsize,
    ) -> Result<ObligationEvidencePage, DiagnosticError> {
        use lash_core::RecordedWork;
        let reads = self.core.backend.durable();
        let actor = ActorKey::session(session.as_str())?;
        let before = reads.claim_facts(None, &actor).await?;
        let Some(row) = before.actor.filter(|row| row.revision == expected_revision) else {
            return Ok(ObligationEvidencePage {
                evidence: Evidence::Unavailable {
                    reason: EvidenceAbsence::ObservationChanged,
                },
                next: None,
                mismatch: None,
                observed_revision: None,
            });
        };
        let turn = reads.turn(session).await?;
        let heads = self.core.backend.session_store_factory();
        let work = match RecordedWork::read_session_within(
            reads.as_ref(),
            heads.as_ref(),
            session,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(16 * 1024 * 1024),
            4096,
        )
        .await
        {
            Ok(work) => work,
            Err(lash_core::durable_port::DurableError::Graph(
                lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit { .. }
                | lash_core_store::checkpoint_graph::GraphRefusal::RootTooLarge { .. },
            )) => {
                return Ok(ObligationEvidencePage {
                    evidence: Evidence::Unavailable {
                        reason: EvidenceAbsence::ReadBoundExceeded,
                    },
                    next: None,
                    mismatch: None,
                    observed_revision: Some(row.revision),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let current = reads.claim_facts(None, &actor).await?.actor;
        if !current
            .is_some_and(|current| current.revision == row.revision && current.epoch == row.epoch)
        {
            return Ok(ObligationEvidencePage {
                evidence: Evidence::Unavailable {
                    reason: EvidenceAbsence::ObservationChanged,
                },
                next: None,
                mismatch: None,
                observed_revision: Some(row.revision),
            });
        }
        let uses = work.outstanding_obligations();
        let end = after.saturating_add(limit.get().min(256)).min(uses.len());
        let page = uses.get(after..end).unwrap_or_default().to_vec();
        let mismatch = work.mismatch(&row.requires);
        // A session with no turn owes only what its head records.
        let source = match turn {
            Some(turn) => RecordLocator::TurnPhase {
                session: session.clone(),
                run: turn.run,
            },
            None => RecordLocator::SessionHead {
                session: session.clone(),
            },
        };
        let evidence = if mismatch.is_some() {
            Evidence::Partial {
                source,
                value: page,
                missing: vec![EvidenceAbsence::StoredRequiresMismatch],
            }
        } else {
            Evidence::Recorded {
                source,
                value: page,
            }
        };
        Ok(ObligationEvidencePage {
            evidence,
            next: (end < uses.len()).then_some(end),
            mismatch,
            observed_revision: Some(row.revision),
        })
    }
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static CELL_REPORT_DECODES: Arc<std::sync::atomic::AtomicUsize>;
}
