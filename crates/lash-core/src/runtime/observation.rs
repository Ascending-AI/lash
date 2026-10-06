use crate::SessionId;
use crate::TurnId;
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
    LiveReplaySubscribeOutcome, LiveReplaySubscription, ParsedSessionCursor, SessionCursor,
    SessionCursorError, SessionObservation, SessionObservationEvent,
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
    pub session_id: SessionId,
    pub revision: SessionRevision,
    pub cursor: SessionCursor,
    pub read_view: crate::SessionReadView,
    /// The session's current durable frame identity at publication time.
    /// Together with `session_id` it is the scope run the frame-scoped
    /// process listing and host probes build from.
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    /// The committed turn index at publication time.
    pub turn_index: usize,
    pub tool_state: Option<crate::ToolState>,
    /// The session's active tool catalog, or the capture error. One field —
    /// an error never travels with a catalog.
    pub tool_catalog: Result<Arc<Vec<serde_json::Value>>, String>,
    /// The plugin query services, present exactly when a resident session
    /// could supply all of them.
    pub plugin_services: Option<ObservationPluginServices>,
    pub process_registry: Option<Arc<dyn ProcessRegistry>>,
    pub queue_store: Option<crate::store::SessionStore>,
    /// The deployment's effect host, which a host reaches the session's
    /// durable waits through without the runtime's writer, such as an
    /// admitted plugin task's cancel signal (FIG-4391).
    pub effect_host: Arc<dyn crate::EffectHost>,
    /// The ingress relay an acceptance through this observation delivers
    /// with (ADR 0109 §3).
    pub ingress: super::shift::IngressRelay,
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
            // No registry is built here (FIG-4857): the session's tool state
            // is what the adopted head recorded (FIG-5139).
            (None, _) => runtime.state.tool_state_snapshot().cloned(),
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
            session_id: runtime.session_id().clone(),
            revision,
            cursor,
            read_view,
            current_frame_node_id: runtime.state.current_frame_node_id.clone(),
            turn_index: runtime.state.turn_index,
            tool_state,
            tool_catalog,
            plugin_services,
            process_registry: runtime.host.process_registry().cloned(),
            queue_store: runtime.services.store.clone(),
            effect_host: runtime.effect_host(),
            ingress: runtime.ingress_relay(),
            authority_fingerprint,
        }
    }

    pub fn session_id(&self) -> &SessionId {
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
        crate::SessionScope::new(self.session_id.clone())
    }

    pub fn process_scope_id(&self) -> crate::SessionScopeId {
        self.process_scope().id()
    }

    pub fn turn_scope(&self, turn_id: impl Into<TurnId>) -> crate::ExecutionScope {
        crate::ExecutionScope::turn(self.session_id.clone(), turn_id)
    }

    pub fn session_operation_scope(
        &self,
        operation_id: impl Into<String>,
    ) -> crate::ExecutionScope {
        crate::ExecutionScope::session_operation(self.session_id.clone(), operation_id)
    }

    pub async fn query_plugin(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<SessionId>,
    ) -> Result<(String, serde_json::Value), crate::PluginOperationInvokeError> {
        let Some(services) = self.plugin_services.as_ref() else {
            return Err(crate::PluginOperationInvokeError::NotPublished {
                session_id: self.session_id.clone(),
            });
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
                self.session_id.clone(),
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

fn export_observation_state(runtime: &LashRuntime) -> (crate::SessionReadView, Vec<u8>) {
    // Observation publication is synchronous. When resident state has been
    // invalidated, project only the already-adopted durable snapshot; never
    // recapture live plugin/tool state before the async reload gate runs.
    // An observer reads the session's record, never a run's execution
    // view: that view outlives its run on resident state, and a replay
    // re-installs it (FIG-4529).
    let read_view = crate::SessionReadView::recorded_from_runtime_state(&runtime.state)
        .with_transcript_options(
            runtime
                .plugin_session()
                .map(|plugins| plugins.transcript_options())
                .unwrap_or_default(),
        );
    (read_view, authority_fingerprint(&runtime.state))
}

/// The session's durable head as an observer reads it: the observation
/// revision it carries and the read view of its current frame, the view
/// [`load_session_read_view`](crate::store::load_session_read_view)
/// answers. The revision and the view come from one window read, so they
/// always agree. `Ok(None)` means the session has no head.
pub async fn load_durable_observation_head(
    store: &crate::store::SessionStore,
) -> Result<Option<(SessionRevision, crate::SessionReadView)>, crate::StoreError> {
    let Some(loaded) =
        crate::store::load_session_window_state(store, crate::store::WindowSelector::Current)
            .await?
    else {
        return Ok(None);
    };
    let meta = store.load_session_meta().await?.ok_or_else(|| {
        crate::StoreError::Backend(format!(
            "session `{}` has durable head state but no session metadata",
            loaded.state.session_id
        ))
    })?;
    Ok(Some((
        observation_revision(&loaded.state),
        crate::SessionReadView::from_persisted_state_with_relation(&loaded.state, meta.relation),
    )))
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

/// A [`RuntimeHandle`] held weakly, every part of it: it keeps nothing of
/// the runtime alive, so a registry of open sessions never holds what a
/// session's engine holds back.
#[derive(Clone)]
pub struct WeakRuntimeHandle {
    runtime: std::sync::Weak<Mutex<LashRuntime>>,
    observation: std::sync::Weak<ArcSwap<RuntimeObservation>>,
    live_replay_store: std::sync::Weak<dyn LiveReplayStore>,
    process_env_store: std::sync::Weak<dyn crate::ProcessExecutionEnvStore>,
    process_engines: crate::WeakProcessEngineRegistry,
}

impl WeakRuntimeHandle {
    /// The handle, while its runtime is alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<RuntimeHandle> {
        Some(RuntimeHandle {
            runtime: self.runtime.upgrade()?,
            observation: self.observation.upgrade()?,
            live_replay_store: self.live_replay_store.upgrade()?,
            process_env_store: self.process_env_store.upgrade()?,
            process_engines: self.process_engines.upgrade()?,
        })
    }

    /// Whether the runtime is still alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.runtime.strong_count() > 0
    }

    /// Whether this is a weak hold on `handle`'s runtime.
    #[must_use]
    pub fn names(&self, handle: &RuntimeHandle) -> bool {
        std::ptr::eq(self.runtime.as_ptr(), Arc::as_ptr(&handle.runtime))
    }
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
        let cursor = live_replay_store.current_cursor(runtime.session_id(), revision);
        let (read_view, authority_fingerprint) = export_observation_state(&runtime);
        let observation = RuntimeObservation::from_runtime(
            &runtime,
            cursor,
            None,
            revision,
            read_view,
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

    /// This handle, holding its runtime weakly: it does not keep the runtime
    /// alive, and [`WeakRuntimeHandle::upgrade`] answers the handle back while
    /// some other holder does.
    #[must_use]
    pub fn downgrade(&self) -> WeakRuntimeHandle {
        WeakRuntimeHandle {
            runtime: Arc::downgrade(&self.runtime),
            observation: Arc::downgrade(&self.observation),
            live_replay_store: Arc::downgrade(&self.live_replay_store),
            process_env_store: Arc::downgrade(&self.process_env_store),
            process_engines: self.process_engines.downgrade(),
        }
    }

    pub fn observe(&self) -> Arc<RuntimeObservation> {
        self.observation.load_full()
    }

    /// Publish `runtime`'s change since this handle's observation, then
    /// install the observation with the cursor the store assigned it.
    pub async fn publish_from(&self, runtime: &LashRuntime) {
        self.publish_from_inner(runtime, false).await;
    }

    /// Publish a revision-stable authoritative resident change that is not
    /// represented in the serializable session projection.
    pub async fn publish_resident_from(&self, runtime: &LashRuntime) {
        self.publish_from_inner(runtime, true).await;
    }

    /// Adopt `runtime`'s state as this handle's observation without
    /// publishing an event: the change it reflects was committed, and
    /// published, by another runtime of the same session (a turn an engine
    /// drove, FIG-3600). The cursor moves to the live replay's current
    /// position.
    pub fn adopt_observation_from(&self, runtime: &LashRuntime) {
        let revision = SessionRevision::from_runtime(runtime);
        let previous = self.observation.load_full();
        let (read_view, authority_fingerprint) = export_observation_state(runtime);
        let cursor = self
            .live_replay_store
            .current_cursor(runtime.session_id(), revision);
        let next = RuntimeObservation::from_runtime(
            runtime,
            cursor,
            Some(previous.as_ref()),
            revision,
            read_view,
            authority_fingerprint,
        );
        self.observation.store(Arc::new(next));
    }

    #[expect(
        clippy::expect_used,
        reason = "resident history was validated on restore and emitted through typed writers"
    )]
    async fn publish_from_inner(&self, runtime: &LashRuntime, force_resident: bool) {
        let revision = SessionRevision::from_runtime(runtime);
        let previous = self.observation.load_full();
        let turn_id = (previous.revision != revision)
            .then(|| runtime.last_committed_turn_id_for_revision(revision))
            .flatten();
        let (read_view, authority_fingerprint) = export_observation_state(runtime);
        let mut next = RuntimeObservation::from_runtime(
            runtime,
            previous.cursor.clone(),
            Some(previous.as_ref()),
            revision,
            read_view,
            authority_fingerprint,
        );
        let payload = if previous.revision < revision {
            let previous_rows = previous
                .read_view
                .transcript()
                .expect("resident history is valid")
                .into_records()
                .into_iter()
                .map(|row| row.row_id)
                .collect::<std::collections::HashSet<_>>();
            let rows = next
                .read_view
                .transcript()
                .expect("resident history is valid")
                .into_records()
                .into_iter()
                .filter(|row| !previous_rows.contains(&row.row_id))
                .collect();
            Some(SessionObservationEventPayload::Committed {
                base_revision: previous.revision,
                rows,
            })
        } else if force_resident || previous.authority_fingerprint != next.authority_fingerprint {
            Some(SessionObservationEventPayload::ResidentChanged)
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
                None::<TurnId>,
                SessionObservationEventPayload::AgentFrameSwitched {
                    frame_id: frame_id.into_inner(),
                },
            ));
        }
        drafts.push(LiveReplayEventDraft::new(turn_id, payload));

        // The store assigns the batch its positions as it publishes it; the
        // observation moves to the batch's cursor only once the batch is
        // visible, so a cursor never names a position nobody can replay.
        next.cursor = match self
            .live_replay_store
            .publish(runtime.session_id(), revision, drafts)
            .await
        {
            Ok(published) => match published.last() {
                Some(event) => event.cursor.clone(),
                None => self
                    .live_replay_store
                    .current_cursor(runtime.session_id(), revision),
            },
            Err(err) => {
                tracing::warn!(
                    session_id = %runtime.session_id(),
                    error = %err,
                    "failed to publish session observation; reconnect will fall back to gap recovery",
                );
                self.live_replay_store
                    .current_cursor(runtime.session_id(), revision)
            }
        };
        self.observation.store(Arc::new(next));
    }

    async fn publish_live_events(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        drafts: Vec<LiveReplayEventDraft>,
        failure: &'static str,
    ) {
        if let Err(err) = self
            .live_replay_store
            .publish(session_id, revision, drafts)
            .await
        {
            tracing::warn!(session_id = %session_id, error = %err, "{failure}");
        }
    }

    pub async fn record_turn_activity(
        &self,
        turn_id: Option<&TurnId>,
        activity: crate::TurnActivity,
    ) {
        let observation = self.observe();
        self.publish_live_events(
            observation.session_id(),
            observation.session_revision(),
            vec![LiveReplayEventDraft::new(
                turn_id,
                SessionObservationEventPayload::TurnActivity(activity),
            )],
            "failed to publish live turn activity to session observation replay; reconnect may require gap recovery",
        )
        .await;
    }

    pub async fn record_queue_changed(&self, kind: SessionQueueEventKind, batch_ids: Vec<String>) {
        let observation = self.observe();
        self.publish_live_events(
            observation.session_id(),
            observation.session_revision(),
            vec![LiveReplayEventDraft::new(
                None::<TurnId>,
                SessionObservationEventPayload::QueueChanged { kind, batch_ids },
            )],
            "failed to publish queue observation event; reconnect may require gap recovery",
        )
        .await;
    }

    /// Build this live session's Durable Session operations and its queue
    /// store.
    ///
    /// The live handle reaches the queue through the same bodies a
    /// catalog-acquired Durable Session uses; nothing here re-implements a
    /// store call or a publication.
    fn durable_queue(
        &self,
    ) -> Result<(super::DurableSessionOps, crate::store::SessionStore), crate::RuntimeError> {
        let observation = self.observe();
        let store = observation
            .queue_store
            .clone()
            .ok_or_else(super::session_api::queued_turn_input_store_required)?;
        let ops = super::DurableSessionOps::new(
            observation.session_id().clone(),
            observation.ingress.clone(),
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
        ops.enqueue_turn_input(
            &store,
            input,
            ingress,
            source_key,
            crate::RunSpec::default(),
        )
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
fn authority_fingerprint(state: &super::RuntimeSessionState) -> Vec<u8> {
    // The resident graph contributes its shape, not its serialized nodes:
    // graph bodies are immutable durable history, and every production
    // mutation moves the leaf, the node count, or another covered field, so
    // serializing the node bodies per publish would re-pay an O(graph) cost
    // for no added signal. `persisted_node_ids` contributes the
    // order-independent digest it keeps in step with its writes.
    let persisted_nodes_digest = state.persisted_node_ids.digest();
    serde_json::to_vec(&(
        &state.session_id,
        &state.policy,
        &state.current_frame_node_id,
        state.session_graph.nodes.len(),
        &state.session_graph.leaf_node_id,
        state.turn_index,
        &state.token_usage,
        &state.last_prompt_usage,
        &state.authority,
        &state.checkpoint_components,
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
            crate::AgentFrameAssignment::unconfigured(state.policy.clone()),
            <crate::SystemClock as crate::ClockWallTime>::node_timestamp(&crate::SystemClock,),
        ));
        state.refresh_current_frame_projection();
    }

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

    #[async_trait::async_trait]
    impl LiveReplayStore for FailCommittedLiveReplayStore {
        async fn publish(
            &self,
            session_id: &SessionId,
            revision: SessionRevision,
            events: Vec<LiveReplayEventDraft>,
        ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
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
            self.inner.publish(session_id, revision, events).await
        }

        async fn replay_after_cursor(
            &self,
            cursor: &SessionCursor,
        ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
            self.inner.replay_after_cursor(cursor).await
        }

        async fn subscribe_after_cursor(
            &self,
            cursor: &SessionCursor,
        ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
            self.inner.subscribe_after_cursor(cursor).await
        }

        fn current_cursor(
            &self,
            session_id: &SessionId,
            revision: SessionRevision,
        ) -> SessionCursor {
            self.inner.current_cursor(session_id, revision)
        }

        async fn invalidate_session(
            &self,
            session_id: &SessionId,
        ) -> Result<(), LiveReplayStoreError> {
            self.inner.invalidate_session(session_id).await
        }

        async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
            self.inner.trim_session(session_id).await
        }
    }

    #[tokio::test]
    async fn publish_keeps_frame_switch_immediately_before_resident_change() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::sqlite_memory_store_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("publish-order")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::SessionPolicy {
                model: Some(crate::LlmProfileConfig::new(
                    crate::RecordedLlmProfile::mint(
                        crate::LlmProfileKey::from("test-model"),
                        crate::LlmProfileMetadata::builder("test-model")
                            .context_window_tokens(1024)
                            .build()
                            .expect("model"),
                    ),
                )),
                ..crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
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

        handle.publish_from(&runtime).await;
        let LiveReplayOutcome::Replayed(events) = handle
            .live_replay_store
            .replay_after_cursor(&cursor)
            .await
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
            SessionObservationEventPayload::ResidentChanged
        ));
        assert_eq!(events[1].turn_id, None);
    }

    #[tokio::test]
    async fn failed_authoritative_batch_does_not_publish_auxiliary_event() {
        let runtime = Box::pin(
            LashRuntime::builder(
                crate::RuntimeHostConfig::new(
                    crate::testing::sqlite_memory_store_backend().await,
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id("auxiliary-reconciliation")
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_policy(crate::SessionPolicy {
                model: Some(crate::LlmProfileConfig::new(
                    crate::RecordedLlmProfile::mint(
                        crate::LlmProfileKey::from("test-model"),
                        crate::LlmProfileMetadata::builder("test-model")
                            .context_window_tokens(1024)
                            .build()
                            .expect("model"),
                    ),
                )),
                ..crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
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

        handle.publish_from(&runtime).await;
        drop(runtime);
        let LiveReplayOutcome::Replayed(events) = replay_store
            .replay_after_cursor(&cursor)
            .await
            .expect("inspect retained auxiliary event")
        else {
            panic!("the retained auxiliary event should remain positionally replayable");
        };
        assert!(
            events.is_empty(),
            "an atomic authoritative batch must not expose its frame switch when publication fails"
        );
    }
}
