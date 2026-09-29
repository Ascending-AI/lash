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

/// The revision a committed head mints for a queue observation event: the
/// head's own revision when a checkpoint backs it, else the empty head.
fn revision_of_head(meta: Option<crate::store::SessionHeadMeta>) -> SessionRevision {
    match meta {
        Some(meta) if meta.checkpoint_ref.is_some() => SessionRevision::new(meta.head_revision),
        _ => EMPTY_HEAD_REVISION,
    }
}

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
    ingress: super::drive::IngressRelay,
    live_replay_store: Arc<dyn LiveReplayStore>,
}

impl DurableSessionOps {
    /// Bind the session identity, the ingress relay that delivers what an
    /// acceptance admits (ADR 0109 §3), and the Live Replay publisher queue
    /// events are published through.
    pub fn new(
        session_id: SessionId,
        ingress: super::drive::IngressRelay,
        live_replay_store: Arc<dyn LiveReplayStore>,
    ) -> Self {
        Self {
            session_id,
            ingress,
            live_replay_store,
        }
    }

    /// The session these operations are bound to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The ingress obligation of the accepted input or batch `item_id`, when
    /// its delivery to the engine stalled (ADR 0109 §3).
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn stalled_ingress(
        &self,
        item_id: &str,
    ) -> Result<Option<crate::store::StalledObligation>, crate::StoreError> {
        self.ingress.stalled(item_id).await
    }

    /// The drive the relay's current claim of item `item_id` asked for, while
    /// one is outstanding ([`IngressRelay::current_ask`](crate::runtime::drive::IngressRelay::current_ask)).
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn current_ingress_ask(
        &self,
        item_id: &str,
    ) -> Result<Option<crate::engine::DriveRequestId>, crate::StoreError> {
        self.ingress.current_ask(item_id).await
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
            Ok(meta) => revision_of_head(meta),
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
    /// `revision`, when the admission already read it (FIG-3975), is stamped
    /// directly instead of costing the head read again.
    async fn publish_queue_changed(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        kind: SessionQueueEventKind,
        batch_ids: Vec<String>,
        revision: Option<SessionRevision>,
    ) {
        let revision = match revision {
            Some(revision) => revision,
            None => self.publication_revision(store).await,
        };
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

    /// Durably accept host turn input under `run_spec`, then deliver the
    /// drive its admission owes (ADR 0109 §3).
    ///
    /// Success acknowledges durable acceptance only; a delivery that fails is
    /// retried by the ingress relay from the row's obligation.
    pub async fn enqueue_turn_input(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
        run_spec: crate::RunSpec,
    ) -> Result<crate::PendingTurnInput, crate::RuntimeError> {
        self.enqueue_turn_inputs(store, vec![(input, source_key)], ingress, run_spec)
            .await?
            .pop()
            .ok_or_else(|| store_error("a batch of one admitted no pending turn input"))
    }

    /// Durably accept `inputs`, each filed under its source key, as one
    /// request under one shared `ingress` and `run_spec` (FIG-3842), then
    /// deliver the drive each admission owes (ADR 0109 §3).
    ///
    /// The rows come back in request order. An input a stored row already
    /// answers returns that row; the others are enqueued in request order as
    /// one contiguous block. A conflict, or one id named twice, refuses the
    /// whole request and accepts nothing.
    pub async fn enqueue_turn_inputs(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        inputs: Vec<(crate::TurnInput, Option<String>)>,
        ingress: crate::TurnInputIngress,
        run_spec: crate::RunSpec,
    ) -> Result<Vec<crate::PendingTurnInput>, crate::RuntimeError> {
        let is_next_turn = matches!(ingress, crate::TurnInputIngress::NextTurn);
        let (enqueued, revision) = enqueue_turn_inputs_to_store(
            self.session_id.clone(),
            Arc::clone(store),
            &self.ingress,
            inputs,
            ingress,
            run_spec,
        )
        .await?;
        self.publish_queue_changed(
            store,
            SessionQueueEventKind::Enqueued,
            if is_next_turn {
                enqueued
                    .iter()
                    .map(|row| row.input_id.to_string())
                    .collect()
            } else {
                Vec::new()
            },
            revision,
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
            .list_open_queued_work(&self.session_id)
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
                None,
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
            self.publish_queue_changed(
                store,
                SessionQueueEventKind::Cancelled,
                cancelled_ids,
                None,
            )
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
                self.publish_queue_changed(
                    store,
                    SessionQueueEventKind::Cancelled,
                    cancelled_ids,
                    None,
                )
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
                None,
            )
            .await;
        }
        Ok(batch)
    }

    /// Does this session still have durable live session metadata?
    pub async fn session_exists(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
    ) -> Result<bool, crate::StoreError> {
        Ok(store.load_session_meta().await?.is_some())
    }
}

pub(in crate::runtime) async fn enqueue_turn_input_to_store(
    session_id: SessionId,
    store: Arc<dyn crate::RuntimePersistence>,
    ingress_relay: &super::drive::IngressRelay,
    input: crate::TurnInput,
    ingress: crate::TurnInputIngress,
    source_key: Option<String>,
    run_spec: crate::RunSpec,
) -> Result<crate::PendingTurnInput, crate::RuntimeError> {
    enqueue_turn_inputs_to_store(
        session_id,
        store,
        ingress_relay,
        vec![(input, source_key)],
        ingress,
        run_spec,
    )
    .await?
    .0
    .pop()
    .ok_or_else(|| {
        crate::RuntimeError::new(
            crate::RuntimeErrorCode::StoreCommitFailed,
            "a batch of one admitted no pending turn input",
        )
    })
}

/// Durably accept `inputs`, each filed under its source key, as one request
/// under one shared `ingress` and `run_spec` (FIG-3842), then deliver the
/// drive each admitted row's ingress obligation owes (ADR 0109 §3). The rows
/// come back in request order; a refusal accepted nothing.
///
/// The admission is the store's whole round when the backend folds it
/// (FIG-3975): it answers [`crate::TurnInputAdmission`] with the claims its commit
/// already took, so the only post-commit operation is the drive ask itself.
/// A backend that does not fold answers `Enqueued`; the relay then takes
/// each row's claim as before. The revision a fused admission read rides
/// back so the caller's queue event does not read the head again.
pub(in crate::runtime) async fn enqueue_turn_inputs_to_store(
    session_id: SessionId,
    store: Arc<dyn crate::RuntimePersistence>,
    ingress_relay: &super::drive::IngressRelay,
    inputs: Vec<(crate::TurnInput, Option<String>)>,
    ingress: crate::TurnInputIngress,
    run_spec: crate::RunSpec,
) -> Result<(Vec<crate::PendingTurnInput>, Option<SessionRevision>), crate::RuntimeError> {
    let mut drafts = Vec::with_capacity(inputs.len());
    for (input, source_key) in inputs {
        let mut draft =
            crate::PendingTurnInputDraft::new(session_id.clone(), ingress.clone(), input)
                .with_run_spec(run_spec.clone());
        // A keyed input's id is its key's: a host re-attaches by the key alone.
        if let Some(key) = source_key.as_deref() {
            draft.input_id = Some(crate::PendingTurnInputDraft::keyed_input_id(
                &draft.session_id,
                key,
            ));
        }
        draft.source_key = source_key;
        drafts.push(draft);
    }
    let batch = crate::PendingTurnInputBatch::new(session_id, drafts)
        .map_err(super::error::runtime_error_from_turn_input_admission)?;
    let admission = store
        .admit_pending_turn_inputs(batch, ingress_relay.claim_ttl_ms())
        .await
        .map_err(super::error::runtime_error_from_turn_input_admission)?;
    // Each admission armed its row's ingress obligation; deliver them now
    // (ADR 0109 §3). An attempt that fails is the relay's to retry: the
    // inputs are accepted either way. A row a resend answered is delivered
    // only if its obligation is still due — a fused admission's claim list
    // holds exactly those rows.
    let (enqueued, revision) = match admission {
        crate::TurnInputAdmission::Fused {
            rows,
            ingress_claims,
            committed_head,
        } => {
            for claimed in ingress_claims {
                ingress_relay.deliver_claimed(claimed).await;
            }
            (rows, Some(revision_of_head(committed_head)))
        }
        crate::TurnInputAdmission::Enqueued(rows) => {
            for row in &rows {
                ingress_relay.deliver_admitted(row.input_id.as_str()).await;
            }
            (rows, None)
        }
    };
    Ok((enqueued, revision))
}
