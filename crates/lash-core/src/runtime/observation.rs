use crate::SessionId;
use crate::TurnId;
use lash_sansio::sync::MutexExt;
mod process_lifecycle;
pub(crate) mod replay;

use crate::facade_support::ToolStateFacadeOps;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{LashRuntime, ProcessHandleView, ProcessRecord, ProcessRegistry};

pub(in crate::runtime) use replay::observation_revision;
pub use replay::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayEventDraft, LiveReplayGap,
    LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, LiveReplaySubscription, PreparedLiveReplayPublication,
    SessionCursor, SessionCursorError, SessionObservation, SessionObservationEvent,
    SessionObservationEventPayload, SessionObservationSubscription, SessionProcessEventKind,
    SessionQueueEventKind, SessionResume, SessionRevision,
};

/// The plugin query services one resident session publishes together.
///
/// They are captured from a single source during observation construction and
/// are all present or all absent; they are not independently optional.
#[derive(Clone)]
pub struct ObservationPluginServices {
    session: Arc<crate::PluginSession>,
    read: Arc<dyn crate::plugin::SessionReadService>,
    process_read: Arc<dyn crate::plugin::ProcessReadService>,
}

#[derive(Clone)]
pub struct RuntimeObservation {
    pub session_id: Arc<str>,
    pub revision: SessionRevision,
    pub cursor: SessionCursor,
    pub read_view: crate::SessionReadView,
    /// The session's current durable frame identity at publication time.
    /// Together with `session_id` it is the scope root the frame-scoped
    /// process listing and host probes build from.
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    /// The committed turn index at publication time.
    pub turn_index: usize,
    pub usage_report: super::SessionUsageReport,
    pub tool_state: Option<crate::ToolState>,
    /// The session's active tool catalog, or the capture error. One field —
    /// an error never travels with a catalog.
    pub tool_catalog: Result<Arc<Vec<serde_json::Value>>, String>,
    /// The plugin query services, present exactly when a resident session
    /// could supply all of them.
    pub plugin_services: Option<ObservationPluginServices>,
    pub process_registry: Option<Arc<dyn ProcessRegistry>>,
    pub queue_store: Option<Arc<dyn crate::RuntimePersistence>>,
    pub queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
    /// Fingerprint of the resident authority at publication time, compared
    /// across publishes to detect revision-stable resident changes without
    /// retaining the resident state itself.
    authority_fingerprint: Vec<u8>,
}

impl RuntimeObservation {
    fn from_runtime(
        runtime: &LashRuntime,
        cursor: SessionCursor,
        previous: Option<&RuntimeObservation>,
        revision: SessionRevision,
        read_view: crate::SessionReadView,
        usage_report: super::SessionUsageReport,
        authority_fingerprint: Vec<u8>,
    ) -> Self {
        let tool_catalog = runtime
            .active_tool_catalog_shared()
            .map_err(|err| err.to_string());
        let tool_state_generation = runtime
            .resident_session
            .is_valid()
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
        let plugin_services = match (runtime.session.as_ref(), runtime.runtime_session_services()) {
            (Some(session), Ok(services)) => Some(ObservationPluginServices {
                session: Arc::clone(session.plugins()),
                read: services.read_service(),
                process_read: services.process_read_service(),
            }),
            (_, Err(err)) => {
                tracing::warn!(
                    session_id = %runtime.session_id(),
                    error = %err,
                    "failed to capture plugin query services for observation",
                );
                None
            }
            (None, _) => None,
        };
        Self {
            session_id: Arc::from(runtime.session_id()),
            revision,
            cursor,
            read_view,
            current_frame_node_id: runtime.state.current_frame_node_id.clone(),
            turn_index: runtime.state.turn_index,
            usage_report,
            tool_state,
            tool_catalog,
            plugin_services,
            process_registry: runtime.host.process_registry().cloned(),
            queue_store: runtime
                .session
                .as_ref()
                .and_then(|session| session.history_store()),
            queued_work: Arc::clone(runtime.host.queued_work()),
            authority_fingerprint,
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

    pub fn turn_scope(&self, turn_id: impl Into<TurnId>) -> crate::ExecutionScope {
        crate::ExecutionScope::turn(self.session_id.as_ref(), turn_id)
    }

    pub fn queue_drain_scope(&self, drain_id: impl Into<String>) -> crate::ExecutionScope {
        crate::ExecutionScope::queue_drain(self.session_id.as_ref(), drain_id)
    }

    pub async fn query_plugin(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
    ) -> Result<(String, serde_json::Value), crate::PluginOperationInvokeError> {
        let Some(services) = self.plugin_services.as_ref() else {
            return Err(crate::PluginOperationInvokeError::Unknown(
                "runtime plugin query services not available".to_string(),
            ));
        };
        services
            .session
            .query_plugin(
                name,
                args,
                session_id,
                true,
                Arc::clone(&services.read),
                Arc::clone(&services.process_read),
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
        if let Some(agent_frame_id) = self.current_frame_node_id.as_ref() {
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

#[expect(
    clippy::expect_used,
    reason = "resident state is normalized before publication"
)]
fn export_observation_state(
    runtime: &LashRuntime,
) -> (crate::SessionReadView, super::SessionUsageReport, Vec<u8>) {
    // Observation publication is synchronous. When resident state has been
    // invalidated, project only the already-adopted durable snapshot; never
    // recapture live plugin/tool state before the async reload gate runs.
    let read_view = runtime
        .read_view()
        .expect("resident runtime state is normalized before observation publication");
    let shared_ledger = runtime.shared_token_ledger.lock_recover();
    let mut token_ledger = runtime.state.token_ledger.clone();
    let mut saturated = false;
    for entry in shared_ledger.iter().cloned() {
        saturated |= super::merge_ledger_entry_saturating(&mut token_ledger, entry.entry);
    }
    let usage_report =
        super::SessionUsageReport::from_entries_with_saturation(&token_ledger, saturated);
    (
        read_view,
        usage_report,
        authority_fingerprint(&runtime.state, &token_ledger),
    )
}

async fn list_scope_process_handles(
    executor: &Arc<dyn crate::ProcessRegistry>,
    scope: &crate::SessionScope,
    mode: crate::ProcessListMode,
) -> Vec<ProcessRecord> {
    match mode {
        crate::ProcessListMode::Live => executor.list_live_observed_by(&scope.session_id).await,
        crate::ProcessListMode::All => {
            executor
                .list_observed_by(
                    &scope.session_id,
                    &crate::ProcessListFilter {
                        status: crate::ProcessStatusFilter::Any,
                        ..Default::default()
                    },
                )
                .await
        }
    }
    .unwrap_or_default()
}

#[derive(Clone)]
pub struct RuntimeHandle {
    pub runtime: Arc<Mutex<LashRuntime>>,
    pub observation: Arc<ArcSwap<RuntimeObservation>>,
    pub live_replay_store: Arc<dyn LiveReplayStore>,
    pub process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub process_engines: crate::ProcessEngineRegistry,
}

impl RuntimeHandle {
    pub fn new(runtime: LashRuntime) -> Self {
        Self::with_live_replay_store(runtime, Arc::new(InMemoryLiveReplayStore::default()))
    }

    pub fn with_live_replay_store(
        runtime: LashRuntime,
        live_replay_store: Arc<dyn LiveReplayStore>,
    ) -> Self {
        let process_env_store = Arc::clone(&runtime.host.core.durability.process_env_store);
        let process_engines = runtime.host.core.process_engines.clone();
        let revision = SessionRevision::from_runtime(&runtime);
        let cursor =
            live_replay_store.current_cursor(&SessionId::from(runtime.session_id()), revision);
        let (read_view, usage_report, authority_fingerprint) = export_observation_state(&runtime);
        let observation = RuntimeObservation::from_runtime(
            &runtime,
            cursor,
            None,
            revision,
            read_view,
            usage_report,
            authority_fingerprint,
        );
        Self {
            runtime: Arc::new(Mutex::new(runtime)),
            observation: Arc::new(ArcSwap::from_pointee(observation)),
            live_replay_store,
            process_env_store,
            process_engines,
        }
    }

    pub fn writer(&self) -> Arc<Mutex<LashRuntime>> {
        Arc::clone(&self.runtime)
    }

    /// Retire an execution artifact owner across the runtime's environment and
    /// process-engine stores after its effect journal is durably unreachable.
    pub async fn retire_artifact_owner(
        &self,
        owner: &crate::ArtifactOwner,
    ) -> Result<(), crate::PluginError> {
        self.process_env_store
            .retire_process_execution_env_owner(owner)
            .await?;
        self.process_engines.retire_artifact_owner(owner).await
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
        let (read_view, usage_report, authority_fingerprint) = export_observation_state(runtime);
        let mut next = RuntimeObservation::from_runtime(
            runtime,
            previous.cursor.clone(),
            Some(previous.as_ref()),
            revision,
            read_view.clone(),
            usage_report,
            authority_fingerprint,
        );
        let payload = if previous.revision < revision {
            Some(SessionObservationEventPayload::Committed {
                read_view: read_view.clone(),
            })
        } else if force_resident || previous.authority_fingerprint != next.authority_fingerprint {
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
        if previous.current_frame_node_id != next.current_frame_node_id
            && let Some(frame_id) = next.current_frame_node_id.clone()
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
            &SessionId::from(runtime.session_id()),
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
                    .current_cursor(&SessionId::from(runtime.session_id()), revision);
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
        session_id: &SessionId,
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

    pub fn record_turn_activity(&self, turn_id: Option<&TurnId>, activity: crate::TurnActivity) {
        let observation = self.observe();
        self.publish_live_events(
            &SessionId::from(observation.session_id()),
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
            &SessionId::from(observation.session_id()),
            observation.session_revision(),
            vec![LiveReplayEventDraft::new(
                None::<String>,
                SessionObservationEventPayload::QueueChanged { kind, batch_ids },
            )],
            "failed to publish queue observation event; reconnect may require gap recovery",
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
        let requested = cursor.parse_for_session(&SessionId::from(observation.session_id()))?;
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
        let requested = cursor.parse_for_session(&SessionId::from(observation.session_id()))?;
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
            .current_cursor(&SessionId::from(observation.session_id()), latest_revision);
        let latest_cursor = match (
            requested_cursor.parse_for_session(&SessionId::from(observation.session_id())),
            observation_cursor.parse_for_session(&SessionId::from(observation.session_id())),
            current_cursor.parse_for_session(&SessionId::from(observation.session_id())),
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
                session_id: SessionId::from(observation.session_id().to_string()),
                requested_cursor: requested_cursor.clone(),
                latest_cursor,
                latest_revision,
                reason,
            },
        )
    }

    /// Build this live session's Durable Session operations and its queue
    /// store.
    ///
    /// The live handle reaches the queue through the same bodies a
    /// catalog-acquired Durable Session uses; nothing here re-implements a
    /// store call or a publication.
    fn durable_queue(
        &self,
    ) -> Result<(super::DurableSessionOps, Arc<dyn crate::RuntimePersistence>), crate::RuntimeError>
    {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        let ops = super::DurableSessionOps::new(
            SessionId::from(observation.session_id().to_string()),
            Arc::clone(&observation.queued_work),
            Arc::clone(&self.live_replay_store),
        );
        Ok((ops, store))
    }

    pub async fn enqueue_turn_input(
        &self,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
    ) -> Result<crate::PendingTurnInput, crate::RuntimeError> {
        let (ops, store) = self.durable_queue()?;
        ops.enqueue_turn_input(&store, input, ingress, source_key)
            .await
    }

    pub async fn cancel_pending_turn_input(
        &self,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, crate::RuntimeError> {
        let (ops, store) = self.durable_queue()?;
        ops.cancel_pending_turn_input(&store, input_id).await
    }

    pub async fn cancel_pending_turn_inputs(
        &self,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::RuntimeError> {
        let (ops, store) = self.durable_queue()?;
        ops.cancel_pending_turn_inputs(&store, targets).await
    }

    pub async fn cancel_pending_turn_input_suffix(
        &self,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::RuntimeError> {
        let (ops, store) = self.durable_queue()?;
        ops.cancel_pending_turn_input_suffix(&store, anchor).await
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
        let (ops, store) = self.durable_queue()?;
        ops.abandon_queued_work_claim(&store, claim).await
    }

    /// Release a held pending-turn-input claim without completing it, returning
    /// its inputs to the pending queue immediately. The turn-input counterpart
    /// of [`abandon_queued_work_claim`](Self::abandon_queued_work_claim).
    pub async fn abandon_turn_input_claim(
        &self,
        claim: &crate::TurnInputClaim,
    ) -> Result<(), crate::RuntimeError> {
        let (ops, store) = self.durable_queue()?;
        ops.abandon_turn_input_claim(&store, claim).await
    }

    pub async fn cancel_queued_work_batch(
        &self,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::RuntimeError> {
        let (ops, store) = self.durable_queue()?;
        ops.cancel_queued_work_batch(&store, batch_id).await
    }
}

#[expect(
    clippy::expect_used,
    reason = "crate-owned state encodes into an in-memory buffer"
)]
fn authority_fingerprint(
    state: &super::RuntimeSessionState,
    token_ledger: &[crate::TokenLedgerEntry],
) -> Vec<u8> {
    // The resident graph contributes its shape, not its serialized nodes:
    // graph bodies are immutable durable history, and every production
    // mutation moves the leaf, the node count, or another covered field, so
    // serializing the node bodies per publish would re-pay an O(graph) cost
    // for no added signal. `persisted_node_ids` is folded into an
    // order-independent digest for the same reason.
    let persisted_nodes_digest = state.persisted_node_ids.iter().fold(0u64, |digest, id| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(id.as_str(), &mut hasher);
        digest.wrapping_add(std::hash::Hasher::finish(&hasher))
    });
    serde_json::to_vec(&(
        &state.session_id,
        &state.policy,
        &state.current_frame_node_id,
        state.session_graph.nodes.len(),
        &state.session_graph.leaf_node_id,
        state.turn_index,
        &state.token_usage,
        &state.last_prompt_usage,
        &state.protocol_turn_options,
        &state.authority,
        &state.checkpoint_components,
        token_ledger,
        &state.checkpoint_ref,
        state.head_revision,
        persisted_nodes_digest,
    ))
    .expect("runtime observation authority must serialize")
}

impl LashRuntime {
    fn last_committed_turn_id_for_revision(&self, revision: SessionRevision) -> Option<&TurnId> {
        self.resident_session
            .last_committed_turn_id_for_revision(revision.as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch_test_frame(state: &mut crate::RuntimeSessionState, material: &str) {
        let frame_key = crate::FrameKey::from_caller_material(material).unwrap();
        let frame_node_id =
            crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
        assert!(state.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key,
            crate::AgentFrameReason::new("observation-test"),
            crate::AgentFrameAssignment::from_policy(state.policy.clone()),
            state.protocol_turn_options.clone(),
            <crate::SystemClock as crate::ClockWallTime>::timestamp_rfc3339(&crate::SystemClock,),
        ));
        state.refresh_current_frame_projection();
    }

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
            session_id: &SessionId,
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

        fn current_cursor(
            &self,
            session_id: &SessionId,
            revision: SessionRevision,
        ) -> SessionCursor {
            self.inner.current_cursor(session_id, revision)
        }

        fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
            self.inner.trim_session(session_id)
        }
    }

    impl LiveReplayStore for PanicLiveReplayStore {
        fn prepare_publication(
            &self,
            _session_id: &SessionId,
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

        fn current_cursor(
            &self,
            session_id: &SessionId,
            revision: SessionRevision,
        ) -> SessionCursor {
            SessionCursor::new("panic-replay-incarnation", session_id, revision, 0)
        }

        fn trim_session(&self, _session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn runtime_rejects_bad_cursors_before_replay_store_gap_handling() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("session-a")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
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
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("future-revision-cursor")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
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
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("revision-equivalence")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
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
        let exported_revision = observation_revision(&exported);
        let accessor_revision = SessionRevision::from_runtime(&runtime);
        assert_eq!(accessor_revision, exported_revision);

        handle.publish_from(&runtime);
        assert_eq!(handle.observe().session_revision(), exported_revision);
    }

    #[tokio::test]
    async fn publish_keeps_frame_switch_immediately_before_resident_change() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("publish-order")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
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
        switch_test_frame(&mut runtime.state, "next-frame");

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
    async fn publication_holds_no_full_state_graph_pin() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("graph-pin")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
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

        // Measure the graph pins each legitimate observation contributor
        // holds, then require the published observation to contribute
        // exactly those — no extra full-state holder. One more pin would
        // force a copy-on-write on every graph mutation until the next
        // publish.
        let pinned_with_observation = runtime.state.session_graph.data_strong_count();
        let read_view = runtime.read_view().expect("read view");
        let read_view_pins =
            runtime.state.session_graph.data_strong_count() - pinned_with_observation;
        drop(read_view);
        let services = runtime.runtime_session_services().expect("plugin services");
        let services_pins =
            runtime.state.session_graph.data_strong_count() - pinned_with_observation;
        drop(services);
        // Resident state (1) + read view + plugin query services snapshot.
        let expected = 1 + read_view_pins + services_pins;
        assert_eq!(pinned_with_observation, expected);

        switch_test_frame(&mut runtime.state, "first-frame");
        handle.publish_from(&runtime);
        assert_eq!(runtime.state.session_graph.data_strong_count(), expected);

        switch_test_frame(&mut runtime.state, "second-frame");
        handle.publish_from(&runtime);
        assert_eq!(runtime.state.session_graph.data_strong_count(), expected);
    }

    #[tokio::test]
    async fn failed_authoritative_batch_does_not_publish_auxiliary_event() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::memory_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("auxiliary-reconciliation")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
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
        switch_test_frame(&mut runtime.state, "next-frame");

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
