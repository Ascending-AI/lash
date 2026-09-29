//! Subscription-revision referrers (ADR 0113 §3.4).
//!
//! A trigger command that commits a revision a delivery can start from —
//! `Register`, `Update`, `Revive`, `Enable`, `Disable` — acquires that
//! revision's referrer on the revision's environment and target artifacts
//! before the command commits. The revision id is known up front: its
//! subscription id is deterministic, a new incarnation is
//! [`trigger_incarnation`](super::trigger_incarnation) of the command's
//! operation id, and every other mutation keeps the live row's incarnation at
//! `expected_revision + 1`. The first acquisition arms the revision's
//! `AwaitSubscriptionRevision` guard on the creator's journal, so an edge
//! never exists without a record that will end it, whether or not the command
//! commits. After a commit, the command nudges the revision it superseded.
//!
//! [`RevisionReferrerTriggerStore`] runs this around the store's
//! `execute_command`, inside the command's journaled effect: a replay that
//! reads the recorded result acquires nothing, and a re-execution repeats the
//! acquisitions idempotently.

use std::sync::Arc;

use super::{
    TriggerCommand, TriggerCommandOutcome, TriggerDeliveryReservation,
    TriggerDeliveryRetentionCandidate, TriggerEffectResult, TriggerIngressReceipt,
    TriggerMutationOutcome, TriggerOccurrenceFilter, TriggerOccurrenceReclamationResult,
    TriggerOccurrenceRecord, TriggerOccurrenceRequest, TriggerOwnerScope,
    TriggerRetentionReconciliationReport, TriggerStore, TriggerSubscriptionFilter,
    TriggerSubscriptionRecord, deterministic_subscription_id, trigger_incarnation,
};
use crate::plugin::PluginError;
use crate::{
    ArtifactCleanupPlan, ArtifactName, ArtifactReferrer, ArtifactStoreId, ProcessEngineRegistry,
    ProcessExecutionEnvRef, ProcessId, ProcessInput, ReferrerClaim, SessionId,
    SubscriptionRevisionId,
};

/// A [`TriggerStore`] whose `execute_command` holds the revision a command
/// commits before it commits, and nudges the revision it superseded after.
/// Every other method is the inner store's.
pub struct RevisionReferrerTriggerStore {
    inner: Arc<dyn TriggerStore>,
    engines: ProcessEngineRegistry,
    creator: lash_sansio::EffectJournalIdentity,
}

impl RevisionReferrerTriggerStore {
    /// Wrap `inner` for commands run under the journal `creator`, acquiring
    /// through `engines`' artifact ports.
    pub fn new(
        inner: Arc<dyn TriggerStore>,
        engines: ProcessEngineRegistry,
        creator: lash_sansio::EffectJournalIdentity,
    ) -> Self {
        Self {
            inner,
            engines,
            creator,
        }
    }

    /// The revision `command` commits if it wins its fence, with the
    /// environment and target it will deliver from; `None` for a command
    /// that commits no deliverable revision.
    async fn committed_revision(
        &self,
        operation_id: &str,
        command: &TriggerCommand,
    ) -> Result<Option<PendingRevision>, PluginError> {
        match command {
            TriggerCommand::Register {
                owner_scope, draft, ..
            } => Ok(Some(PendingRevision::new(
                owner_scope,
                &draft.subscription_key,
                trigger_incarnation(owner_scope, operation_id),
                1,
                draft.env_ref.clone(),
                draft.target.clone(),
            )?)),
            TriggerCommand::Revive {
                owner_scope,
                subscription_key,
                draft,
                expected_revision,
                ..
            } => Ok(Some(PendingRevision::new(
                owner_scope,
                subscription_key,
                trigger_incarnation(owner_scope, operation_id),
                next_revision(*expected_revision)?,
                draft.env_ref.clone(),
                draft.target.clone(),
            )?)),
            TriggerCommand::Update {
                owner_scope,
                subscription_key,
                draft,
                expected_revision,
                ..
            } => {
                let Some(live) = self
                    .live_row(owner_scope, subscription_key, *expected_revision)
                    .await?
                else {
                    return Ok(None);
                };
                Ok(Some(PendingRevision::new(
                    owner_scope,
                    subscription_key,
                    live.incarnation,
                    next_revision(*expected_revision)?,
                    draft.env_ref.clone(),
                    draft.target.clone(),
                )?))
            }
            TriggerCommand::Enable {
                owner_scope,
                subscription_key,
                expected_revision,
                ..
            }
            | TriggerCommand::Disable {
                owner_scope,
                subscription_key,
                expected_revision,
                ..
            } => {
                let enable = matches!(command, TriggerCommand::Enable { .. });
                let Some(live) = self
                    .live_row(owner_scope, subscription_key, *expected_revision)
                    .await?
                else {
                    return Ok(None);
                };
                // A command that leaves the lifecycle as it is commits no
                // revision.
                if live.lifecycle.enabled() == enable {
                    return Ok(None);
                }
                Ok(Some(PendingRevision::new(
                    owner_scope,
                    subscription_key,
                    live.incarnation,
                    next_revision(*expected_revision)?,
                    live.env_ref,
                    live.target,
                )?))
            }
            TriggerCommand::List { .. }
            | TriggerCommand::Delete { .. }
            | TriggerCommand::Prune { .. } => Ok(None),
        }
    }

    /// The live row `subscription_key` names under `owner_scope`, when it is
    /// at `expected_revision`. Any other row loses the command's fence, so
    /// the command commits nothing and there is nothing to acquire.
    async fn live_row(
        &self,
        owner_scope: &TriggerOwnerScope,
        subscription_key: &str,
        expected_revision: u64,
    ) -> Result<Option<TriggerSubscriptionRecord>, PluginError> {
        let subscription_id = deterministic_subscription_id(owner_scope, subscription_key);
        let mut filter = TriggerSubscriptionFilter::for_registrant_scope(owner_scope.namespace());
        filter.subscription_key = Some(subscription_key.to_owned());
        Ok(self
            .inner
            .list_subscriptions(filter)
            .await?
            .into_iter()
            .find(|record| {
                record.subscription_id == subscription_id
                    && !record.is_tombstoned()
                    && record.revision == expected_revision
            }))
    }

    async fn acquire(&self, revision: &PendingRevision) -> Result<(), PluginError> {
        let Some(ports) = self.engines.artifact_ports() else {
            return Err(PluginError::Session(format!(
                "trigger revision `{}` names artifacts but the runtime's engine registry has no \
                 artifact stores to hold them",
                revision.referrer
            )));
        };
        let claim = ReferrerClaim::guarded(
            revision.referrer.clone(),
            ArtifactCleanupPlan::AwaitSubscriptionRevision {
                creator: self.creator.clone(),
            },
        )
        .map_err(|error| PluginError::Session(error.to_string()))?;
        let mut names = vec![ArtifactName {
            store: ArtifactStoreId::ProcessEnv,
            artifact_ref: revision.env_ref.as_str().to_owned(),
        }];
        if let ProcessInput::Engine { kind, payload } = &revision.target {
            names.extend(self.engines.require(kind)?.start_artifacts(payload)?);
        }
        // A fence means an earlier execution of this same command committed
        // the revision and it has since ended: the store answers that
        // execution's receipt, and nothing may hold the revision again.
        ports.acquire(&self.engines, &claim, &names).await?;
        Ok(())
    }

    /// Nudge every revision `result` superseded: the one before each
    /// receipt that moved a live row on within its incarnation.
    async fn nudge_superseded(&self, result: &TriggerEffectResult) {
        let Some(ports) = self.engines.artifact_ports() else {
            return;
        };
        let receipts = match result {
            Ok(TriggerCommandOutcome::Mutation { receipt }) => vec![receipt.as_ref()],
            Ok(TriggerCommandOutcome::Prune { receipts }) => receipts.iter().collect(),
            Ok(TriggerCommandOutcome::List { .. }) | Err(_) => Vec::new(),
        };
        for receipt in receipts {
            let moved = matches!(
                receipt.disposition,
                TriggerMutationOutcome::Updated
                    | TriggerMutationOutcome::Enabled
                    | TriggerMutationOutcome::Disabled
                    | TriggerMutationOutcome::Deleted
            );
            if !moved || receipt.revision <= 1 {
                continue;
            }
            match SubscriptionRevisionId::new(
                receipt.subscription_id.clone(),
                receipt.incarnation.clone(),
                receipt.revision - 1,
            ) {
                Ok(superseded) => {
                    ports
                        .nudge(&ArtifactReferrer::SubscriptionRevision(superseded))
                        .await;
                }
                Err(error) => {
                    tracing::warn!(%error, "superseded trigger revision has no referrer id");
                }
            }
        }
    }
}

/// One revision a command will commit if it wins its fence.
struct PendingRevision {
    referrer: ArtifactReferrer,
    env_ref: ProcessExecutionEnvRef,
    target: ProcessInput,
}

impl PendingRevision {
    fn new(
        owner_scope: &TriggerOwnerScope,
        subscription_key: &str,
        incarnation: String,
        revision: u64,
        env_ref: ProcessExecutionEnvRef,
        target: ProcessInput,
    ) -> Result<Self, PluginError> {
        let id = SubscriptionRevisionId::new(
            deterministic_subscription_id(owner_scope, subscription_key),
            incarnation,
            revision,
        )
        .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(Self {
            referrer: ArtifactReferrer::SubscriptionRevision(id),
            env_ref,
            target,
        })
    }
}

fn next_revision(expected_revision: u64) -> Result<u64, PluginError> {
    expected_revision
        .checked_add(1)
        .ok_or_else(|| PluginError::Session("trigger subscription revision overflowed".to_string()))
}

#[async_trait::async_trait]
impl TriggerStore for RevisionReferrerTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<TriggerEffectResult, PluginError> {
        if let Some(revision) = self.committed_revision(operation_id, &command).await? {
            self.acquire(&revision).await?;
        }
        let result = self.inner.execute_command(operation_id, command).await?;
        self.nudge_superseded(&result).await;
        Ok(result)
    }

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError> {
        self.inner.list_subscriptions(filter).await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<TriggerIngressReceipt, PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.inner.list_deliveries().await
    }

    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &ProcessId,
    ) -> Result<(), PluginError> {
        self.inner
            .bind_delivery_process(occurrence_id, subscription_id, process_id)
            .await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<ProcessId>, PluginError> {
        self.inner.list_delivery_process_ids().await
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<TriggerDeliveryRetentionCandidate>, PluginError> {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn list_session_owner_ids_for_retention(&self) -> Result<Vec<SessionId>, PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<TriggerRetentionReconciliationReport, PluginError> {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, PluginError> {
        self.inner
            .prune_non_fired_occurrences(cutoff_epoch_ms)
            .await
    }
}
