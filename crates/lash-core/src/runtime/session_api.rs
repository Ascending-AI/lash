use super::*;
use crate::SessionId;
use crate::facade_support::RuntimeSessionStateFacadeOps;

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

    /// Publish the resident authority to the live plugin session: its tool
    /// authority, and the plugin configuration its hooks run under — the
    /// installed view's, which inside a root is the root's recorded
    /// admission (FIG-4379).
    pub(super) fn publish_resident_authority(&self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        session
            .plugins()
            .publish_plugin_config(self.state.admitted_plugin_config());
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
            .map(|session| session.shared_tool_catalog())
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
    /// The protocol turn options the session runs under: a view of the
    /// protocol plugin's recorded configuration namespace (FIG-4379).
    pub fn protocol_turn_options(&self) -> crate::ProtocolTurnOptions {
        self.state.effective_protocol_turn_options()
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
            self.state.effective_protocol_turn_options(),
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

    /// The session's model usage, read from the engine-owned ledger
    /// (ADR 0125). A durable read keyed by the session's owner: it covers every
    /// call made for the session, whichever runtime or engine ran it.
    pub async fn usage(&self) -> Result<crate::OwnerUsage, SessionError> {
        let owner = crate::RuntimeOwner::Session(self.state.session_id.clone());
        self.host
            .core
            .usage_accounting()
            .store
            .load_owner_usage(&owner)
            .await
            .map_err(|source| SessionError::Store {
                context: "failed to load session usage".to_string(),
                source,
            })
    }

    /// Ask the session's provider for the usage of every unreported attempt the
    /// ledger holds for this session, and append one correction per recovered
    /// generation (ADR 0125, FIG-2765).
    ///
    /// Host-invoked and never on the turn hot path: each lookup is bounded by
    /// the provider (timeout plus one retry). The outstanding attempts are
    /// read from the store, so any host can reconcile, and a correction is an
    /// idempotent append: a retried correction is a no-op. An attempt without
    /// a generation id, one the provider has no record of, one whose lookup
    /// failed and one whose correction conflicts with a stored one come back
    /// as `unresolved`.
    pub async fn reconcile_unreported_usage(
        &mut self,
    ) -> Result<UsageReconciliationReport, SessionError> {
        let mut report = UsageReconciliationReport::default();
        let session_id = self.state.session_id.clone();
        let owner = crate::RuntimeOwner::Session(session_id.clone());
        let accounting = self.host.core.usage_accounting();
        let outstanding = accounting
            .store
            .load_owner_usage(&owner)
            .await
            .map_err(|source| SessionError::Store {
                context: "failed to load outstanding usage attempts".to_string(),
                source,
            })?
            .outstanding;
        if outstanding.is_empty() {
            return Ok(report);
        }
        let policy = self.state.effective_policy().clone();
        let mut provider = self
            .host
            .resolve_session_policy(&session_id, policy)?
            .binding
            .provider;
        for attempt in outstanding {
            let Some(generation_id) = attempt.generation_id.clone() else {
                report.unresolved.push(attempt);
                continue;
            };
            match provider.reconcile_usage(&generation_id).await {
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
                    let correction = crate::UsageCorrection {
                        effect: attempt.effect.clone(),
                        call_ordinal: attempt.call_ordinal,
                        provider_attempt: attempt.provider_attempt,
                        usage: usage.clone(),
                        generation_id: generation_id.clone(),
                    };
                    let now_ms = crate::ClockWallTime::timestamp_ms(accounting.clock.as_ref());
                    match accounting
                        .store
                        .append_usage_corrections(&owner, std::slice::from_ref(&correction), now_ms)
                        .await
                    {
                        Ok(_) => report.reconciled.push(ReconciledUsageAttempt {
                            attempt,
                            usage,
                            provider_usage: reconciled.provider_usage,
                        }),
                        Err(crate::UsageAppendError::Store(source)) => {
                            return Err(SessionError::Store {
                                context: "failed to append a usage correction".to_string(),
                                source,
                            });
                        }
                        Err(error) => {
                            tracing::warn!(
                                session_id = %session_id,
                                llm_call_id = ?attempt.llm_call_id,
                                call_ordinal = attempt.call_ordinal,
                                provider_attempt = attempt.provider_attempt,
                                generation_id,
                                error = %error,
                                "usage correction refused; attempt stays unreported"
                            );
                            report.unresolved.push(attempt);
                        }
                    }
                }
                Ok(None) => report.unresolved.push(attempt),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        llm_call_id = ?attempt.llm_call_id,
                        call_ordinal = attempt.call_ordinal,
                        provider_attempt = attempt.provider_attempt,
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
                // The config-only head a creating admission writes (FIG-4099)
                // carries no graph: a runtime still at revision 0 has not
                // fallen behind it, whatever unpersisted initial frame it
                // holds.
                let created_only = head.head_revision == 0
                    && head.leaf_node_id.is_none()
                    && head.checkpoint_ref.is_none();
                let moved = self.state.head_revision != head.head_revision
                    || (!created_only
                        && (head.leaf_node_id != self.state.session_graph.leaf_node_id
                            || head.checkpoint_ref != self.state.checkpoint_ref));
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

    /// Adopt `base`, the head the running direct turn was admitted on, or the
    /// one an administrative compaction recorded (FIG-4133), as the resident
    /// session (FIG-3682).
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
        Ok(Arc::new(RuntimeSessionServices::new(self, None)?))
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
        .with_policy(self.host.core.control.relay_policy())
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
                plugin_config: plugin_session.admitted_plugin_config(),
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
}

pub(super) enum AcceptedSessionCommand {
    Inline(crate::SessionCommandReceipt),
    Queued(crate::runtime::SessionCommandSettlementHandle),
}

impl LashRuntime {
    async fn accept_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<AcceptedSessionCommand, RuntimeError> {
        self.enqueue_session_command(command, idempotency_key)
            .await
            .map(|(accepted, _)| accepted)
    }

    /// Accept `command` under `idempotency_key`, and the command a retained
    /// submission under the same key already holds, when the store answered
    /// with that submission's row instead of inserting one.
    pub(super) async fn enqueue_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<(AcceptedSessionCommand, Option<crate::SessionCommand>), RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        let idempotency_key = idempotency_key.into();
        if idempotency_key.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandIdempotencyKey,
                "session command idempotency key cannot be empty",
            ));
        }
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
            return Ok((AcceptedSessionCommand::Inline(receipt), None));
        };
        let draft = crate::QueuedWorkBatchDraft::new(
            session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            command,
        )
        .with_source_key(source_key.clone());
        let (enqueued, existing) = match store
            .enqueue_queued_work_with_outcome(draft)
            .await
            .map_err(super::runtime_error_from_store_commit)?
        {
            crate::QueuedWorkEnqueueOutcome::Inserted(batch) => (batch, None),
            crate::QueuedWorkEnqueueOutcome::Existing(batch) => {
                let existing = batch.items.first().and_then(|item| match &item.payload {
                    crate::QueuedWorkPayload::SessionCommand { command } => {
                        Some(command.as_ref().clone())
                    }
                    _ => None,
                });
                (batch, existing)
            }
        };
        // The command's batch owes its session a drive, armed at admission;
        // deliver it now (ADR 0109 §3). The drive applies the command at its
        // next boundary, before any turn input (ADR 0101 §4).
        self.ingress_relay()
            .deliver_admitted(enqueued.batch_id.as_str())
            .await;
        Ok((
            AcceptedSessionCommand::Queued(crate::runtime::SessionCommandSettlementHandle {
                receipt: crate::SessionCommandReceipt {
                    session_id,
                    batch_id: enqueued.batch_id,
                    source_key,
                },
            }),
            existing,
        ))
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
            let Some(completion) = store
                .queued_work_batch_completion(&handle.receipt.batch_id)
                .await
                .map_err(super::runtime_error_from_store_commit)?
            else {
                return Ok(crate::runtime::SessionCommandSettlement::Cancelled(
                    handle.receipt,
                ));
            };
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
            Ok(match completion.command_outcome {
                // A command that settles with an outcome answers what it
                // settled as, on whichever runtime applied it (FIG-4201,
                // FIG-4202).
                Some(outcome) => crate::runtime::SessionCommandSettlement::Applied {
                    receipt: handle.receipt,
                    outcome,
                },
                None => crate::runtime::SessionCommandSettlement::Durable(handle.receipt),
            })
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
            &controller,
        )
        .await
    }

    pub async fn drain_next_session_command_with_cancellation(
        &mut self,
        drive_fence: &crate::store::DriveFence,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        self.drain_next_session_command_fenced(drive_fence, cancellation, effect_controller)
            .await
            .map_err(CommandDrainStop::into_runtime_error)
    }

    /// Apply the session's leading open command run, its commit fenced by
    /// `drive_fence` (ADR 0109 §7, FIG-3927 §2.7): a drive that sealed an
    /// admission since the fence was read refuses the commit as superseded.
    ///
    /// The command lane takes no binding. The run's rows are read open and
    /// their obligations acknowledged delivered in one fenced write, and the
    /// commit that applies the run settles them, predicated on each row still
    /// being open. The read admits the run (FIG-4202): a host withdrawal
    /// after it is refused, so the commit's predicate is a backstop, and a
    /// commit it refuses applies nothing and the lane is read again.
    ///
    /// The resident session is reloaded outside any recorded step, so its
    /// outcome never decides what the root journals (FIG-4346): a reload that
    /// failed stops the drain [`CommandDrainStop::Headless`] before the next
    /// recorded read, and the root reads on headless. So does a session that
    /// retired under a run the root read, whose settlement and commit write
    /// nothing to the journal.
    pub(super) async fn drain_next_session_command_fenced(
        &mut self,
        drive_fence: &crate::store::DriveFence,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<Option<crate::SessionCommandReceipt>, CommandDrainStop> {
        loop {
            if let Err(fault) = self.reload_invalidated_resident_session_state().await {
                return Err(CommandDrainStop::Headless(fault));
            }
            let Some(store) = self
                .session
                .as_ref()
                .and_then(|session| session.history_store())
            else {
                return Ok(None);
            };
            let batches = execute_session_command_run_read(
                effect_controller,
                &self.state.session_id,
                crate::RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(ReadSessionCommandRunRunner {
                        store: store.clone(),
                        fence: drive_fence.clone(),
                    }),
                    None,
                ),
            )
            .await
            .map_err(CommandDrainStop::Failed)?;
            if batches.is_empty() {
                return Ok(None);
            }
            let run = crate::AdmittedQueuedWork {
                session_id: self.state.session_id.clone(),
                batches,
            };
            let Some(commands) = run.session_commands() else {
                return Err(CommandDrainStop::Failed(RuntimeError::new(
                    crate::RuntimeErrorCode::SessionCommandRun,
                    format!(
                        "session command run {:?} did not contain only single-command control \
                         batches",
                        run.batch_ids()
                    ),
                )));
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
            // A replayed read may name a run this root already applied. An
            // administrative compaction or a config transaction runs again:
            // it replays the steps it journaled, then finds its command
            // settled and adopts the head its commit published without
            // committing again (FIG-4258, FIG-4379). Any other command
            // journals nothing, so a settled run is simply passed.
            let journaled = matches!(
                commands.as_slice(),
                [crate::SessionCommand::CompactContext { .. }
                    | crate::SessionCommand::ApplyConfigTransaction { .. }]
            );
            // Only a compaction and a config transaction journal their
            // apply. Any other run settles and commits off the journal, so a
            // session that retired under it leaves the root's next recorded
            // step the next read.
            let off_journal = |error: RuntimeError| {
                if !journaled && error.is_session_retirement() {
                    CommandDrainStop::Headless(error)
                } else {
                    CommandDrainStop::Failed(error)
                }
            };
            if !journaled
                && self
                    .session_command_run_settled(&store, &run.completion())
                    .await
                    .map_err(off_journal)?
            {
                return Ok(receipts.into_iter().next());
            }
            if Box::pin(self.apply_session_command(
                commands,
                run.completion(),
                drive_fence,
                cancellation.clone(),
                effect_controller,
            ))
            .await
            .map_err(off_journal)?
            {
                return Ok(receipts.into_iter().next());
            }
        }
    }

    /// Whether the commit that applies the command run `completion` names
    /// has landed: its first batch's completion is recorded.
    pub(super) async fn session_command_run_settled(
        &self,
        store: &crate::store::SessionStore,
        completion: &crate::QueuedWorkCompletion,
    ) -> Result<bool, RuntimeError> {
        let Some(batch_id) = completion.batch_ids.first() else {
            return Ok(false);
        };
        store
            .queued_work_batch_completion(batch_id.as_str())
            .await
            .map(|completion| completion.is_some())
            .map_err(super::runtime_error_from_store_commit)
    }

    /// Apply `commands` and commit them, settling `completion`'s rows.
    /// `false` when a row was withdrawn since the lane was read: nothing was
    /// applied. An administrative compaction applies alone, under its own
    /// scope (FIG-4201).
    async fn apply_session_command(
        &mut self,
        commands: Vec<crate::SessionCommand>,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<bool, RuntimeError> {
        // A command that settles with an outcome applies alone, under its
        // own scope, in the commit that settles it (FIG-4201, FIG-4202).
        match commands.as_slice() {
            [crate::SessionCommand::CompactContext { instructions }] => {
                return Box::pin(self.apply_compact_context_command(
                    instructions.clone(),
                    completion,
                    drive_fence,
                    effect_controller,
                ))
                .await;
            }
            [crate::SessionCommand::ApplyConfigTransaction { transaction }] => {
                return Box::pin(self.apply_config_transaction_command(
                    transaction.as_ref().clone(),
                    completion,
                    drive_fence,
                    effect_controller,
                ))
                .await;
            }
            [
                crate::SessionCommand::AppendSessionNodes { .. }
                | crate::SessionCommand::RunPluginCommand { .. }
                | crate::SessionCommand::RunPluginTask { .. }
                | crate::SessionCommand::OpenAgentFrame { .. },
            ] => {
                drop(RuntimeNamedPhase::begin(
                    self.turn_phase_probe.clone(),
                    super::host_commands::SESSION_COMMAND_APPLYING_PHASE,
                ));
                // A host command applies against the boundary's committed
                // head, whichever runtime committed it last (FIG-4202).
                self.adopt_committed_head().await?;
            }
            _ => {}
        }
        match commands.as_slice() {
            [crate::SessionCommand::AppendSessionNodes { request }] => {
                return Box::pin(self.apply_append_session_nodes_command(
                    request.as_ref().clone(),
                    completion,
                    drive_fence,
                ))
                .await;
            }
            [crate::SessionCommand::RunPluginCommand { name, args }] => {
                return Box::pin(self.apply_plugin_operation_command(
                    super::host_commands::HostPluginOperation::Command,
                    name.clone(),
                    args.clone(),
                    completion,
                    drive_fence,
                ))
                .await;
            }
            [crate::SessionCommand::RunPluginTask { name, args }] => {
                return Box::pin(self.apply_plugin_operation_command(
                    super::host_commands::HostPluginOperation::Task,
                    name.clone(),
                    args.clone(),
                    completion,
                    drive_fence,
                ))
                .await;
            }
            [crate::SessionCommand::OpenAgentFrame { request }] => {
                return Box::pin(self.apply_open_agent_frame_command(
                    request.as_ref().clone(),
                    completion,
                    drive_fence,
                ))
                .await;
            }
            _ => {}
        }
        let effect_controller = effect_controller.controller();
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
        debug_assert_eq!(commands.len(), 1, "session commands apply alone");
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
                // The drive's command lane applies a persisted command that
                // settles with an outcome before this point; only a storeless
                // runtime's inline command reaches here, and a storeless
                // runtime writes its head directly.
                command @ (crate::SessionCommand::CompactContext { .. }
                | crate::SessionCommand::AppendSessionNodes { .. }
                | crate::SessionCommand::RunPluginCommand { .. }
                | crate::SessionCommand::RunPluginTask { .. }
                | crate::SessionCommand::OpenAgentFrame { .. }
                | crate::SessionCommand::ApplyConfigTransaction { .. }) => {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::SessionCommandRequired,
                        format!(
                            "a storeless runtime applies `{}` directly, not through a session \
                             command",
                            command.kind()
                        ),
                    ));
                }
            }
        }
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
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
                crate::OperationId::new(self.state.queue_drain_scope(batch_id), "session-command")
            })
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "persisted session commands require an open command row",
                )
            })?;
        let fleet_format = self.fleet_format();
        let commit_state = &mut self.state;
        if let Some(session) = self.session.as_ref() {
            commit_state.capture_plugin_states(session.plugins());
        }
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                commit_state,
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

pub(in crate::runtime) fn queued_turn_input_store_required() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::StoreCommitFailed,
        "queued turn input requires a persistent runtime store",
    )
}

/// The first execution of one `ReadSessionCommandRun` step: the live read of
/// the command lane under the root's fence (FIG-4201).
/// Why a fenced command drain stopped without an answer.
pub(in crate::runtime) enum CommandDrainStop {
    /// The drain holds no current head for its next recorded read: the
    /// resident session could not be reloaded (a deleted session's reload
    /// among them), or the session retired under a run the root read. The
    /// root reads on headless from its next read.
    Headless(RuntimeError),
    /// Anything else the drain met.
    Failed(RuntimeError),
}

impl CommandDrainStop {
    pub(in crate::runtime) fn into_runtime_error(self) -> RuntimeError {
        match self {
            Self::Headless(error) | Self::Failed(error) => error,
        }
    }
}

/// Read the session's leading open command run as one recorded step,
/// `session-command-run:{ordinal}` on `controller`'s scope, keyed by the
/// read's ordinal among its reads (FIG-4201), whose first execution runs
/// `runner`.
///
/// The first execution reads the lane live, acknowledging the run's
/// obligations delivered under the drive fence. A replay of the root reads
/// back the run it recorded, even after the commit that applied it settled
/// the lane: an administrative compaction replays the base and the summary
/// it journaled and adopts its settled commit, and any other settled run is
/// passed. The live lane would skip the settled command and run the root's
/// next steps where its journal holds the compaction's.
pub(in crate::runtime) async fn execute_session_command_run_read(
    controller: &crate::ScopedEffectController<'_>,
    session_id: &SessionId,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Vec<crate::QueuedWorkBatch>, RuntimeError> {
    let ordinal = controller.next_command_run_ordinal();
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            controller.execution_scope().clone(),
            format!("session-command-run:{ordinal}"),
        )?,
        crate::RuntimeAttribution::for_session(session_id.clone()),
        format!("session-command-run:{ordinal}"),
    );
    controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::ReadSessionCommandRun {
                    session: session_id.clone(),
                },
            ),
            runner,
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_session_command_run)
        .map_err(crate::RuntimeEffectControllerError::into_runtime_error)
}

struct ReadSessionCommandRunRunner {
    store: crate::store::SessionStore,
    fence: crate::store::DriveFence,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for ReadSessionCommandRunRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::ReadSessionCommandRun { .. } = &envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "session-command-run executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        match self.store.open_session_command_run(&self.fence).await {
            Ok(batches) => Ok(crate::RuntimeEffectOutcome::ReadSessionCommandRun { batches }),
            // A superseded fence is the root's settled fact: every replay
            // decodes the same refusal.
            Err(error @ crate::StoreError::StaleDriveFence { .. }) => {
                Err(crate::RuntimeEffectControllerError::from(
                    super::runtime_error_from_store_commit(error),
                ))
            }
            // A store that did not answer is this attempt's fault.
            Err(error) => Err(crate::RuntimeEffectControllerError::from(
                super::runtime_error_from_store_commit(error),
            )
            .retryable_uncommitted_derivation()),
        }
    }
}
