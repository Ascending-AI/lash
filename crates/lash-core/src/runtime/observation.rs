use lash_sansio::sync::MutexExt;
pub(crate) mod replay;

use crate::facade_support::ToolStateFacadeOps;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{LashRuntime, ProcessHandleView, ProcessRecord, ProcessRegistry};

pub use replay::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayEventDraft, LiveReplayGap,
    LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, LiveReplaySubscription, PreparedLiveReplayPublication,
    SessionCursor, SessionCursorError, SessionObservation, SessionObservationEvent,
    SessionObservationEventPayload, SessionObservationSubscription, SessionProcessEventKind,
    SessionQueueEventKind, SessionResume, SessionRevision,
};

#[derive(Clone)]
pub struct RuntimeObservation {
    pub session_id: Arc<str>,
    pub revision: SessionRevision,
    pub cursor: SessionCursor,
    pub policy: crate::SessionPolicy,
    pub read_view: crate::SessionReadView,
    pub persisted_state: super::RuntimeSessionState,
    pub usage_report: super::SessionUsageReport,
    pub tool_state: Option<crate::ToolState>,
    pub tool_catalog: Arc<Vec<serde_json::Value>>,
    pub tool_catalog_error: Option<String>,
    pub plugin_session: Option<Arc<crate::PluginSession>>,
    pub session_read_service: Option<Arc<dyn crate::plugin::SessionReadService>>,
    pub process_read_service: Option<Arc<dyn crate::plugin::ProcessReadService>>,
    pub process_registry: Option<Arc<dyn ProcessRegistry>>,
    pub queue_store: Option<Arc<dyn crate::RuntimePersistence>>,
    pub queued_work_driver: Option<super::QueuedWorkDriver>,
}

impl RuntimeObservation {
    fn from_runtime(
        runtime: &LashRuntime,
        cursor: SessionCursor,
        previous: Option<&RuntimeObservation>,
        revision: SessionRevision,
        read_view: crate::SessionReadView,
        persisted_state: super::RuntimeSessionState,
        usage_report: super::SessionUsageReport,
    ) -> Self {
        let (tool_catalog, tool_catalog_error) = match runtime.active_tool_catalog_shared() {
            Ok(catalog) => (catalog, None),
            Err(err) => (Arc::new(Vec::new()), Some(err.to_string())),
        };
        let tool_state_generation = matches!(
            runtime.resident_session_state,
            super::ResidentSessionState::Valid
        )
        .then(|| {
            runtime
                .session
                .as_ref()
                .map(|session| session.plugins().tool_registry().generation())
        })
        .flatten();
        let tool_state = match (
            tool_state_generation,
            previous.and_then(|observation| observation.tool_state.as_ref()),
        ) {
            (Some(generation), Some(snapshot)) if snapshot.generation() == generation => {
                Some(snapshot.clone())
            }
            (Some(_), _) => match runtime.tool_state() {
                Ok(state) => Some(state),
                Err(err) => {
                    tracing::warn!(
                        session_id = %runtime.session_id(),
                        error = %err,
                        "failed to capture tool state for observation; omitting the snapshot",
                    );
                    None
                }
            },
            (None, _) => None,
        };
        let (plugin_session, session_read_service, process_read_service) =
            match (runtime.session.as_ref(), runtime.runtime_session_services()) {
                (Some(session), Ok(services)) => (
                    Some(Arc::clone(session.plugins())),
                    Some(services.read_service()),
                    Some(services.process_read_service()),
                ),
                (_, Err(err)) => {
                    tracing::warn!(
                        session_id = %runtime.session_id(),
                        error = %err,
                        "failed to capture plugin query services for observation",
                    );
                    (None, None, None)
                }
                (None, _) => (None, None, None),
            };
        Self {
            session_id: Arc::from(runtime.session_id()),
            revision,
            cursor,
            policy: read_view.policy().clone(),
            read_view,
            persisted_state,
            usage_report,
            tool_state,
            tool_catalog,
            tool_catalog_error,
            plugin_session,
            session_read_service,
            process_read_service,
            process_registry: runtime.host.process_registry.clone(),
            queue_store: runtime
                .session
                .as_ref()
                .and_then(|session| session.history_store()),
            queued_work_driver: runtime.host.queued_work_driver.clone(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn session_revision(&self) -> SessionRevision {
        self.revision
    }

    pub fn cursor(&self) -> &SessionCursor {
        &self.cursor
    }

    pub fn session_observation(&self) -> SessionObservation {
        SessionObservation {
            read_view: self.read_view.clone(),
            cursor: self.cursor.clone(),
        }
    }

    pub fn process_scope(&self) -> crate::SessionScope {
        crate::SessionScope::new(self.session_id.as_ref())
    }

    pub fn process_scope_id(&self) -> crate::SessionScopeId {
        self.process_scope().id()
    }

    pub async fn query_plugin(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<String>,
    ) -> Result<(String, serde_json::Value), crate::PluginOperationInvokeError> {
        let Some(plugin_session) = self.plugin_session.as_ref().cloned() else {
            return Err(crate::PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        let Some(session_read_service) = self.session_read_service.as_ref().cloned() else {
            return Err(crate::PluginOperationInvokeError::Unknown(
                "runtime session read service not available".to_string(),
            ));
        };
        let Some(process_read_service) = self.process_read_service.as_ref().cloned() else {
            return Err(crate::PluginOperationInvokeError::Unknown(
                "runtime process read service not available".to_string(),
            ));
        };
        plugin_session
            .query_plugin(
                name,
                args,
                session_id,
                true,
                session_read_service,
                process_read_service,
            )
            .await
    }

    pub async fn list_process_handles(&self) -> Vec<ProcessHandleView> {
        let Some(executor) = self.process_registry.as_ref() else {
            return Vec::new();
        };
        self.list_process_handles_with_mode(executor, crate::ProcessListMode::Live)
            .await
    }

    pub async fn list_all_process_handles(&self) -> Vec<ProcessHandleView> {
        let Some(executor) = self.process_registry.as_ref() else {
            return Vec::new();
        };
        self.list_process_handles_with_mode(executor, crate::ProcessListMode::All)
            .await
    }

    async fn list_process_handles_with_mode(
        &self,
        executor: &Arc<dyn crate::ProcessRegistry>,
        mode: crate::ProcessListMode,
    ) -> Vec<ProcessHandleView> {
        let root_scope = self.process_scope();
        let mut entries = list_scope_process_handles(executor, &root_scope, mode).await;
        if let Some(agent_frame_id) = self.persisted_state.current_frame_node_id.as_ref() {
            let frame_scope = crate::SessionScope::for_agent_frame(
                self.session_id.as_ref(),
                agent_frame_id.clone(),
            );
            if frame_scope.id() != root_scope.id() {
                entries.extend(list_scope_process_handles(executor, &frame_scope, mode).await);
                entries.sort_by(|left, right| left.id.cmp(&right.id));
                entries.dedup_by(|left, right| left.id == right.id);
            }
        }
        entries
            .into_iter()
            .map(ProcessHandleView::from_record)
            .collect()
    }
}

fn export_observation_state(
    runtime: &LashRuntime,
) -> (
    super::RuntimeSessionState,
    crate::SessionReadView,
    super::SessionUsageReport,
) {
    // Observation publication is synchronous. When resident state has been
    // invalidated, project only the already-adopted durable snapshot; never
    // recapture live plugin/tool state before the async reload gate runs.
    let mut state = runtime.export_persistence_state();
    let read_view = runtime.read_view();
    let shared_ledger = runtime.shared_token_ledger.lock_recover();
    let mut saturated = false;
    for entry in shared_ledger.iter().cloned() {
        saturated |= super::merge_ledger_entry_saturating(&mut state.token_ledger, entry.entry);
    }
    let usage_report =
        super::SessionUsageReport::from_entries_with_saturation(&state.token_ledger, saturated);
    (state, read_view, usage_report)
}

async fn list_scope_process_handles(
    executor: &Arc<dyn crate::ProcessRegistry>,
    scope: &crate::SessionScope,
    mode: crate::ProcessListMode,
) -> Vec<ProcessRecord> {
    match mode {
        crate::ProcessListMode::Live => executor.list_live_observed_by(&scope.session_id).await,
        crate::ProcessListMode::All => executor.list_observed_by(&scope.session_id).await,
    }
    .unwrap_or_default()
}

#[derive(Clone)]
pub struct RuntimeHandle {
    pub(in crate::runtime) runtime: Arc<Mutex<LashRuntime>>,
    observation: Arc<ArcSwap<RuntimeObservation>>,
    live_replay_store: Arc<dyn LiveReplayStore>,
}

impl RuntimeHandle {
    pub fn new(runtime: LashRuntime) -> Self {
        Self::with_live_replay_store(runtime, Arc::new(InMemoryLiveReplayStore::default()))
    }

    pub fn with_live_replay_store(
        runtime: LashRuntime,
        live_replay_store: Arc<dyn LiveReplayStore>,
    ) -> Self {
        let revision = SessionRevision::from_runtime(&runtime);
        let cursor = live_replay_store.current_cursor(runtime.session_id(), revision);
        let (state, read_view, usage_report) = export_observation_state(&runtime);
        let observation = RuntimeObservation::from_runtime(
            &runtime,
            cursor,
            None,
            revision,
            read_view,
            state,
            usage_report,
        );
        Self {
            runtime: Arc::new(Mutex::new(runtime)),
            observation: Arc::new(ArcSwap::from_pointee(observation)),
            live_replay_store,
        }
    }

    pub fn writer(&self) -> Arc<Mutex<LashRuntime>> {
        Arc::clone(&self.runtime)
    }

    pub fn observe(&self) -> Arc<RuntimeObservation> {
        self.observation.load_full()
    }

    pub fn publish_from(&self, runtime: &LashRuntime) {
        self.publish_from_inner(runtime, false);
    }

    /// Publish a revision-stable authoritative resident change that is not
    /// represented in the serializable session projection.
    pub fn publish_resident_from(&self, runtime: &LashRuntime) {
        self.publish_from_inner(runtime, true);
    }

    fn publish_from_inner(&self, runtime: &LashRuntime, force_resident: bool) {
        let revision = SessionRevision::from_runtime(runtime);
        let previous = self.observation.load_full();
        let turn_id = (previous.revision != revision)
            .then(|| runtime.last_committed_turn_id_for_revision(revision))
            .flatten();
        let (state, read_view, usage_report) = export_observation_state(runtime);
        let mut next = RuntimeObservation::from_runtime(
            runtime,
            previous.cursor.clone(),
            Some(previous.as_ref()),
            revision,
            read_view.clone(),
            state,
            usage_report,
        );
        let payload = if previous.revision < revision {
            Some(SessionObservationEventPayload::Committed {
                read_view: read_view.clone(),
            })
        } else if force_resident
            || authority_fingerprint(&previous.persisted_state)
                != authority_fingerprint(&next.persisted_state)
        {
            Some(SessionObservationEventPayload::ResidentChanged {
                read_view: read_view.clone(),
            })
        } else {
            None
        };
        let Some(payload) = payload else {
            return;
        };

        let mut drafts = Vec::with_capacity(2);
        if previous.persisted_state.current_frame_node_id
            != next.persisted_state.current_frame_node_id
            && let Some(frame_id) = next.persisted_state.current_frame_node_id.clone()
        {
            drafts.push(LiveReplayEventDraft::new(
                None::<String>,
                SessionObservationEventPayload::AgentFrameSwitched {
                    frame_id: frame_id.into_inner(),
                },
            ));
        }
        drafts.push(LiveReplayEventDraft::new(turn_id, payload));

        let prepared = match self.live_replay_store.prepare_publication(
            runtime.session_id(),
            revision,
            drafts,
        ) {
            Ok(prepared) => prepared,
            Err(err) => {
                tracing::warn!(
                    session_id = %runtime.session_id(),
                    error = %err,
                    "failed to reserve session observation publication; reconnect will fall back to gap recovery",
                );
                next.cursor = self
                    .live_replay_store
                    .current_cursor(runtime.session_id(), revision);
                self.observation.store(Arc::new(next));
                return;
            }
        };
        next.cursor = prepared.latest_cursor().clone();
        self.observation.store(Arc::new(next));
        if let Err(err) = self.live_replay_store.publish_prepared(prepared) {
            tracing::warn!(
                session_id = %runtime.session_id(),
                error = %err,
                "failed to publish prepared session observation; reconnect will fall back to gap recovery",
            );
        }
    }

    fn publish_live_events(
        &self,
        session_id: &str,
        revision: SessionRevision,
        drafts: Vec<LiveReplayEventDraft>,
        failure: &'static str,
    ) {
        let result = self
            .live_replay_store
            .prepare_publication(session_id, revision, drafts)
            .and_then(|prepared| {
                self.live_replay_store
                    .publish_prepared(prepared)
                    .map(|_| ())
            });
        if let Err(err) = result {
            tracing::warn!(session_id = %session_id, error = %err, "{failure}");
        }
    }

    pub fn record_turn_activity(&self, turn_id: Option<&str>, activity: crate::TurnActivity) {
        let observation = self.observe();
        self.publish_live_events(
            observation.session_id(),
            observation.session_revision(),
            vec![LiveReplayEventDraft::new(
                turn_id,
                SessionObservationEventPayload::TurnActivity(activity),
            )],
            "failed to publish live turn activity to session observation replay; reconnect may require gap recovery",
        );
    }

    pub fn record_queue_changed(&self, kind: SessionQueueEventKind, batch_ids: Vec<String>) {
        let observation = self.observe();
        self.publish_live_events(
            observation.session_id(),
            observation.session_revision(),
            vec![LiveReplayEventDraft::new(
                None::<String>,
                SessionObservationEventPayload::QueueChanged { kind, batch_ids },
            )],
            "failed to publish queue observation event; reconnect may require gap recovery",
        );
    }

    pub fn record_process_changed(&self, kind: SessionProcessEventKind, process_ids: Vec<String>) {
        let observation = self.observe();
        self.publish_live_events(
            observation.session_id(),
            observation.session_revision(),
            vec![LiveReplayEventDraft::new(
                None::<String>,
                SessionObservationEventPayload::ProcessChanged { kind, process_ids },
            )],
            "failed to publish process observation event; reconnect may require gap recovery",
        );
    }

    pub fn current_session_observation(&self) -> SessionObservation {
        self.observe().session_observation()
    }

    pub fn resume_session_observation(
        &self,
        cursor: &SessionCursor,
    ) -> Result<SessionResume, LiveReplayStoreError> {
        let observation = self.observe();
        let requested = cursor.parse_for_session(observation.session_id())?;
        match self.live_replay_store.replay_after_cursor(cursor)? {
            LiveReplayOutcome::Replayed(events)
                if Self::has_replacement_evidence(
                    requested.revision,
                    observation.session_revision(),
                    events.iter().map(AsRef::as_ref),
                ) =>
            {
                Ok(SessionResume::Replayed { events })
            }
            LiveReplayOutcome::Replayed(_) => {
                let (observation, gap) = self.live_replay_gap(
                    cursor,
                    LiveReplayGapReason::Unavailable,
                    observation.as_ref(),
                );
                Ok(SessionResume::Gap { observation, gap })
            }
            LiveReplayOutcome::Gap(reason) => {
                let (observation, gap) = self.live_replay_gap(cursor, reason, observation.as_ref());
                Ok(SessionResume::Gap { observation, gap })
            }
        }
    }

    pub fn subscribe_session_observation(
        &self,
        cursor: &SessionCursor,
    ) -> Result<SessionObservationSubscription, LiveReplayStoreError> {
        let observation = self.observe();
        let requested = cursor.parse_for_session(observation.session_id())?;
        match self.live_replay_store.subscribe_after_cursor(cursor)? {
            LiveReplaySubscribeOutcome::Subscribed(subscription)
                if requested.revision == observation.session_revision()
                    || (requested.revision < observation.session_revision()
                        && subscription
                            .contains_committed_at_or_after(observation.session_revision())) =>
            {
                Ok(SessionObservationSubscription::Subscribed(subscription))
            }
            LiveReplaySubscribeOutcome::Subscribed(_) => {
                let (observation, gap) = self.live_replay_gap(
                    cursor,
                    LiveReplayGapReason::Unavailable,
                    observation.as_ref(),
                );
                Ok(SessionObservationSubscription::Gap { observation, gap })
            }
            LiveReplaySubscribeOutcome::Gap(reason) => {
                let (observation, gap) = self.live_replay_gap(cursor, reason, observation.as_ref());
                Ok(SessionObservationSubscription::Gap { observation, gap })
            }
        }
    }

    fn has_replacement_evidence<'a>(
        requested_revision: SessionRevision,
        authoritative_revision: SessionRevision,
        events: impl IntoIterator<Item = &'a SessionObservationEvent>,
    ) -> bool {
        requested_revision == authoritative_revision
            || (requested_revision < authoritative_revision
                && events.into_iter().any(|event| {
                    event.revision() >= authoritative_revision
                        && matches!(
                            &event.payload,
                            SessionObservationEventPayload::Committed { .. }
                        )
                }))
    }

    fn live_replay_gap(
        &self,
        requested_cursor: &SessionCursor,
        reason: LiveReplayGapReason,
        observation: &RuntimeObservation,
    ) -> (SessionObservation, LiveReplayGap) {
        let latest_revision = observation.session_revision();
        let observation_cursor = observation.cursor();
        let current_cursor = self
            .live_replay_store
            .current_cursor(observation.session_id(), latest_revision);
        let latest_cursor = match (
            requested_cursor.parse_for_session(observation.session_id()),
            observation_cursor.parse_for_session(observation.session_id()),
            current_cursor.parse_for_session(observation.session_id()),
        ) {
            (Ok(requested), Ok(observation), Ok(current)) => [
                (observation.live_position, observation_cursor.clone()),
                (current.live_position, current_cursor),
            ]
            .into_iter()
            .filter(|(position, _)| *position != requested.live_position)
            .min_by_key(|(position, _)| *position)
            .map_or_else(|| observation_cursor.clone(), |(_, cursor)| cursor),
            _ => observation_cursor.clone(),
        };
        (
            SessionObservation {
                read_view: observation.read_view.clone(),
                cursor: latest_cursor.clone(),
            },
            LiveReplayGap {
                session_id: observation.session_id().to_string(),
                requested_cursor: requested_cursor.clone(),
                latest_cursor,
                latest_revision,
                reason,
            },
        )
    }

    pub async fn enqueue_turn_input(
        &self,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
    ) -> Result<crate::PendingTurnInput, crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        let is_next_turn = matches!(ingress, crate::TurnInputIngress::NextTurn);
        super::session_api::enqueue_turn_input_to_store(
            observation.session_id.as_ref().to_string(),
            store,
            observation.queued_work_driver.clone(),
            input,
            ingress,
            source_key,
        )
        .await
        .inspect(|input| {
            self.record_queue_changed(
                SessionQueueEventKind::Enqueued,
                if is_next_turn {
                    vec![input.input_id.clone()]
                } else {
                    Vec::new()
                },
            );
        })
    }

    pub async fn cancel_pending_turn_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        store
            .cancel_pending_turn_input(session_id, input_id)
            .await
            .map_err(|err| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                )
            })
            .inspect(|outcome| {
                if outcome.is_cancelled() {
                    self.record_queue_changed(
                        SessionQueueEventKind::Cancelled,
                        vec![input_id.to_string()],
                    );
                }
            })
    }

    pub async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        store
            .cancel_pending_turn_inputs(session_id, targets)
            .await
            .map_err(|err| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                )
            })
            .inspect(|results| {
                let cancelled_ids = results
                    .iter()
                    .filter_map(|result| match &result.outcome {
                        crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
                            Some(input.input_id.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !cancelled_ids.is_empty() {
                    self.record_queue_changed(SessionQueueEventKind::Cancelled, cancelled_ids);
                }
            })
    }

    pub async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        store
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
            .map_err(|err| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                )
            })
            .inspect(|outcome| {
                let crate::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = outcome
                else {
                    return;
                };
                let cancelled_ids = outcomes
                    .iter()
                    .filter_map(|outcome| match outcome {
                        crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
                            Some(input.input_id.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !cancelled_ids.is_empty() {
                    self.record_queue_changed(SessionQueueEventKind::Cancelled, cancelled_ids);
                }
            })
    }

    /// Release a held queued-work claim without completing it, returning its
    /// batches to the pending queue immediately.
    ///
    /// This is the host lever behind stopping an external queued-work driver
    /// mid-claim: the host clears its ownership and the work becomes claimable
    /// at once instead of remaining held, and hidden from pending views, until
    /// this owner's generation stops holding the session lease.
    pub async fn abandon_queued_work_claim(
        &self,
        claim: &crate::QueuedWorkClaim,
    ) -> Result<(), crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        store
            .abandon_queued_work_claim(claim)
            .await
            .map_err(|err| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                )
            })?;
        self.record_queue_changed(
            SessionQueueEventKind::Enqueued,
            claim
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect(),
        );
        Ok(())
    }

    /// Release a held pending-turn-input claim without completing it, returning
    /// its inputs to the pending queue immediately. The turn-input counterpart
    /// of [`abandon_queued_work_claim`](Self::abandon_queued_work_claim).
    pub async fn abandon_turn_input_claim(
        &self,
        claim: &crate::TurnInputClaim,
    ) -> Result<(), crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        store.abandon_turn_input_claim(claim).await.map_err(|err| {
            crate::RuntimeError::new(crate::RuntimeErrorCode::StoreCommitFailed, err.to_string())
        })?;
        self.record_queue_changed(
            SessionQueueEventKind::Enqueued,
            claim
                .inputs
                .iter()
                .map(|input| input.input_id.clone())
                .collect(),
        );
        Ok(())
    }

    pub async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        store
            .cancel_queued_work_batch(session_id, batch_id)
            .await
            .map_err(|err| {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                )
            })
            .inspect(|batch| {
                if batch.is_some() {
                    self.record_queue_changed(
                        SessionQueueEventKind::Cancelled,
                        vec![batch_id.to_string()],
                    );
                }
            })
    }

    /// How many live references share this handle's runtime, including this
    /// one. `try_into_runtime` can only succeed at `1`; activation traces this
    /// count so a refused promotion is explainable from the trace.
    pub(in crate::runtime) fn runtime_reference_count(&self) -> usize {
        Arc::strong_count(&self.runtime)
    }

    pub fn try_into_runtime(self) -> Result<LashRuntime, Self> {
        match Arc::try_unwrap(self.runtime) {
            Ok(mutex) => Ok(mutex.into_inner()),
            Err(runtime) => Err(Self {
                runtime,
                observation: self.observation,
                live_replay_store: self.live_replay_store,
            }),
        }
    }
}

fn authority_fingerprint(state: &super::RuntimeSessionState) -> Vec<u8> {
    let mut persisted_node_ids = state.persisted_node_ids.iter().collect::<Vec<_>>();
    persisted_node_ids.sort_unstable();
    serde_json::to_vec(&(
        state,
        &state.checkpoint_components,
        state.head_revision,
        persisted_node_ids,
    ))
    .expect("runtime observation authority must serialize")
}

impl LashRuntime {
    fn last_committed_turn_id_for_revision(&self, revision: SessionRevision) -> Option<&str> {
        self.last_committed_observation_turn
            .as_ref()
            .filter(|(committed_revision, _)| *committed_revision == revision.as_u64())
            .map(|(_, turn_id)| turn_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanicLiveReplayStore;

    #[derive(Debug)]
    struct FailCommittedLiveReplayStore {
        inner: InMemoryLiveReplayStore,
    }

    impl FailCommittedLiveReplayStore {
        fn new() -> Self {
            Self {
                inner: InMemoryLiveReplayStore::default(),
            }
        }
    }

    impl LiveReplayStore for FailCommittedLiveReplayStore {
        fn prepare_publication(
            &self,
            session_id: &str,
            revision: SessionRevision,
            events: Vec<LiveReplayEventDraft>,
        ) -> Result<PreparedLiveReplayPublication, LiveReplayStoreError> {
            if events.iter().any(|event| {
                matches!(
                    &event.payload,
                    SessionObservationEventPayload::Committed { .. }
                )
            }) {
                return Err(LiveReplayStoreError::Store(
                    "injected committed-event append failure".to_string(),
                ));
            }
            self.inner.prepare_publication(session_id, revision, events)
        }

        fn publish_prepared(
            &self,
            prepared: PreparedLiveReplayPublication,
        ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
            self.inner.publish_prepared(prepared)
        }

        fn replay_after_cursor(
            &self,
            cursor: &SessionCursor,
        ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
            self.inner.replay_after_cursor(cursor)
        }

        fn subscribe_after_cursor(
            &self,
            cursor: &SessionCursor,
        ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
            self.inner.subscribe_after_cursor(cursor)
        }

        fn current_cursor(&self, session_id: &str, revision: SessionRevision) -> SessionCursor {
            self.inner.current_cursor(session_id, revision)
        }

        fn trim_session(&self, session_id: &str) -> Result<(), LiveReplayStoreError> {
            self.inner.trim_session(session_id)
        }
    }

    impl LiveReplayStore for PanicLiveReplayStore {
        fn prepare_publication(
            &self,
            _session_id: &str,
            _revision: SessionRevision,
            _events: Vec<LiveReplayEventDraft>,
        ) -> Result<PreparedLiveReplayPublication, LiveReplayStoreError> {
            panic!("prepare should not be called by cursor rejection tests")
        }

        fn publish_prepared(
            &self,
            _prepared: PreparedLiveReplayPublication,
        ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
            panic!("publish should not be called by cursor rejection tests")
        }

        fn replay_after_cursor(
            &self,
            _cursor: &SessionCursor,
        ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
            panic!("replay_after_cursor should not be called for rejected cursors")
        }

        fn subscribe_after_cursor(
            &self,
            _cursor: &SessionCursor,
        ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
            panic!("subscribe_after_cursor should not be called for rejected cursors")
        }

        fn current_cursor(&self, session_id: &str, revision: SessionRevision) -> SessionCursor {
            SessionCursor::new("panic-replay-incarnation", session_id, revision, 0)
        }

        fn trim_session(&self, _session_id: &str) -> Result<(), LiveReplayStoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn runtime_rejects_bad_cursors_before_replay_store_gap_handling() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("session-a")
            .with_policy(crate::SessionPolicy {
                model: crate::ModelSpec::builder("test-model")
                    .context_window_tokens(1024)
                    .build()
                    .expect("model"),
                ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
            })
            .build(),
        )
        .await
        .expect("runtime");
        let handle = RuntimeHandle::with_live_replay_store(runtime, Arc::new(PanicLiveReplayStore));
        let wrong_session = SessionCursor::new(
            "panic-replay-incarnation",
            "session-b",
            SessionRevision(0),
            99,
        );
        let malformed = SessionCursor::from_raw_for_testing("bad");

        assert!(matches!(
            handle.resume_session_observation(&wrong_session),
            Err(LiveReplayStoreError::Cursor(
                SessionCursorError::WrongSession { .. }
            ))
        ));
        assert!(matches!(
            handle.subscribe_session_observation(&wrong_session),
            Err(LiveReplayStoreError::Cursor(
                SessionCursorError::WrongSession { .. }
            ))
        ));
        assert!(matches!(
            handle.resume_session_observation(&malformed),
            Err(LiveReplayStoreError::Cursor(
                SessionCursorError::Malformed { .. }
            ))
        ));
        assert!(matches!(
            handle.subscribe_session_observation(&malformed),
            Err(LiveReplayStoreError::Cursor(
                SessionCursorError::Malformed { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn empty_is_proven_continuity_not_missing_history_for_future_revision() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("future-revision-cursor")
            .with_policy(crate::SessionPolicy {
                model: crate::ModelSpec::builder("test-model")
                    .context_window_tokens(1024)
                    .build()
                    .expect("model"),
                ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
            })
            .build(),
        )
        .await
        .expect("runtime");
        let handle = RuntimeHandle::new(runtime);
        let ahead = SessionCursor::new(
            "future-replay-incarnation",
            "future-revision-cursor",
            SessionRevision::new(1),
            0,
        );

        assert!(matches!(
            handle
                .resume_session_observation(&ahead)
                .expect("resume future revision"),
            SessionResume::Gap {
                gap: LiveReplayGap {
                    reason: LiveReplayGapReason::Unavailable,
                    latest_revision: SessionRevision(0),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            handle
                .subscribe_session_observation(&ahead)
                .expect("subscribe future revision"),
            SessionObservationSubscription::Gap {
                gap: LiveReplayGap {
                    reason: LiveReplayGapReason::Unavailable,
                    latest_revision: SessionRevision(0),
                    ..
                },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn publish_revision_matches_the_single_export_across_a_commit() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("revision-equivalence")
            .with_policy(crate::SessionPolicy {
                model: crate::ModelSpec::builder("test-model")
                    .context_window_tokens(1024)
                    .build()
                    .expect("model"),
                ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
            })
            .build(),
        )
        .await
        .expect("runtime");
        let handle = RuntimeHandle::new(runtime);
        let writer = handle.writer();
        let mut runtime = writer.lock().await;
        runtime.state.turn_index = 9;
        runtime.state.head_revision = 17;

        let exported = runtime.export_persistence_state();
        let exported_revision = SessionRevision::from_state(&exported);
        let accessor_revision = SessionRevision::from_runtime(&runtime);
        assert_eq!(accessor_revision, exported_revision);

        handle.publish_from(&runtime);
        assert_eq!(handle.observe().session_revision(), exported_revision);
    }

    #[tokio::test]
    async fn publish_keeps_frame_switch_immediately_before_resident_change() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("publish-order")
            .with_policy(crate::SessionPolicy {
                model: crate::ModelSpec::builder("test-model")
                    .context_window_tokens(1024)
                    .build()
                    .expect("model"),
                ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
            })
            .build(),
        )
        .await
        .expect("runtime");
        let handle = RuntimeHandle::new(runtime);
        let cursor = handle.observe().cursor().clone();
        let writer = handle.writer();
        let mut runtime = writer.lock().await;
        runtime.state.current_frame_node_id = Some(crate::session_graph::frame_node_id(
            &runtime.state.session_id,
            "next-frame",
        ));

        handle.publish_from(&runtime);
        let SessionResume::Replayed { events } = handle
            .resume_session_observation(&cursor)
            .expect("replay publication")
        else {
            panic!("publication should remain replayable");
        };
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].payload,
            SessionObservationEventPayload::AgentFrameSwitched { .. }
        ));
        assert_eq!(events[0].turn_id, None);
        assert!(matches!(
            events[1].payload,
            SessionObservationEventPayload::ResidentChanged { .. }
        ));
        assert_eq!(events[1].turn_id, None);
    }

    #[tokio::test]
    async fn failed_authoritative_batch_does_not_publish_auxiliary_event() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("auxiliary-reconciliation")
            .with_policy(crate::SessionPolicy {
                model: crate::ModelSpec::builder("test-model")
                    .context_window_tokens(1024)
                    .build()
                    .expect("model"),
                ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
            })
            .build(),
        )
        .await
        .expect("runtime");
        let replay_store = Arc::new(FailCommittedLiveReplayStore::new());
        let handle = RuntimeHandle::with_live_replay_store(runtime, replay_store.clone());
        let cursor = handle.observe().cursor().clone();
        let writer = handle.writer();
        let mut runtime = writer.lock().await;
        runtime.state.turn_index = 1;
        runtime.state.current_frame_node_id = Some(crate::session_graph::frame_node_id(
            &runtime.state.session_id,
            "next-frame",
        ));

        handle.publish_from(&runtime);
        drop(runtime);
        let LiveReplayOutcome::Replayed(events) = replay_store
            .replay_after_cursor(&cursor)
            .expect("inspect retained auxiliary event")
        else {
            panic!("the retained auxiliary event should remain positionally replayable");
        };
        assert!(
            events.is_empty(),
            "an atomic authoritative batch must not expose its frame switch when reservation fails"
        );

        assert!(matches!(
            handle
                .resume_session_observation(&cursor)
                .expect("resume through public runtime seam"),
            SessionResume::Gap {
                gap: LiveReplayGap {
                    reason: LiveReplayGapReason::Unavailable,
                    latest_revision: SessionRevision(1),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            handle
                .subscribe_session_observation(&cursor)
                .expect("subscribe through public runtime seam"),
            SessionObservationSubscription::Gap {
                gap: LiveReplayGap {
                    reason: LiveReplayGapReason::Unavailable,
                    latest_revision: SessionRevision(1),
                    ..
                },
                ..
            }
        ));
    }
}
