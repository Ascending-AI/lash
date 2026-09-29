use super::*;
use crate::SessionId;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::sync::MutexExt;

impl LashRuntime {
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    /// The resident runtime state, read-only. Writes go through the install
    /// helpers, which publish the resident authority (FIG-4024).
    pub fn state(&self) -> &RuntimeSessionState {
        &self.state
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

    /// Make `state` the resident runtime state. Every whole-state swap goes
    /// through here (durable adoption and reload, append rollback and receipt
    /// replay, settled config commands, turn commits, session creation), so
    /// none can skip what a replacement owes the live session:
    ///
    /// - the replacement rebuilds the marker field, so the per-open
    ///   `PreservePersisted` claim is reasserted from host configuration
    ///   before any later stamp consults it (FIG-3353);
    /// - the whole resident authority, tool access and subagent context (the
    ///   two inputs of the plugin catalog projection), is published to the
    ///   live plugin session, invalidating discovery caches only when it
    ///   changed, so live discovery always reflects the settled authority
    ///   (FIG-2415, FIG-2987).
    pub(in crate::runtime) fn install_resident_state(&mut self, state: crate::RuntimeSessionState) {
        self.state = state;
        self.reapply_tool_state_preservation_marker();
        self.publish_resident_authority();
    }

    /// Install a root's recorded [`ResolvedRun`](crate::ResolvedRun) as the
    /// resident execution view. The view's config carries its own authority,
    /// which can differ from the resident one (a replay after a config
    /// change, a recovered follow-on's inherited shape, a refresh the root
    /// re-installs its record over), so the install publishes it to the live
    /// plugin session just as a whole-state swap does (FIG-4022). This and
    /// [`Self::install_resident_state`] are the only writers of the resident
    /// authority.
    pub(in crate::runtime) fn install_resolved_run(&mut self, resolved: &crate::ResolvedRun) {
        crate::runtime::state::adopt_resolved_run(&mut self.state, resolved);
        self.publish_resident_authority();
    }

    /// Test hook: applies `edit` to a copy of the resident state and installs
    /// it through [`Self::install_resident_state`], so the edited authority is
    /// published to the live plugin session. No durable publication and no
    /// plugin or tool-state hydration.
    #[cfg(any(test, feature = "testing"))]
    pub fn edit_resident_state_for_test(&mut self, edit: impl FnOnce(&mut RuntimeSessionState)) {
        let mut state = self.state.clone();
        edit(&mut state);
        self.install_resident_state(state);
    }

    /// Publish the resident authority to the live plugin session.
    fn publish_resident_authority(&self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session.plugins().replace_authority(
            &self.state.authority.tool_access,
            self.state.authority.subagent.as_ref(),
        ) {
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
        let fleet_format = self.fleet_format();
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
                        fleet_format,
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

    pub fn read_view(&self) -> crate::SessionReadView {
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
        let mut totals = self.state.usage.clone();
        let drained = self.shared_token_ledger.lock_recover();
        let mut saturated = false;
        for entry in drained.iter() {
            saturated |= totals.fold_saturating(&entry.entry);
        }
        let mut report = totals.report();
        report.saturated |= saturated;
        report
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
        let read = store
            .load_session_window(crate::store::WindowSelector::Current)
            .await
            .map_err(|err| {
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
    /// before any head existed. Any other base is the window
    /// [`WindowSelector::Admitted`](crate::store::WindowSelector::Admitted)
    /// reads at the admitted leaf (ADR 0112 §5), which refuses a head the
    /// store no longer retains.
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
            .load_session_window(crate::store::WindowSelector::Admitted(base.clone()))
            .await
            .and_then(|read| {
                read.ok_or(crate::StoreError::TurnBaseNotRetained {
                    revision: base.revision,
                })
            })
            .map_err(|source| SessionError::Store {
                context: "failed to read the head the turn was admitted on".to_string(),
                source,
            })?;
        self.adopt_session_read(read).await
    }

    /// Adopt a durable window read as the resident session,
    /// head-authoritatively. The resident graph becomes the window (ADR 0112
    /// §9).
    async fn adopt_session_read(
        &mut self,
        read: crate::store::SessionWindowRead,
    ) -> Result<(), SessionError> {
        // Head-authoritative adoption (FIG-1875): the durable head wins for
        // every fact it carries. Session config is durable (read + guarded
        // write, FIG-1555/FIG-1895), so the head already holds any committed
        // override; only the runtime-lease facts stay live-owned. The
        // provider resolver is also live-owned and is not part of the
        // durable head.
        let live_owned = crate::runtime::state::LiveOwnedSessionFacts::of(&self.state.policy);
        let mut adopted = self.state.clone();
        crate::runtime::state::adopt_durable_head(
            &mut adopted,
            read,
            live_owned,
            self.fleet_format(),
        )
        .map_err(|source| SessionError::Store {
            context: "failed to adopt the session window".to_string(),
            source,
        })?;
        Box::pin(self.adopt_resident_state(adopted)).await?;
        self.resident_session.mark_graph_head_current();
        // The adopted head is authoritative for usage too: rebuild the attempts
        // this session still owes usage for from the durable totals plus the
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
        self.install_resident_state(adopted);
        if tool_restore.is_some() {
            self.tool_restore_report = tool_restore;
        }
        Ok(())
    }

    /// Rebuild the pending-attempt registry from the durable usage totals and
    /// the unconfirmed resident rows layered on top. Confirmed resident rows
    /// are already folded into `state.usage`, and folding holes by identity
    /// rather than by count means seeing a hole twice cannot double-count it.
    pub(in crate::runtime) fn rehydrate_unreported_usage_attempts(&mut self) {
        let mut totals = self.state.usage.clone();
        for pending in self.shared_token_ledger.lock_recover().iter() {
            totals.fold_saturating(&pending.entry);
        }
        self.unreported_usage_attempts = totals.outstanding;
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
        held_drive_fence: Option<&DriveFence>,
        turn_graph_appends: &TurnGraphAppendDraft,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::for_turn(
            self,
            held_drive_fence,
            turn_graph_appends,
        )?))
    }

    pub(super) fn runtime_session_services_after_commit(
        &self,
        held_drive_fence: Option<&DriveFence>,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::new(
            self,
            true,
            held_drive_fence,
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

    /// The ingress relay of this runtime's backend, asking this runtime's
    /// session work for drives (ADR 0109 §3).
    pub(in crate::runtime) fn ingress_relay(&self) -> super::drive::IngressRelay {
        super::drive::IngressRelay::over_backend(
            self.host.core.backend(),
            Arc::clone(self.host.queued_work()),
            Arc::clone(&self.host.core.clock),
        )
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
        super::durable_queue::enqueue_turn_input_to_store(
            self.state.session_id.clone(),
            store,
            &self.ingress_relay(),
            input,
            ingress,
            source_key,
            crate::RunSpec::default(),
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
        if store.session_id() != session_id {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                crate::StoreError::ForeignSessionRequest {
                    view_session_id: store.session_id().clone(),
                    request_session_id: session_id.clone(),
                }
                .to_string(),
            ));
        }
        store
            .cancel_queued_work_batch(batch_id)
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
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store());
        super::state::refuse_historical_frame_switch(
            store.as_ref(),
            &self.state.session_id,
            self.state.current_frame_node_id.as_deref(),
            &self.state.session_graph,
            &request.frame_key,
        )
        .await?;
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
        // The frame a compaction switch ends, and the execution that commits
        // the switch and gates its cleanup (ADR 0113 §3.1).
        let compacted_frame = self.state.current_frame_node_id.clone();
        let compaction_scope = scoped_effect_controller.execution_scope().clone();
        let Some(session) = self.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "runtime session not available".to_string(),
            ));
        };
        let plugin_session = Arc::clone(session.plugins());
        let state = self.read_view();
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
            traces: services.trace_emitter(),
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
        let frame_switch = matches!(outcome, Ok(true)).then_some(CompactionFrameSwitch {
            ended: compacted_frame,
            committing: compaction_scope,
        });
        let settlement = Box::pin(self.settle_pending_compaction_usage(frame_switch)).await;
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
            execution_title: Arc::from("Execution"),
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
    async fn settle_pending_compaction_usage(
        &mut self,
        frame_switch: Option<CompactionFrameSwitch>,
    ) -> Result<(), PluginOperationInvokeError> {
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
            self.fleet_format(),
        )
        .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let operation = super::lifecycle::initial_park_operation(&proposed)
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        let staged =
            session_manager::stage_token_ledger_shared(&self.shared_token_ledger, &operation)
                .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        for delta in staged.deltas() {
            self.state
                .usage
                .fold_checked(&delta.entry)
                .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        }
        let fleet_format = self.fleet_format();
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_staged_usage_and_budget(
                &mut self.state,
                staged.deltas(),
                operation,
                self.host.core.durability.commit_budget,
                fleet_format,
            )
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        // A compaction that opened a frame ends the one it left, with no
        // carries: the open cleared execution state, and only `continue_as`
        // carries values (ADR 0113 §3.1).
        let switched = frame_switch.is_some();
        if let Some(CompactionFrameSwitch { ended, committing }) = frame_switch {
            commit.frame_transition = super::turn_boundary::committed_frame_transition(
                &self.state,
                ended,
                Vec::new(),
                &committing,
                &persisted_node_ids,
            )
            .map_err(|err| PluginOperationInvokeError::Unknown(err.to_string()))?;
        }
        let commit_result = store.commit_runtime_state_verified(commit).await;
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
        if switched {
            self.restore_code_executor_after_frame_switch().await?;
        }
        Ok(())
    }

    /// Restore the live protocol session from the committed state after a
    /// frame switch, so the live globals match the durable ones: the switch
    /// cleared execution state, and the executor must not keep the ended
    /// frame's values (ADR 0113 §3.1).
    async fn restore_code_executor_after_frame_switch(
        &mut self,
    ) -> Result<(), PluginOperationInvokeError> {
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        let protocol_session = Arc::clone(session.plugins().protocol_session());
        let session_id = self.state.session_id.clone();
        let view = crate::plugin::ProtocolSessionRestoreView::new(&self.state);
        let restored = protocol_session
            .restore_session(
                crate::plugin::ProtocolSessionContext::new(session, &session_id),
                view,
            )
            .await;
        if let Err(err) = restored {
            self.invalidate_resident_session_state();
            return Err(PluginOperationInvokeError::Unknown(format!(
                "failed to restore the protocol session after a compaction frame switch: {err}"
            )));
        }
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
            self.apply_session_command_after_admission(vec![command], None)
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
        let enqueued = store
            .enqueue_queued_work(draft)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        // The command's batch owes its session a drive, armed at admission;
        // deliver it now (ADR 0109 §3). The drive applies the command at its
        // next boundary, before any turn input (ADR 0101 §4).
        self.ingress_relay()
            .deliver_admitted(enqueued.batch_id.as_str())
            .await;
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
                self.await_session_command_settlement(handle, None).await
            }
        }
    }

    async fn await_session_command_settlement(
        &mut self,
        handle: crate::runtime::SessionCommandSettlementHandle,
        previous_policy: Option<SessionPolicy>,
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
        let still_pending = store
            .list_queued_work()
            .await
            .map_err(super::runtime_error_from_store_commit)?
            .iter()
            .any(|batch| batch.batch_id == handle.receipt.batch_id);
        if !still_pending {
            let completed = store
                .queued_work_batch_completed(&handle.receipt.batch_id)
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            if !completed {
                return Ok(crate::runtime::SessionCommandSettlement::Cancelled(
                    handle.receipt,
                ));
            }
            let previous_policy = previous_policy.unwrap_or_else(|| self.session_policy());
            self.refresh_session_graph_from_store()
                .await
                .map_err(runtime_error_from_session_command_refresh)?;
            self.notify_session_config_changed(previous_policy)
                .await
                .map_err(|error| {
                    RuntimeError::new(
                        RuntimeErrorCode::SessionCommandPostDriveRefresh,
                        error.to_string(),
                    )
                })?;
            // The refresh adopts the durable head, which already carries
            // this command's committed values — or newer ones from a
            // later writer (head-authoritative adoption, FIG-1875). Every
            // successful refresh path either confirms the resident state
            // already carries the drain commit or fully hydrates the
            // head, and probe failures propagate as errors, so there is
            // no edge that needs the patch re-published residently.
            // Reapplying it here would overwrite a newer settled head
            // with this command's older values, resident-only.
            Ok(crate::runtime::SessionCommandSettlement::Durable(
                handle.receipt,
            ))
        } else {
            Ok(crate::runtime::SessionCommandSettlement::Pending(
                handle.receipt,
            ))
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
        self.invalidate_resident_session_state();
        Ok(receipt)
    }

    /// Wait for the command `receipt` names to settle, and adopt the durable
    /// head it settled on. `Pending` when the engine has not settled it yet;
    /// the command stays durable and settles later. Callers that need a
    /// settled result await the engine drive outside this runtime's writer lock.
    pub async fn settle_session_command(
        &mut self,
        receipt: crate::SessionCommandReceipt,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        self.await_session_command_settlement(
            crate::runtime::SessionCommandSettlementHandle { receipt },
            None,
        )
        .await
    }

    /// Settle an engine-driven command and report its policy transition from
    /// the state observed before the command was enqueued.
    pub async fn settle_session_command_from_policy(
        &mut self,
        receipt: crate::SessionCommandReceipt,
        previous_policy: SessionPolicy,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        self.await_session_command_settlement(
            crate::runtime::SessionCommandSettlementHandle { receipt },
            Some(previous_policy),
        )
        .await
    }

    pub async fn drain_next_session_command(
        &mut self,
        drive_fence: &crate::store::DriveFence,
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
        // The command commit keeps its batch's existing operation identity.
        let controller = host.scoped(crate::AdmittedScope::queue_drain(
            &self.state.session_id,
            "session-command",
        ))?;
        self.drain_next_session_command_with_cancellation(
            drive_fence,
            tokio_util::sync::CancellationToken::new(),
            controller.controller(),
        )
        .await
    }

    pub async fn drain_next_session_command_with_cancellation(
        &mut self,
        drive_fence: &crate::store::DriveFence,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        self.drain_next_session_command_fenced(drive_fence, cancellation, effect_controller)
            .await
    }

    /// Apply the session's leading open command run, its commit fenced by
    /// `drive_fence` (ADR 0109 §7, FIG-3927 §2.7): a drive that sealed an
    /// admission since the fence was read refuses the commit as superseded.
    ///
    /// The command lane takes no binding. The run's rows are read open and
    /// their obligations acknowledged delivered in one fenced write, and the
    /// commit that applies the run settles them, predicated on each row still
    /// being open. A host withdrawal in between refuses that commit, which
    /// applies nothing, and the lane is read again.
    pub(super) async fn drain_next_session_command_fenced(
        &mut self,
        drive_fence: &crate::store::DriveFence,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        loop {
            self.reload_invalidated_resident_session_state().await?;
            let Some(store) = self
                .session
                .as_ref()
                .and_then(|session| session.history_store())
            else {
                return Ok(None);
            };
            let batches = store
                .open_session_command_run(drive_fence)
                .await
                .map_err(super::runtime_error_from_store_commit)?;
            if batches.is_empty() {
                return Ok(None);
            }
            let run = crate::AdmittedQueuedWork {
                session_id: self.state.session_id.clone(),
                batches,
            };
            let Some(commands) = run.session_commands() else {
                return Err(RuntimeError::new(
                    crate::RuntimeErrorCode::SessionCommandClaim,
                    format!(
                        "session command run {:?} did not contain only single-command control \
                         batches",
                        run.batch_ids()
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
            if self
                .apply_session_command(
                    commands,
                    run.completion(),
                    drive_fence,
                    cancellation.clone(),
                    effect_controller,
                )
                .await?
            {
                return Ok(receipts.into_iter().next());
            }
        }
    }

    /// Apply `commands` and commit them, settling `completion`'s rows.
    /// `false` when a row was withdrawn since the lane was read: nothing was
    /// applied.
    async fn apply_session_command(
        &mut self,
        commands: Vec<crate::SessionCommand>,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<bool, RuntimeError> {
        let has_durable_store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .is_some();
        if !has_durable_store
            || !super::commit_admission::requires_local_commit_admission(effect_controller)
        {
            return self
                .apply_session_command_after_admission(commands, Some((completion, drive_fence)))
                .await;
        }
        let session_id = self.state.session_id.clone();
        let work_identity = completion
            .batch_ids
            .first()
            .map_or_else(|| "session-command".to_string(), ToString::to_string);
        let result: Result<bool, RuntimeCommitAdmissionError> =
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
                        Some((completion, drive_fence)),
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
        applied: Option<(crate::QueuedWorkCompletion, &crate::store::DriveFence)>,
    ) -> Result<bool, RuntimeError> {
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
            // Commands explicitly change the sticky config, unlike the
            // recorded execution view of a root.
            next_state.authority.committed_config = None;
            next_state.authority.root_snapshot = None;
            for command in &commands {
                let crate::SessionCommand::ApplyConfigPatch { patch } = command else {
                    unreachable!("config-only command group was checked above")
                };
                patch.validate_for_fleet(self.fleet_format())?;
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
                self.install_resident_state(next_state);
            }
            return Ok(true);
        };
        let Some((completion, drive_fence)) = applied else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "persisted session commands are applied by the drive's command lane",
            ));
        };
        let operation = completion
            .batch_ids
            .first()
            .map(|batch_id| {
                let state = next_config_state.as_ref().unwrap_or(&self.state);
                crate::OperationId::new(state.queue_drain_scope(batch_id), "session-command")
            })
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "persisted session commands require an open command row",
                )
            })?;
        let fleet_format = self.fleet_format();
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
                fleet_format,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        commit.drive_fence = Some(Box::new(drive_fence.clone()));
        commit.applied_commands = Some(completion);
        let result = match store.commit_runtime_state_verified(commit).await {
            // A host withdrew a command since the lane was read: the commit
            // applied nothing, and the lane is read again (FIG-3927 §2.7).
            Err(crate::StoreError::SessionCommandWithdrawn { .. }) => return Ok(false),
            result => result,
        }
        .map_err(|error| match error {
            // A later admission sealed after the drain presented its
            // fence: nothing was written, and the drive applies the
            // command (ADR 0109 §7).
            error @ crate::StoreError::StaleDriveFence { .. } => {
                RuntimeError::new(RuntimeErrorCode::StoreCommitSuperseded, error.to_string())
            }
            error => super::runtime_error_from_store_commit(error),
        })?;
        commit_state.apply_persisted_commit_result(result);
        commit_state.mark_node_ids_persisted(persisted_node_ids);
        if let Some(next_state) = next_config_state {
            self.install_resident_state(next_state);
        }
        Ok(true)
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

/// The frame switch an administrative compaction commits (ADR 0113 §3.1):
/// the frame it left, and the compaction's own execution, which gates the
/// ended frame's cleanup.
struct CompactionFrameSwitch {
    ended: Option<crate::FrameNodeId>,
    committing: crate::ExecutionScope,
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
