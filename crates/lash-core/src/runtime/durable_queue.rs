//! The one implementation of every durable session queue and read operation.
//!
//! These operations are answerable from a session's store and stay correct
//! while another process holds the Session Execution Lease, so they belong to
//! the **Durable Session** authority rather than to a live runtime. The facade
//! builds [`DurableSessionOps`] twice — once from a catalog-acquired store and
//! once from an open session's Session Binding — and the live
//! [`RuntimeHandle`](super::RuntimeHandle) routes its own queue methods here,
//! so there is exactly one body per operation.
//!
//! Observation is best-effort and always runs *after* durable success: a queue
//! mutation that commits is never failed by a publication that does not. The
//! revision stamped on the published event is the committed head read back
//! from the store ([`SessionCommitStore::load_session_head_meta`]), not the
//! session-state encoding marker, so a cursor minted from it reconnects.

use std::sync::Arc;

use crate::SessionId;

use super::observation::{
    LiveReplayEventDraft, LiveReplayStore, SessionObservationEventPayload, SessionQueueEventKind,
    SessionRevision,
};

/// The revision a Durable Session publishes when the store has no committed
/// head yet: a session whose queue is reachable but whose transcript has never
/// been checkpointed observes revision zero, matching the live projection's
/// pre-checkpoint start.
pub const EMPTY_HEAD_REVISION: SessionRevision = SessionRevision(0);

fn store_error(err: impl std::fmt::Display) -> crate::RuntimeError {
    crate::RuntimeError::new(crate::RuntimeErrorCode::StoreCommitFailed, err.to_string())
}

/// The handle owns no store: every operation takes the acquired store so the
/// catalog-acquired handle (which resolves its store lazily through a
/// non-creating seam) and the binding-derived handle (which already holds its
/// owner-issued store) share these bodies without either manufacturing the
/// other's capabilities.
#[derive(Clone)]
pub struct DurableSessionOps {
    session_id: SessionId,
    queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
    live_replay_store: Arc<dyn LiveReplayStore>,
}

impl DurableSessionOps {
    /// Bind the session identity, the queued-work port that receives driver
    /// wakes, and the Live Replay publisher queue events are published through.
    pub fn new(
        session_id: SessionId,
        queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
        live_replay_store: Arc<dyn LiveReplayStore>,
    ) -> Self {
        Self {
            session_id,
            queued_work,
            live_replay_store,
        }
    }

    /// The session these operations are bound to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// A head that cannot be read is not an error for a best-effort
    /// publication; it degrades to [`EMPTY_HEAD_REVISION`], which mints a
    /// cursor a reconnect resolves through gap recovery rather than losing the
    /// event silently.
    async fn publication_revision(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
    ) -> SessionRevision {
        match store.load_session_head_meta().await {
            Ok(Some(meta)) if meta.checkpoint_ref.is_some() => {
                SessionRevision::new(meta.head_revision)
            }
            Ok(_) => EMPTY_HEAD_REVISION,
            Err(err) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %err,
                    "failed to read committed head for a queue observation event; publishing at the empty-head revision",
                );
                EMPTY_HEAD_REVISION
            }
        }
    }

    /// Publish one `QueueChanged` event, best-effort, after durable success.
    async fn publish_queue_changed(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        kind: SessionQueueEventKind,
        batch_ids: Vec<String>,
    ) {
        let revision = self.publication_revision(store).await;
        let drafts = vec![LiveReplayEventDraft::new(
            None::<String>,
            SessionObservationEventPayload::QueueChanged { kind, batch_ids },
        )];
        let result = self
            .live_replay_store
            .prepare_publication(&self.session_id, revision, drafts)
            .and_then(|prepared| {
                self.live_replay_store
                    .publish_prepared(prepared)
                    .map(|_| ())
            });
        if let Err(err) = result {
            tracing::warn!(
                session_id = %self.session_id,
                error = %err,
                "failed to publish queue observation event; reconnect may require gap recovery",
            );
        }
    }

    /// Durably accept host turn input, then wake the queued-work driver.
    ///
    /// Success acknowledges durable acceptance only; the wake is a separate
    /// best-effort signal reconciled from the pending row.
    pub async fn enqueue_turn_input(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
    ) -> Result<crate::PendingTurnInput, crate::RuntimeError> {
        let is_next_turn = matches!(ingress, crate::TurnInputIngress::NextTurn);
        let enqueued = super::session_api::enqueue_turn_input_to_store(
            self.session_id.clone(),
            Arc::clone(store),
            Arc::clone(&self.queued_work),
            input,
            ingress,
            source_key,
        )
        .await?;
        self.publish_queue_changed(
            store,
            SessionQueueEventKind::Enqueued,
            if is_next_turn {
                vec![enqueued.input_id.to_string()]
            } else {
                Vec::new()
            },
        )
        .await;
        Ok(enqueued)
    }

    /// Every open turn input with its factual read-time claim status; a held
    /// input is still reported, held.
    pub async fn pending_turn_inputs(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
    ) -> Result<Vec<crate::PendingTurnInputRead>, crate::RuntimeError> {
        store
            .list_pending_turn_inputs(&self.session_id)
            .await
            .map_err(store_error)
    }

    /// Settled canonical input applications from durable turn commits.
    pub async fn turn_input_applications(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
    ) -> Result<Vec<crate::TurnInputApplication>, crate::RuntimeError> {
        store
            .list_turn_input_applications(&self.session_id)
            .await
            .map_err(store_error)
    }

    /// Pending durable queued-work batches for this session.
    pub async fn queued_work(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::RuntimeError> {
        store
            .list_pending_queued_work(&self.session_id)
            .await
            .map_err(store_error)
    }

    /// Cancel one pending turn input by runtime input id.
    pub async fn cancel_pending_turn_input(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, crate::RuntimeError> {
        let outcome = store
            .cancel_pending_turn_input(&self.session_id, input_id)
            .await
            .map_err(store_error)?;
        if outcome.is_cancelled() {
            self.publish_queue_changed(
                store,
                SessionQueueEventKind::Cancelled,
                vec![input_id.to_string()],
            )
            .await;
        }
        Ok(outcome)
    }

    /// Atomically cancel a selected set of pending inputs.
    pub async fn cancel_pending_turn_inputs(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::RuntimeError> {
        let receipts = store
            .cancel_pending_turn_inputs(&self.session_id, targets)
            .await
            .map_err(store_error)?;
        let cancelled_ids = receipts
            .iter()
            .filter_map(|receipt| match &receipt.outcome {
                crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
                    Some(input.input_id.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if !cancelled_ids.is_empty() {
            self.publish_queue_changed(store, SessionQueueEventKind::Cancelled, cancelled_ids)
                .await;
        }
        Ok(receipts)
    }

    /// Atomically cancel the same-session pending-input suffix from `anchor`.
    pub async fn cancel_pending_turn_input_suffix(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::RuntimeError> {
        let outcome = store
            .cancel_pending_turn_input_suffix(&self.session_id, anchor)
            .await
            .map_err(store_error)?;
        if let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = &outcome {
            let cancelled_ids = outcomes
                .iter()
                .filter_map(|outcome| match outcome {
                    crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
                        Some(input.input_id.to_string())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !cancelled_ids.is_empty() {
                self.publish_queue_changed(store, SessionQueueEventKind::Cancelled, cancelled_ids)
                    .await;
            }
        }
        Ok(outcome)
    }

    /// Cancel one pending queued-work batch.
    pub async fn cancel_queued_work_batch(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::RuntimeError> {
        let batch = store
            .cancel_queued_work_batch(&self.session_id, batch_id)
            .await
            .map_err(store_error)?;
        if batch.is_some() {
            self.publish_queue_changed(
                store,
                SessionQueueEventKind::Cancelled,
                vec![batch_id.to_string()],
            )
            .await;
        }
        Ok(batch)
    }

    /// Release a held queued-work claim without completing it.
    ///
    /// This is token-authorised release, not interference with a live
    /// claimant: the store matches both `claim_id` and `claim_token`, so a
    /// non-holder is refused. It is the lever a host pulls after stopping its
    /// own queued-work driver mid-claim, returning the batches to the pending
    /// queue at once instead of waiting out the claim's lease.
    pub async fn abandon_queued_work_claim(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        claim: &crate::QueuedWorkClaim,
    ) -> Result<(), crate::RuntimeError> {
        store
            .abandon_queued_work_claim(claim)
            .await
            .map_err(store_error)?;
        self.publish_queue_changed(
            store,
            SessionQueueEventKind::Enqueued,
            claim
                .batches
                .iter()
                .map(|batch| batch.batch_id.to_string())
                .collect(),
        )
        .await;
        Ok(())
    }

    /// Release a held pending-turn-input claim without completing it. The
    /// turn-input counterpart of
    /// [`abandon_queued_work_claim`](Self::abandon_queued_work_claim), with the
    /// same `claim_id`/`claim_token` authorisation.
    pub async fn abandon_turn_input_claim(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        claim: &crate::TurnInputClaim,
    ) -> Result<(), crate::RuntimeError> {
        store
            .abandon_turn_input_claim(claim)
            .await
            .map_err(store_error)?;
        self.publish_queue_changed(
            store,
            SessionQueueEventKind::Enqueued,
            claim
                .inputs
                .iter()
                .map(|input| input.input_id.to_string())
                .collect(),
        )
        .await;
        Ok(())
    }

    /// Does this session still have durable live session metadata?
    pub async fn session_exists(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
    ) -> Result<bool, crate::StoreError> {
        Ok(store.load_session_meta().await?.is_some())
    }
}
