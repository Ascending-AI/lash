use super::*;
use crate::SessionId;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::sync::MutexExt;

impl LashRuntime {
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    /// Whether this open declared it will not run a turn (FIG-3353). The host
    /// configuration is the per-open authority: `RuntimeSessionState` carries
    /// a copy so cloned states and store-level stamping self-guard, but every
    /// whole-state replacement rebuilds the field, so the runtime reasserts
    /// the marker from here at each adoption and stamp boundary.
    pub(in crate::runtime) fn preserves_persisted_tool_state(&self) -> bool {
        self.host.core.control.tool_surface_open_mode
            == crate::ToolSurfaceOpenMode::PreservePersisted
    }

    /// Reassert the per-open tool-preservation claim onto the resident state
    /// after a whole-state replacement or before a stamp boundary.
    pub(in crate::runtime) fn reapply_tool_state_preservation_marker(&mut self) {
        self.state.preserve_tool_state_snapshot = self.preserves_persisted_tool_state();
    }

    /// The shared gate for the `PreservePersisted` contract (FIG-3353): an
    /// open that declared it would not run a turn may never execute one — its
    /// tool surface was never reconciled and no `ToolSourcePolicy` was
    /// enforced, so every turn-execution entry refuses before admission.
    pub(in crate::runtime) fn refuse_turn_execution_on_preserved_tool_surface(
        &self,
    ) -> Result<(), RuntimeError> {
        if self.preserves_persisted_tool_state() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::TurnExecutionRequiresReconciledToolSurface,
                format!(
                    "session `{}` was opened with `ToolSurfaceOpenMode::PreservePersisted` \
                     (enqueue-only); reopen it in `Reconcile` mode to run a turn",
                    self.state.session_id
                ),
            ));
        }
        Ok(())
    }

    pub fn stamp_live_plugin_state(&mut self) {
        // Whole-state replacements (resident reload, append-receipt replay)
        // rebuild `self.state`; reassert the per-open claim before the flag
        // is consulted so the durable snapshot is never lost in the gap.
        self.reapply_tool_state_preservation_marker();
        if let Some(session) = self.session.as_ref() {
            // A `PreservePersisted` open never reconciled its registry, so
            // exporting it would overwrite the durable surface with whatever
            // the sources happen to advertise. The loaded snapshot stays on
            // the state and rides the next commit forward untouched.
            if !self.state.preserve_tool_state_snapshot {
                let snapshot = session.plugins().tool_registry().export_state();
                self.state.set_tool_state_snapshot(Some(snapshot));
            }
            self.state.capture_plugin_states(session.plugins());
        } else {
            self.state.set_tool_state_snapshot(None);
            self.state.set_plugin_state(None);
        }
    }

    /// Publish the already-adopted runtime authority to the live plugin
    /// session, invalidating discovery caches only when it changed.
    pub(super) fn publish_plugin_tool_access(&self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session
            .plugins()
            .replace_tool_access(self.state.authority.tool_access.clone())
        {
            session.invalidate_runtime_caches();
        }
    }
    pub fn active_tool_catalog_shared(
        &self,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        match self.resident_session.validity() {
            ResidentSessionState::Invalidated { decision_id } => {
                self.trace_synchronous_resident_state_refusal(
                    decision_id,
                    "active_tool_catalog_shared",
                );
                return Err(crate::PluginError::Session(
                    "resident session state is invalidated; durable reload is required".to_string(),
                ));
            }
            ResidentSessionState::Valid => {}
        }
        self.session
            .as_ref()
            .map(|session| session.shared_tool_catalog(&self.state.session_id))
            .unwrap_or_else(|| Ok(Arc::new(Vec::new())))
    }

    pub fn tool_state(&self) -> Result<crate::ToolState, SessionError> {
        match self.resident_session.validity() {
            ResidentSessionState::Invalidated { decision_id } => {
                self.trace_synchronous_resident_state_refusal(decision_id, "tool_state");
                return Err(SessionError::Protocol(
                    "resident session state is invalidated; durable reload is required".to_string(),
                ));
            }
            ResidentSessionState::Valid => {}
        }
        let Some(session) = self.session.as_ref() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        Ok(session.plugins().tool_registry().export_state())
    }
    /// The durable protocol turn options recorded on the session.
    pub fn protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        self.state.effective_protocol_turn_options()
    }

    /// This is the initialization half of the FIG-2479 contract: protocol
    /// materialization hooks run before the session has a committed head, and
    /// [`Self::configure_protocol_on_materialize`] marks the resulting config
    /// dirty so `persist_materialized_protocol_config` publishes it durably
    /// before any queued command work. Mid-run changes never come through
    /// here — they use the commanded
    /// [`Self::set_protocol_turn_options`] path instead.
    #[expect(dead_code, reason = "retained during the runtime crate extraction")]
    pub(crate) fn record_materialized_protocol_turn_options(
        &mut self,
        options: crate::ProtocolTurnOptions,
    ) {
        self.state.protocol_turn_options = options;
    }

    /// `plugin_options` are the plugin-keyed options that reached this materialization
    /// (builder options for root opens, request options for child create); `is_root_session`
    /// distinguishes root from child.
    pub fn configure_protocol_on_materialize(
        &mut self,
        plugin_options: &crate::PluginOptions,
        is_root_session: bool,
    ) -> Result<(), crate::PluginError> {
        match self.resident_session.validity() {
            ResidentSessionState::Invalidated { decision_id } => {
                self.trace_synchronous_resident_state_refusal(
                    decision_id,
                    "configure_protocol_on_materialize",
                );
                return Err(crate::PluginError::Session(
                    "resident session state is invalidated; durable reload is required".to_string(),
                ));
            }
            ResidentSessionState::Valid => {}
        }
        let recorded_options = self.state.protocol_turn_options.payload.clone();
        let protocol_session = self
            .session
            .as_ref()
            .map(|session| Arc::clone(session.plugins().protocol_session()));
        if let Some(protocol_session) = protocol_session {
            let materialization = crate::plugin::ProtocolSessionMaterialization {
                plugin_options,
                is_root_session,
            };
            protocol_session
                .configure_runtime_on_materialize(
                    crate::plugin::ProtocolRuntimeContext::new(
                        &mut self.state.protocol_turn_options,
                    ),
                    materialization,
                )
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        }
        self.materialized_protocol_config_dirty |=
            self.state.protocol_turn_options.payload != recorded_options;
        self.state
            .open_unpersisted_initial_frame_under_settled_protocol_options();
        Ok(())
    }

    /// Export a snapshot of the current in-memory session state.
    /// This keeps persistence-heavy snapshots untouched; callers that need a
    /// fully persisted view should use `export_persisted_state`.
    pub fn export_state(&self) -> crate::SessionSnapshot {
        self.state.to_snapshot()
    }

    pub fn read_view(&self) -> Result<crate::SessionReadView, crate::SessionGraphScopeError> {
        crate::SessionReadView::from_runtime_state(
            &self.state,
            self.state.effective_policy().clone(),
            self.state.effective_protocol_turn_options().clone(),
        )
    }

    /// Export the narrow persistence snapshot used by stores and resume logic.
    pub fn export_persistence_state(&self) -> RuntimeSessionState {
        self.state.clone()
    }

    /// Replaces resident state WITHOUT durable publication; test and recovery
    /// tooling only, never a product path.
    #[cfg(any(test, feature = "testing"))]
    pub fn apply_persistence_state(
        &mut self,
        state: RuntimeSessionState,
    ) -> Result<(), SessionError> {
        self.set_persisted_state(state)
    }

    /// Export a persistence-ready state envelope with dynamic/plugin snapshots
    /// refreshed from the live session.
    pub async fn export_persisted_state(&mut self) -> Result<RuntimeSessionState, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        self.reapply_tool_state_preservation_marker();
        let mut state = self.state.clone();
        if let Some(session) = self.session.as_ref() {
            if !state.preserve_tool_state_snapshot {
                let snapshot = session.plugins().tool_registry().export_state();
                state.set_tool_state_snapshot(Some(snapshot));
            }
            state.capture_plugin_states(session.plugins());
        }
        Ok(state)
    }

    pub fn usage_report(&self) -> SessionUsageReport {
        let mut entries = self.state.token_ledger.clone();
        let drained = self.shared_token_ledger.lock_recover();
        let mut saturated = false;
        for entry in drained.iter().cloned() {
            saturated |= merge_ledger_entry_saturating(&mut entries, entry.entry);
        }
        SessionUsageReport::from_entries_with_saturation(&entries, saturated)
    }

    /// Attempts of finished turns whose usage never arrived after an abort or
    /// failure and have not been reconciled (ADR 0031). The ledger already
    /// carries them as unreported rows; this is their attribution.
    pub fn unreported_usage_attempts(&self) -> &[UnreportedUsageAttempt] {
        &self.unreported_usage_attempts
    }

    /// Ask the session's provider for the usage of every registered
    /// unreported attempt and append one `Reconciled` correction row per
    /// recovered generation (FIG-2765).
    ///
    /// Host-invoked and never on the turn hot path: each lookup is bounded by
    /// the provider (timeout plus one retry). Rows are append-only; the
    /// unreported row written at turn end is never rewritten, and
    /// [`UsageTotals::unreported_attempts`] derives the outstanding hole from
    /// both. Corrections ride the shared pending ledger and persist at the
    /// next usage-ledger boundary like live usage does. Attempts the provider
    /// cannot resolve stay registered and come back as `unresolved`.
    pub async fn reconcile_unreported_usage(
        &mut self,
    ) -> Result<UsageReconciliationReport, SessionError> {
        let mut report = UsageReconciliationReport::default();
        if self.unreported_usage_attempts.is_empty() {
            return Ok(report);
        }
        let session_id = self.state.session_id.clone();
        let policy = self.state.effective_policy().clone();
        let mut provider = self
            .host
            .resolve_session_policy(&session_id, policy)?
            .binding
            .provider;
        // Cancellation safety (FIG-2765): the registry is NOT drained up front.
        // Dropping this future mid-lookup must leave every unfinished attempt
        // registered, so we iterate a snapshot and remove each key only after
        // its correction is on the shared ledger, with no await in between.
        let pending = self.unreported_usage_attempts.clone();
        for attempt in pending {
            let Some(generation_id) = attempt.generation_id.as_deref() else {
                report.unresolved.push(attempt);
                continue;
            };
            match provider.reconcile_usage(generation_id).await {
                Ok(Some(reconciled)) => {
                    let crate::llm::types::LlmUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_write_input_tokens,
                        reasoning_output_tokens,
                    } = reconciled.usage;
                    let usage = TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_write_input_tokens,
                        reasoning_output_tokens,
                    };
                    session_manager::record_reconciled_usage_shared(
                        &self.shared_token_ledger,
                        &attempt.source,
                        &attempt.model,
                        &usage,
                        &attempt.call_id,
                        attempt.attempt_ordinal,
                    );
                    // Synchronous with the append above: no await may separate
                    // recording the correction from retiring the attempt.
                    self.unreported_usage_attempts.retain(|registered| {
                        registered.call_id != attempt.call_id
                            || registered.attempt_ordinal != attempt.attempt_ordinal
                    });
                    report.reconciled.push(ReconciledUsageAttempt {
                        attempt,
                        usage,
                        provider_usage: reconciled.provider_usage,
                    });
                }
                Ok(None) => report.unresolved.push(attempt),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        call_id = %attempt.call_id,
                        attempt_ordinal = attempt.attempt_ordinal,
                        generation_id,
                        error = %error,
                        "usage reconciliation lookup failed; attempt stays unreported"
                    );
                    report.unresolved.push(attempt);
                }
            }
        }
        Ok(report)
    }

    pub async fn await_background_work(&mut self) -> Result<(), SessionError> {
        if self.process_sync_needed.swap(false, Ordering::AcqRel) {
            self.refresh_session_graph_from_store().await?;
        }
        Ok(())
    }

    pub async fn refresh_session_graph_from_store(&mut self) -> Result<(), SessionError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            self.resident_session.mark_graph_head_current();
            return Ok(());
        };
        let requires_hydration = match store.load_session_head_meta().await {
            Ok(Some(head)) => {
                let moved = self.state.head_revision != head.head_revision
                    || head.leaf_node_id != self.state.session_graph.leaf_node_id
                    || head.checkpoint_ref != self.state.checkpoint_ref;
                // A recovery raise rewrites the pending follow-on without
                // moving the head revision (ADR 0101 §3), so the fact is
                // taken from the head even when nothing else moved.
                if !moved {
                    self.state.pending_follow_on = head.pending_follow_on.map(Box::new);
                }
                moved
            }
            Ok(None) => {
                if self.state.checkpoint_ref.is_some() {
                    return Err(SessionError::Store {
                        context: "failed to refresh session graph from store".to_string(),
                        source: crate::StoreError::SessionDeleted {
                            session_id: self.state.session_id.clone(),
                        },
                    });
                }
                self.resident_session.mark_graph_loaded();
                self.resident_session.mark_graph_head_current();
                return Ok(());
            }
            // The bounded read is an optimization. If it cannot determine the
            // durable head, retain the canonical full read rather than letting
            // probe failure report the resident graph as fresh.
            Err(_) => true,
        };
        if !requires_hydration {
            self.resident_session.mark_graph_loaded();
            self.resident_session.mark_graph_head_current();
            return Ok(());
        }
        let read = store.load_session().await.map_err(|err| {
            SessionError::Protocol(format!("failed to refresh session graph from store: {err}"))
        })?;
        self.resident_session.mark_graph_loaded();
        let Some(read) = read else {
            self.resident_session.mark_graph_head_current();
            return Ok(());
        };
        self.adopt_session_read(read).await
    }

    /// Adopt `base`, the head the running direct turn was admitted on, as the
    /// resident session (FIG-3682).
    ///
    /// A replay of an admitted turn rebuilds its input state from here, never
    /// from the live head: the turn's own commit, or a lane service, may have
    /// moved the live head since the turn was admitted, and a turn replayed on
    /// that head would issue other effects than its journal holds. A base the
    /// resident session already is needs no read. Revision zero is the session
    /// before any head existed. Any other base comes from
    /// [`load_session_at`](crate::store::SessionCommitStore::load_session_at),
    /// which refuses a head the store no longer retains.
    pub(in crate::runtime) async fn adopt_admission_base(
        &mut self,
        base: &crate::store::SessionHeadRef,
    ) -> Result<(), SessionError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(());
        };
        if self.state.head_revision == base.revision
            && self.state.session_graph.leaf_node_id == base.leaf
            && self.state.checkpoint_ref == base.checkpoint
        {
            return Ok(());
        }
        if base.revision == 0 {
            // No head existed when the turn was admitted: the resident session
            // is the one a runtime opens over an empty store, which has no
            // graph, frame, checkpoint or turn counters yet. Its first commit
            // opens the initial frame.
            let mut empty = self.state.clone();
            empty.session_graph = crate::SessionGraph::default();
            empty.agent_frames.clear();
            empty.current_frame_node_id = None;
            empty.pending_follow_on = None;
            empty.checkpoint_ref = None;
            empty.checkpoint_components =
                crate::RuntimeSessionState::new(empty.policy.clone()).checkpoint_components;
            empty.persisted_node_ids.clear();
            empty.head_revision = 0;
            empty.turn_index = 0;
            empty.token_usage = crate::TokenUsage::default();
            empty.last_prompt_usage = None;
            Box::pin(self.adopt_resident_state(empty)).await?;
            self.resident_session.mark_graph_loaded();
            self.resident_session.mark_graph_head_current();
            return Ok(());
        }
        let read = store
            .load_session_at(base)
            .await
            .map_err(|source| SessionError::Store {
                context: "failed to read the head the turn was admitted on".to_string(),
                source,
            })?;
        self.adopt_session_read(read).await
    }

    /// Adopt a durable session read as the resident session, head-authoritatively.
    async fn adopt_session_read(
        &mut self,
        read: crate::store::PersistedSessionRead,
    ) -> Result<(), SessionError> {
        // Defend refreshes against third-party stores that return an unvalidated resident graph.
        read.graph
            .validate_resident_integrity()
            .map_err(|source| SessionError::Store {
                context: "failed to refresh session graph from store".to_string(),
                source,
            })?;
        let head = crate::store::SessionHead {
            session_id: read.session_id.clone(),
            head_revision: read.head_revision,
            current_frame_node_id: read.current_frame_node_id.clone(),
            pending_follow_on: read.pending_follow_on.clone(),
            graph: read.graph,
            config: read.config.clone(),
            checkpoint_ref: read.checkpoint_ref.clone(),
            token_ledger: read.token_ledger,
        };
        // Head-authoritative adoption (FIG-1875): the durable head wins for
        // every fact it carries. Session config is durable (read + guarded
        // write, FIG-1555/FIG-1895), so the head already holds any committed
        // override; only the runtime-lease facts stay live-owned. The
        // provider resolver is also live-owned and is not part of the
        // durable head.
        let live_owned = crate::runtime::state::LiveOwnedSessionFacts::of(&self.state.policy);
        let mut adopted = self.state.clone();
        crate::runtime::state::adopt_durable_head(&mut adopted, &head, read.checkpoint, live_owned)
            .map_err(|source| SessionError::Store {
                context: "failed to restore session checkpoint".to_string(),
                source,
            })?;
        Box::pin(self.adopt_resident_state(adopted)).await?;
        self.resident_session.mark_graph_head_current();
        // The adopted head is authoritative for usage too: rebuild the attempts
        // this session still owes usage for from the durable rows plus the
        // resident rows that have not been confirmed into them yet.
        self.rehydrate_unreported_usage_attempts();
        Ok(())
    }

    /// Make `adopted` the resident session: its tool state, plugin state and
    /// protocol session (the code executor) are restored from it before it
    /// replaces `self.state`.
    ///
    /// A head adopted without its components leaves the executor on whatever
    /// head was open before, so the next turn runs its cells over another
    /// head's heap and commits an execution state no other execution of that
    /// turn produces (FIG-3684).
    ///
    /// Callers box this future: the restore is large, and every refresh and
    /// admitted-head adoption would otherwise inline it into the turn's stack
    /// (a 2 MiB tokio worker overflowed in lash-perf without the box).
    async fn adopt_resident_state(
        &mut self,
        mut adopted: crate::RuntimeSessionState,
    ) -> Result<(), SessionError> {
        let tracing = self.host.core.tracing.clone();
        let clock = Arc::clone(&self.host.core.clock);
        let tool_restore = Box::pin(self.restore_resident_session_components(
            &mut adopted,
            &tracing,
            clock.as_ref(),
        ))
        .await
        .map_err(|(_stage, error)| {
            SessionError::Protocol(format!(
                "failed to restore the adopted session head: {error}"
            ))
        })?;
        self.state = adopted;
        self.reapply_tool_state_preservation_marker();
        self.publish_plugin_tool_access();
        if tool_restore.is_some() {
            self.tool_restore_report = tool_restore;
        }
        Ok(())
    }

    /// Rebuild the pending-attempt registry from durable ledger rows and the
    /// unconfirmed resident rows layered on top. Confirmed resident rows are
    /// already in `state.token_ledger`, and rebuilding by identity rather than
    /// by count means seeing a hole twice cannot double-count it.
    pub(in crate::runtime) fn rehydrate_unreported_usage_attempts(&mut self) {
        let mut entries = self.state.token_ledger.clone();
        for pending in self.shared_token_ledger.lock_recover().iter() {
            entries.push(pending.entry.clone());
        }
        self.unreported_usage_attempts = crate::runtime::outstanding_unreported_attempts(&entries);
    }

    pub fn runtime_session_services(
        &self,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        match self.resident_session.validity() {
            ResidentSessionState::Invalidated { decision_id } => {
                self.trace_synchronous_resident_state_refusal(
                    decision_id,
                    "runtime_session_services",
                );
                return Err(PluginOperationInvokeError::Unknown(
                    "resident session state is invalidated; durable reload is required".to_string(),
                ));
            }
            ResidentSessionState::Valid => {}
        }
        Ok(Arc::new(RuntimeSessionServices::new(self, true, None)?))
    }

    /// This session's tool-execution context for a group tool child whose
    /// opener is not live where the child runs (FIG-3712): what a deployment's
    /// [`ToolChildContextSource`](crate::facade_support::ToolChildContextSource)
    /// builds a child's context from. `lent_controller` fills the controller
    /// slots the tool-child driver's rebind replaces.
    pub fn tool_child_dispatch(
        &self,
        lent_controller: crate::ScopedEffectController<'static>,
    ) -> Result<crate::tool_dispatch::ToolDispatchContext<'static>, crate::PluginError> {
        self.runtime_session_services()
            .map_err(|error| crate::PluginError::Session(error.to_string()))?
            .tool_child_dispatch(lent_controller)
    }

    pub(super) fn runtime_session_services_for_turn(
        &self,
        held_session_execution_lease: Option<&SessionExecutionLeaseGuard>,
        turn_graph_appends: &TurnGraphAppendDraft,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::for_turn(
            self,
            held_session_execution_lease,
            turn_graph_appends,
        )?))
    }

    pub(super) fn runtime_session_services_after_commit(
        &self,
        held_session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::new(
            self,
            true,
            held_session_execution_lease,
        )?))
    }

    pub fn session_state_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionStateService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.state_service())
    }

    pub fn session_lifecycle_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionLifecycleService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.lifecycle_service())
    }

    /// Returns a lane-less host service for calls between turn drivers, never concurrently with a running turn.
    pub fn session_graph_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionGraphService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.graph_service())
    }

    pub fn process_service(
        &self,
    ) -> Result<Arc<dyn crate::ProcessService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.process_service())
    }

    pub fn effect_host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.host.core.control.effect_host)
    }

    pub async fn enqueue_turn_input(
        &self,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
    ) -> Result<crate::PendingTurnInput, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(queued_turn_input_store_required)?;
        enqueue_turn_input_to_store(
            self.state.session_id.clone(),
            store,
            Arc::clone(self.host.queued_work()),
            input,
            ingress,
            source_key,
        )
        .await
    }

    pub async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(queued_turn_input_store_required)?;
        store
            .cancel_queued_work_batch(session_id, batch_id)
            .await
            .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))
    }

    /// The plugin session bound to the currently active runtime session, if any.
    pub fn plugin_session(&self) -> Option<Arc<crate::PluginSession>> {
        match self.resident_session.validity() {
            ResidentSessionState::Invalidated { decision_id } => {
                self.trace_synchronous_resident_state_refusal(decision_id, "plugin_session");
                return None;
            }
            ResidentSessionState::Valid => {}
        }
        self.session.as_ref().map(|s| Arc::clone(s.plugins()))
    }

    /// Open a new Agent Frame, or replay the current one idempotently.
    ///
    /// Refuses with
    /// [`RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported`] when the
    /// key names a persisted frame that is not current: making it resident
    /// would replace session configuration without a commanded config patch.
    pub async fn open_agent_frame(
        &mut self,
        request: crate::OpenAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameResult, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        // A pending follow-on owns the session's frame until its turn commits
        // (ADR 0101 §3); the store's frame invariant is the backstop.
        if let Some(pending) = self.state.pending_follow_on.as_ref() {
            return Err(super::runtime_error_from_store_commit(
                pending.pending_error(&self.state.session_id),
            ));
        }
        open_agent_frame_in_state_with_clock(
            &mut self.state,
            request,
            self.host.core.clock.as_ref(),
        )
    }

    pub async fn compact_context(
        &mut self,
        instructions: Option<String>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
    ) -> Result<bool, PluginOperationInvokeError> {
        self.reload_invalidated_resident_session_state()
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let services = self.runtime_session_services()?;
        let compaction_boundary = scoped_effect_controller.scope_id().to_string();
        let Some(session) = self.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        let plugin_session = Arc::clone(session.plugins());
        let state = self
            .read_view()
            .map_err(|error| PluginOperationInvokeError::Unknown(error.to_string()))?;
        let system_prompt = Self::compaction_system_prompt(
            session.context_prompt_contributions().to_vec(),
            Arc::clone(&plugin_session),
            Arc::clone(&services),
            self.state.session_id.clone(),
            state.clone(),
            self.protocol_turn_options().clone(),
            self.host.core.prompt.prompt.clone(),
            self.state.effective_policy().prompt.clone(),
        )
        .await?;
        let ctx = crate::CompactionContext {
            session_id: self.state.session_id.clone(),
            state,
            instructions,
            system_prompt,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            direct_completions: services.direct_completion_client(
                crate::runtime::RuntimeEffectControllerHandle::Borrowed(
                    scoped_effect_controller.clone(),
                ),
                None,
            ),
        };
        let outcome = async {
            let Some(compaction) = plugin_session.compact_context(&ctx).await.map_err(|err| {
                PluginOperationInvokeError::Unknown(format!("context compaction failed: {err}"))
            })?
            else {
                return Ok(false);
            };
            let frame_key = compaction_frame_key(
                &self.state.session_id,
                &compaction_boundary,
                self.state
                    .current_frame_node_id
                    .as_deref()
                    .unwrap_or_default(),
            );
            let result = self
                .open_agent_frame(
                    crate::OpenAgentFrameRequest::new(
                        frame_key,
                        crate::AgentFrameReason::compaction(),
                    )
                    .with_initial_nodes(compaction.initial_nodes),
                )
                .await
                .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
            if result.opened {
                self.stamp_live_plugin_state();
            }
            Ok(result.opened)
        }
        .await;
        // Usage settlement runs on every exit past the reload gate, not only
        // a successful frame switch: a compaction that produced no summary or
        // failed outright can still have staged billed usage into the shared
        // ledger, and this boundary is the only place it persists.
        let settlement = Box::pin(self.settle_pending_compaction_usage()).await;
        match (outcome, settlement) {
            (Ok(opened), Ok(())) => Ok(opened),
            (Ok(_), Err(err)) | (Err(err), Ok(())) => Err(err),
            (Err(err), Err(settle_err)) => Err(PluginOperationInvokeError::Unknown(format!(
                "{err}; usage settlement also failed: {settle_err}"
            ))),
        }
    }

    /// Renders the system prompt a compaction completion carries (`FIG-3374`).
    ///
    /// A turn resolves capability contributions, the core layer, the session
    /// layer, and the turn layer (`turn_driver/tool_catalog.rs`
    /// `build_prompt`). Compaction is one direct completion, not a turn: it
    /// resolves the same stack minus the turn layer, with an empty execution
    /// prompt and an empty tool list, so every tool-gated contribution drops
    /// — the request ships no tools and could never honor them. Plugin prompt
    /// hooks see the session's current read view, and a failing hook fails
    /// the compaction exactly as it would fail a turn's prompt build.
    #[allow(
        clippy::too_many_arguments,
        reason = "prompt assembly needs each resolved layer and the hook context pieces as explicit owned inputs so the recovery path can defer them into a 'static provider"
    )]
    pub(crate) async fn compaction_system_prompt(
        context_contributions: Vec<crate::PromptContribution>,
        plugin_session: Arc<crate::PluginSession>,
        services: Arc<RuntimeSessionServices>,
        session_id: crate::SessionId,
        state: crate::SessionReadView,
        protocol_turn_options: crate::ProtocolTurnOptions,
        core_prompt: crate::PromptLayer,
        policy_prompt: crate::PromptLayer,
    ) -> Result<Option<Arc<str>>, PluginOperationInvokeError> {
        let mut capability_prompt = crate::PromptLayer::new();
        for contribution in context_contributions {
            capability_prompt.add_contribution(contribution);
        }
        for contribution in plugin_session
            .collect_prompt_contributions(crate::PromptHookContext {
                session_id,
                sessions: services.state_service(),
                state,
                protocol_turn_options,
                turn_context: crate::TurnContext::new(),
            })
            .await
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?
        {
            capability_prompt.add_contribution(contribution);
        }
        let resolved =
            crate::resolve_prompt_layers([&capability_prompt, &core_prompt, &policy_prompt]);
        let contributions = resolved
            .contributions
            .into_iter()
            .filter(|contribution| contribution.gate.is_empty())
            .collect();
        let rendered = lash_sansio::build_prompt(crate::PromptBuildInput {
            template_fingerprint: crate::prompt_template_fingerprint(&resolved.template),
            template: resolved.template,
            execution_prompt_fingerprint: crate::prompt_text_fingerprint(""),
            execution_prompt: Arc::from(""),
            tool_names_fingerprint: lash_sansio::prompt_tool_names_fingerprint(&[]),
            tool_names: Arc::new(Vec::new()),
            contributions: lash_sansio::PromptContributionSet::new(contributions),
        });
        let system_prompt = rendered.system_prompt.trim();
        Ok((!system_prompt.is_empty()).then(|| Arc::from(system_prompt)))
    }

    /// Persists pending graph nodes and staged usage an administrative
    /// compaction left behind (`FIG-3374`).
    ///
    /// Administrative compaction has no owning turn to settle usage at a
    /// commit (`direct_outcome.rs` stages into the shared ledger only), so
    /// `compact_context` runs this on every exit past the reload gate —
    /// including the no-summary and error paths. Mirrors `park()`: pending
    /// state persists at this explicit boundary with the same content-derived
    /// operation, so a retried compact_context reuses byte-identical row
    /// identities.
    async fn settle_pending_compaction_usage(&mut self) -> Result<(), PluginOperationInvokeError> {
        let Some(store) = self.services.store.clone() else {
            return Ok(());
        };
        let pending_usage = self
            .shared_token_ledger
            .lock_recover()
            .iter()
            .map(|pending| pending.entry.clone())
            .collect::<Vec<_>>();
        if self.state.pending_graph_commit().nodes().is_empty() && pending_usage.is_empty() {
            return Ok(());
        }
        let proposed = super::lifecycle::initial_park_preview(
            &self.state,
            &pending_usage,
            self.host.core.durability.commit_budget,
        )
        .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let operation = super::lifecycle::initial_park_operation(&proposed)
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let staged =
            session_manager::stage_token_ledger_shared(&self.shared_token_ledger, &operation)
                .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        for delta in staged.deltas() {
            crate::store::merge_token_ledger_entry_checked(
                &mut self.state.token_ledger,
                delta.entry.clone(),
            )
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        }
        let (commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_staged_usage_and_budget(
                &mut self.state,
                staged.deltas(),
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let commit_result = commit_runtime_state_with_fresh_session_execution_lease(
            store,
            commit,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await;
        let commit_result = match commit_result {
            Ok(result) => result,
            Err(err) => {
                // The frame switch and the staged usage merge above live only
                // in resident state until this commit lands. On failure,
                // discard them and reload the durable head: the facade must
                // not publish a mutation that never persisted. The staged
                // pending rows are discarded too — the journaled completion
                // effect re-records the billed usage when a retry replays it,
                // so retaining them would double-merge the same usage.
                staged.discard_staged();
                self.invalidate_resident_session_state();
                if let Err(reload_err) = self.reload_invalidated_resident_session_state().await {
                    return Err(PluginOperationInvokeError::Unknown(format!(
                        "{err}; resident-state reload after commit failure also failed: \
                         {reload_err}"
                    )));
                }
                return Err(PluginOperationInvokeError::Unknown(err.to_string()));
            }
        };
        let confirmed_usage = commit_result.committed_usage_delta_identities.clone();
        staged
            .confirm_identities(&confirmed_usage)
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        self.state.apply_persisted_commit_result(commit_result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        Ok(())
    }

    pub fn session_policy(&self) -> SessionPolicy {
        self.state.effective_policy().clone()
    }

    pub(super) async fn notify_session_config_changed(
        &self,
        previous: SessionPolicy,
    ) -> Result<(), crate::PluginError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let current = self.session_policy();
        if current == previous {
            return Ok(());
        }
        let Ok(services) = self.runtime_session_services() else {
            return Ok(());
        };
        session
            .plugins()
            .emit_runtime_event(crate::PluginLifecycleEvent::SessionConfigChanged(Box::new(
                SessionConfigChangedContext {
                    session_id: self.state.session_id.clone(),
                    previous,
                    current,
                    sessions: services.state_service(),
                },
            )))
            .await
    }

    pub(super) async fn resolve_session_config_mutations(
        &self,
        previous: SessionPolicy,
        candidate: SessionPolicy,
    ) -> SessionPolicy {
        let Some(session) = self.session.as_ref() else {
            return candidate;
        };
        if candidate == previous {
            return candidate;
        }
        let Ok(services) = self.runtime_session_services() else {
            return candidate;
        };
        session
            .plugins()
            .mutate_session_config(
                SessionConfigChangedContext {
                    session_id: self.state.session_id.clone(),
                    previous,
                    current: candidate.clone(),
                    sessions: services.state_service(),
                },
                candidate,
            )
            .await
    }
}

pub(in crate::runtime) async fn enqueue_turn_input_to_store(
    session_id: SessionId,
    store: Arc<dyn crate::RuntimePersistence>,
    queued_work: Arc<dyn crate::SessionWorkEngine>,
    input: crate::TurnInput,
    ingress: crate::TurnInputIngress,
    source_key: Option<String>,
) -> Result<crate::PendingTurnInput, RuntimeError> {
    super::turn_loop::ensure_durable_effect_input(&input)?;
    let is_next_turn = matches!(ingress, crate::TurnInputIngress::NextTurn);
    let mut draft = crate::PendingTurnInputDraft::new(session_id, ingress, input);
    draft.source_key = source_key;
    store
        .read_session_state_version()
        .await
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()))?;
    let enqueued = store
        .enqueue_pending_turn_input(draft)
        .await
        .map_err(super::error::runtime_error_from_turn_input_admission)?;
    if is_next_turn {
        // One drive request per accepted row (FIG-3600): an engine dedupes a
        // repeated ask for the same row, and a drive admits whatever else is
        // pending too.
        queued_work.schedule_drive(
            &enqueued.session_id,
            crate::engine::DriveRequestId::new(enqueued.input_id.to_string()),
        );
    }
    Ok(enqueued)
}

/// The first re-ask of a settlement wait whose session another drive holds;
/// each later one doubles, up to [`SETTLEMENT_REASK_MAX_SHIFT`] doublings.
const SETTLEMENT_REASK_BASE_MS: u64 = 10;
const SETTLEMENT_REASK_MAX_SHIFT: u32 = 7;

enum AcceptedSessionCommand {
    Inline(crate::SessionCommandReceipt),
    Queued(crate::runtime::SessionCommandSettlementHandle),
}

impl LashRuntime {
    async fn accept_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<AcceptedSessionCommand, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let idempotency_key = idempotency_key.into();
        if idempotency_key.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandIdempotencyKey,
                "session command idempotency key cannot be empty",
            ));
        }
        self.refuse_unservable_route(&command)?;
        let source_key = command.source_key(&idempotency_key);
        let session_id = self.state.session_id.clone();
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            let receipt = crate::SessionCommandReceipt {
                session_id,
                batch_id: crate::BatchId::new(format!("inline-command:{}", uuid::Uuid::new_v4())),
                source_key,
            };
            self.apply_session_command_after_admission(vec![command], None, None)
                .await?;
            return Ok(AcceptedSessionCommand::Inline(receipt));
        };
        self.persist_materialized_protocol_config()
            .await
            .map_err(runtime_error_from_session_command_refresh)?;
        let draft = crate::QueuedWorkBatchDraft::new(
            session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            command,
        )
        .with_source_key(source_key.clone());
        let enqueued = store.enqueue_queued_work(draft).await.map_err(|err| {
            RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string())
        })?;
        Ok(AcceptedSessionCommand::Queued(
            crate::runtime::SessionCommandSettlementHandle {
                receipt: crate::SessionCommandReceipt {
                    session_id,
                    batch_id: enqueued.batch_id,
                    source_key,
                },
            },
        ))
    }

    pub(super) async fn submit_apply_config_patch(
        &mut self,
        patch: super::ApplyConfigPatch,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        Box::pin(self.submit_apply_config_patch_with_idempotency_key(
            patch,
            format!("config-patch:{}", uuid::Uuid::new_v4()),
        ))
        .await
    }

    pub async fn submit_apply_config_patch_with_idempotency_key(
        &mut self,
        patch: super::ApplyConfigPatch,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        let accepted = match self
            .accept_session_command(
                crate::SessionCommand::ApplyConfigPatch {
                    patch: Box::new(patch),
                },
                idempotency_key,
            )
            .await
        {
            Ok(accepted) => accepted,
            Err(rejection) => {
                return Ok(crate::runtime::SessionCommandSettlement::Rejected(
                    rejection,
                ));
            }
        };
        match accepted {
            AcceptedSessionCommand::Inline(receipt) => {
                Ok(crate::runtime::SessionCommandSettlement::Durable(receipt))
            }
            AcceptedSessionCommand::Queued(handle) => {
                self.await_session_command_settlement(handle).await
            }
        }
    }

    async fn await_session_command_settlement(
        &mut self,
        handle: crate::runtime::SessionCommandSettlementHandle,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "accepted session command lost its persistent store",
                )
            })?;
        // Session-command settlement is a control-plane wait. Reuse the
        // host-configured lease TTL as its deadline: the default is the same
        // 30-second operational window, and hosts that tighten durable-control
        // timings through `with_lease_timings` tighten this wait as well.
        let settlement_timeout = self.host.core.control.lease_timings.ttl();
        let settlement_started = self.host.core.clock.now();
        let mut asks = 0_u32;
        let mut next_ask = std::time::Duration::ZERO;
        loop {
            let still_pending = store
                .list_queued_work(&handle.receipt.session_id)
                .await
                .map_err(super::runtime_error_from_store_commit)?
                .iter()
                .any(|batch| batch.batch_id == handle.receipt.batch_id);
            if !still_pending {
                let completed = store
                    .queued_work_batch_completed(
                        &handle.receipt.session_id,
                        &handle.receipt.batch_id,
                    )
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
                if !completed {
                    return Ok(crate::runtime::SessionCommandSettlement::Cancelled(
                        handle.receipt,
                    ));
                }
                self.refresh_session_graph_from_store()
                    .await
                    .map_err(runtime_error_from_session_command_refresh)?;
                // The refresh adopts the durable head, which already carries
                // this command's committed values — or newer ones from a
                // later writer (head-authoritative adoption, FIG-1875). Every
                // successful refresh path either confirms the resident state
                // already carries the drain commit or fully hydrates the
                // head, and probe failures propagate as errors, so there is
                // no edge that needs the patch re-published residently.
                // Reapplying it here would overwrite a newer settled head
                // with this command's older values, resident-only.
                return Ok(crate::runtime::SessionCommandSettlement::Durable(
                    handle.receipt,
                ));
            }

            if self
                .host
                .core
                .clock
                .now()
                .saturating_duration_since(settlement_started)
                >= settlement_timeout
            {
                return Ok(crate::runtime::SessionCommandSettlement::Pending(
                    handle.receipt,
                ));
            }

            let lease = super::session_execution_lease::SessionExecutionLeaseGuard::try_acquire_for_executor(
                Arc::clone(&store),
                &self.state.session_id,
                &self.runtime_lease_owner,
                &self.runtime_lease_executor_id,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
            if let Some(lease) = lease {
                let fence = lease.fence();
                while self.drain_next_session_command(&fence).await?.is_some() {
                    let target_pending = store
                        .list_queued_work(&handle.receipt.session_id)
                        .await
                        .map_err(super::runtime_error_from_store_commit)?
                        .iter()
                        .any(|batch| batch.batch_id == handle.receipt.batch_id);
                    if !target_pending {
                        break;
                    }
                }
                lease
                    .release_if_live()
                    .await
                    .map_err(super::runtime_error_from_store_commit)?;
            } else if self
                .host
                .core
                .clock
                .now()
                .saturating_duration_since(settlement_started)
                >= next_ask
            {
                // Another drive holds the session. Ask for a drive again,
                // under an id no earlier ask used: an engine dedupes a
                // repeated id, and a drive already past its last admission
                // would swallow it. The asks back off, so a long drive is
                // not flooded (review-2290 MEDIUM-9).
                asks = asks.saturating_add(1);
                self.host.queued_work().schedule_drive(
                    &handle.receipt.session_id,
                    crate::engine::DriveRequestId::new(format!(
                        "{}#settle-{asks}",
                        handle.receipt.batch_id
                    )),
                );
                next_ask = self
                    .host
                    .core
                    .clock
                    .now()
                    .saturating_duration_since(settlement_started)
                    .saturating_add(std::time::Duration::from_millis(
                        SETTLEMENT_REASK_BASE_MS << asks.min(SETTLEMENT_REASK_MAX_SHIFT),
                    ));
            }
            let remaining = settlement_timeout.saturating_sub(
                self.host
                    .core
                    .clock
                    .now()
                    .saturating_duration_since(settlement_started),
            );
            self.host
                .core
                .clock
                .sleep(remaining.min(std::time::Duration::from_millis(10)))
                .await;
        }
    }

    /// Submit `command` to the session's command lane and return as soon as
    /// it is durable: **before** it is applied (FIG-3600). The session's
    /// drive applies it at its next turn boundary, in order; its outcome is
    /// observed with [`Self::settle_session_command`].
    ///
    /// The resident session state is marked stale: the drive commits the
    /// command over the durable head, so the next use of this runtime reloads
    /// the head instead of committing over a pre-command copy.
    pub async fn submit_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::SessionCommandReceipt, RuntimeError> {
        let accepted = self
            .accept_session_command(command, idempotency_key)
            .await?;
        let receipt = match accepted {
            AcceptedSessionCommand::Inline(receipt) => return Ok(receipt),
            AcceptedSessionCommand::Queued(handle) => handle.receipt,
        };
        self.host.queued_work().schedule_drive(
            &receipt.session_id,
            crate::engine::DriveRequestId::new(receipt.batch_id.to_string()),
        );
        self.invalidate_resident_session_state();
        Ok(receipt)
    }

    /// Wait for the command `receipt` names to settle, and adopt the durable
    /// head it settled on. `Pending` when it has not settled within the
    /// host's lease TTL; the command stays durable and settles later.
    pub async fn settle_session_command(
        &mut self,
        receipt: crate::SessionCommandReceipt,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        self.await_session_command_settlement(crate::runtime::SessionCommandSettlementHandle {
            receipt,
        })
        .await
    }

    pub async fn drain_next_session_command(
        &mut self,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        if self
            .session
            .as_ref()
            .and_then(Session::history_store)
            .is_none()
        {
            self.reload_invalidated_resident_session_state().await?;
            return Ok(None);
        }
        let host = self.effect_host();
        // The command commit keeps its claimed batch's existing operation identity.
        let controller = host.scoped(crate::AdmittedScope::queue_drain(
            &self.state.session_id,
            "session-command",
        ))?;
        self.drain_next_session_command_with_cancellation(
            session_execution_lease,
            tokio_util::sync::CancellationToken::new(),
            controller.controller(),
        )
        .await
    }

    pub async fn drain_next_session_command_with_cancellation(
        &mut self,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(None);
        };
        let claim = store
            .claim_leading_ready_session_command(
                &self.state.session_id,
                session_execution_lease,
                &self.runtime_lease_owner,
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(claim) = claim else {
            return Ok(None);
        };
        let Some(commands) = claim.session_commands() else {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandClaim,
                format!(
                    "queued-work claim `{}` did not contain only single-command control batches",
                    claim.claim_id
                ),
            ));
        };
        let receipts = commands
            .iter()
            .map(|(batch, _)| {
                let batch_id = batch.batch_id.clone();
                crate::SessionCommandReceipt {
                    session_id: self.state.session_id.clone(),
                    source_key: batch
                        .source_key
                        .clone()
                        .unwrap_or_else(|| batch_id.to_string()),
                    batch_id,
                }
            })
            .collect::<Vec<_>>();
        let commands = commands
            .into_iter()
            .map(|(_, command)| command.clone())
            .collect::<Vec<_>>();
        self.apply_session_command(
            commands,
            Some(claim.completion()),
            Some(session_execution_lease),
            cancellation,
            effect_controller,
        )
        .await?;
        Ok(receipts.into_iter().next())
    }

    async fn apply_session_command(
        &mut self,
        commands: Vec<crate::SessionCommand>,
        completion: Option<crate::QueuedWorkCompletion>,
        session_execution_lease: Option<&crate::SessionExecutionLeaseAuthority>,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<(), RuntimeError> {
        let has_durable_store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .is_some();
        if !has_durable_store
            || !super::commit_admission::requires_local_commit_admission(effect_controller)
        {
            return self
                .apply_session_command_after_admission(
                    commands,
                    completion,
                    session_execution_lease,
                )
                .await;
        }
        let session_id = self.state.session_id.clone();
        let work_identity = completion
            .as_ref()
            .map(|completion| completion.claim_id.clone())
            .unwrap_or_else(|| "inline-session-command".to_string());
        let result: Result<(), RuntimeCommitAdmissionError> =
            super::run_head_advancing_commit_attempt(
                session_id.clone(),
                work_identity.clone(),
                cancellation,
                move |waited, queue_depth| async move {
                    super::commit_admission::record_product_commit_admission(
                        "session_command_commit",
                        &session_id,
                        &work_identity,
                        waited,
                        queue_depth,
                    );
                    let _product_commit_phase = super::RuntimeNamedPhase::begin(
                        self.turn_phase_probe.clone(),
                        "commit_admission.product_attempt",
                    );
                    self.apply_session_command_after_admission(
                        commands,
                        completion,
                        session_execution_lease,
                    )
                    .await
                    .map_err(RuntimeCommitAdmissionError)
                },
            )
            .await;
        result.map_err(|error| error.0)
    }

    async fn apply_session_command_after_admission(
        &mut self,
        commands: Vec<crate::SessionCommand>,
        completion: Option<crate::QueuedWorkCompletion>,
        session_execution_lease: Option<&crate::SessionExecutionLeaseAuthority>,
    ) -> Result<(), RuntimeError> {
        self.refresh_session_graph_from_store()
            .await
            .map_err(|err| {
                RuntimeError::new(
                    crate::RuntimeErrorCode::SessionCommandRefresh,
                    err.to_string(),
                )
            })?;
        let config_only = commands
            .iter()
            .all(|command| matches!(command, crate::SessionCommand::ApplyConfigPatch { .. }));
        let mut next_config_state = config_only.then(|| self.state.clone());
        if let Some(next_state) = next_config_state.as_mut() {
            for command in &commands {
                let crate::SessionCommand::ApplyConfigPatch { patch } = command else {
                    unreachable!("config-only command group was checked above")
                };
                patch.validate()?;
                if self.refuses_route_at_apply(patch, next_state.effective_policy()) {
                    continue;
                }
                if patch.apply_to_state(next_state).is_err() {
                    // A stale base is a silent no-op that still settles
                    // completed: the old queued-work tables cannot carry a
                    // typed stale outcome; the ingress drain's planner can
                    // (ADR 0101 §12, Q8).
                }
            }
        } else {
            debug_assert_eq!(commands.len(), 1, "non-config commands remain exclusive");
            for command in commands {
                match command {
                    crate::SessionCommand::RefreshToolCatalog { .. } => {
                        self.refresh_session_tool_catalog().await.map_err(|err| {
                            RuntimeError::new(
                                crate::RuntimeErrorCode::SessionCommandRefreshTools,
                                err.to_string(),
                            )
                        })?;
                    }
                    crate::SessionCommand::ApplyConfigPatch { .. } => {
                        unreachable!("config commands use the cloned publication path")
                    }
                }
            }
        }
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            if let Some(next_state) = next_config_state {
                self.state = next_state;
                self.publish_plugin_tool_access();
            }
            return Ok(());
        };
        let operation = completion
            .as_ref()
            .and_then(|completion| completion.batch_ids.first())
            .map(|batch_id| {
                let state = next_config_state.as_ref().unwrap_or(&self.state);
                crate::OperationId::new(state.queue_drain_scope(batch_id), "session-command")
            })
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "persisted session commands require a claimed queue boundary",
                )
            })?;
        let commit_state = next_config_state.as_mut().unwrap_or(&mut self.state);
        if let Some(session) = self.session.as_ref() {
            commit_state.capture_plugin_states(session.plugins());
        }
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                commit_state,
                &[],
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(session_execution_lease) = session_execution_lease else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "session command commit requires a session execution lease",
            ));
        };
        commit.session_execution_lease_fence = Some(session_execution_lease.clone());
        if let Some(completion) = completion {
            commit = commit.completing_queue_claim(completion);
        }
        let result = crate::store::commit_runtime_state_verified(store.as_ref(), commit)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        commit_state.apply_persisted_commit_result(result);
        commit_state.mark_node_ids_persisted(persisted_node_ids);
        if let Some(next_state) = next_config_state {
            self.state = next_state;
            self.publish_plugin_tool_access();
        }
        Ok(())
    }
}

struct RuntimeCommitAdmissionError(RuntimeError);

impl From<crate::StoreError> for RuntimeCommitAdmissionError {
    fn from(error: crate::StoreError) -> Self {
        Self(super::runtime_error_from_store_commit(error))
    }
}

fn runtime_error_from_session_command_refresh(error: SessionError) -> RuntimeError {
    let deleted_session_id = match &error {
        SessionError::Store {
            source: crate::StoreError::SessionDeleted { session_id },
            ..
        } => Some(session_id.clone()),
        _ => None,
    };
    let runtime_error = RuntimeError::new(
        RuntimeErrorCode::SessionCommandPostDriveRefresh,
        error.to_string(),
    );
    match deleted_session_id {
        Some(session_id) => {
            runtime_error.with_cause(crate::RuntimeErrorCause::SessionDeleted { session_id })
        }
        None => runtime_error,
    }
}

fn compaction_frame_key(
    session_id: &SessionId,
    boundary_id: &str,
    previous_frame_node_id: &str,
) -> crate::FrameKey {
    crate::FrameKey::from_compaction_material(session_id, boundary_id, previous_frame_node_id)
}

pub(in crate::runtime) fn queued_turn_input_store_required() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::StoreCommitFailed,
        "queued turn input requires a persistent runtime store",
    )
}

#[cfg(test)]
mod tests {
    use super::compaction_frame_key;
    use crate::SessionId;

    #[test]
    fn compaction_frame_identity_is_replay_stable() {
        let first = compaction_frame_key(&SessionId::from("session"), "turn", "frame-before");
        let replay = compaction_frame_key(&SessionId::from("session"), "turn", "frame-before");
        let next = compaction_frame_key(&SessionId::from("session"), "turn", "frame-after");

        assert_eq!(first, replay);
        assert_ne!(first, next);
    }
}
