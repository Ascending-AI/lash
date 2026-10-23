use super::*;
use crate::ActorContext;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use lash_vocabulary::identity::SessionId;

impl LashRuntime {
    pub fn session_id(&self) -> &SessionId {
        &self.state.session_id
    }

    /// The resident runtime state, read-only. Writes go through the install
    /// helpers, which publish the resident authority (FIG-4024).
    pub fn state(&self) -> &RuntimeSessionState {
        &self.state
    }

    /// The session actor's page reader the durable actor set on this
    /// runtime under its claim (FIG-6418); none where no claim activated
    /// it or the store publishes no heap pages.
    pub(in crate::runtime) fn page_reader(&self) -> Option<&crate::plugin::SessionPageReader> {
        self.page_reader.as_ref()
    }

    /// Takes `reader` as the session actor's page reader for this
    /// activation of the runtime under `claim`'s epoch.
    pub(in crate::runtime) fn note_page_reader(
        &mut self,
        reader: Option<crate::plugin::SessionPageReader>,
    ) {
        self.page_reader = reader;
    }

    /// The resident session's code executor: what publishes the heap
    /// pages of the execution root a head commit names (FIG-6418).
    pub(in crate::runtime) fn code_executor(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::plugin::CodeExecutorPlugin>> {
        self.session
            .as_ref()
            .and_then(|session| session.plugins().code_executor())
    }

    /// Stamp the live plugin state onto the resident state.
    ///
    /// # Errors
    /// The typed refusal of a plugin namespace that cannot be written in
    /// the format its admission recorded (FIG-4747).
    pub fn stamp_live_plugin_state(&mut self) -> Result<(), RuntimeError> {
        if let Some(session) = self.session.as_ref() {
            self.state
                .capture_plugin_states(session.plugins(), self.fleet_format())?;
        }
        Ok(())
    }

    /// Make `state` the resident runtime state. Every whole-state swap goes
    /// through here (durable adoption and reload, append rollback and receipt
    /// replay, settled config commands, turn commits, session creation), so
    /// none can skip what a replacement owes the live session: the resident
    /// plugin configuration is published to the live plugin session, so live
    /// discovery always reflects the settled configuration (FIG-2415,
    /// FIG-2987).
    pub(in crate::runtime) fn install_resident_state(
        &mut self,
        state: crate::RuntimeSessionState,
    ) -> Result<(), crate::FormatRefusal> {
        self.state = state;
        self.publish_resident_authority()
    }

    /// Transfer the acknowledged owner state into the runtime without reading
    /// back a head that this owner just committed.
    pub(in crate::runtime) async fn adopt_owner_state(
        &mut self,
        owner: &ActorContext,
    ) -> Result<(), RuntimeError> {
        let projection = owner
            .session_projection()
            .await
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::ResidentSessionReloadFailed,
                    error.to_string(),
                )
            })?
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::ResidentSessionReloadFailed,
                    "owner has no acknowledged session state",
                )
            })?;
        let run = self.state.authority.run_view().map(|view| view.run.clone());
        let mut state = projection.head.as_ref().clone();
        // The acknowledged head carries sticky configuration. A head write
        // inside a turn must keep executing under that turn's recorded view.
        if let Some(run) = run {
            state.install_run_view(&run);
        }
        self.install_resident_state(state).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::ResidentSessionReloadFailed,
                error.to_string(),
            )
        })?;
        self.resident_session.mark_adopted();
        Ok(())
    }

    /// Install a run's recorded [`ResolvedRun`](crate::ResolvedRun) as the
    /// resident execution view. The view's config carries its own authority,
    /// which can differ from the resident one (a replay after a config
    /// change, a recovered follow-on's inherited shape, a refresh the run
    /// re-installs its record over), so the install publishes it to the live
    /// plugin session just as a whole-state swap does (FIG-4022). This,
    /// [`Self::uninstall_run_view`] and [`Self::install_resident_state`]
    /// are the only writers of the resident authority.
    pub(in crate::runtime) fn install_resolved_run(
        &mut self,
        resolved: &crate::ResolvedRun,
    ) -> Result<(), crate::FormatRefusal> {
        if let Some(session) = &self.session {
            session
                .plugins()
                .host()
                .validate_config_formats(&resolved.config().plugin_config)?;
        }
        self.state.install_run_view(resolved);
        self.publish_resident_authority()
    }

    /// Uninstall the recorded view of the run this runtime ran last and
    /// restore the sticky config under it, published to the live plugin
    /// session as the install was. Whatever resolves or publishes config
    /// over the resident state afterwards reads the session's, never a
    /// run's overrides.
    pub(in crate::runtime) fn uninstall_run_view(&mut self) -> Result<(), crate::FormatRefusal> {
        if self.state.take_run_view().is_some() {
            self.publish_resident_authority()?;
        }
        Ok(())
    }

    /// Test hook: applies `edit` to a copy of the resident state and installs
    /// it through [`Self::install_resident_state`], so the edited authority is
    /// published to the live plugin session. No durable publication and no
    /// plugin or tool-state hydration.
    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::expect_used,
        reason = "test edits must leave config readable by the installed factories"
    )]
    pub fn edit_resident_state_for_test(&mut self, edit: impl FnOnce(&mut RuntimeSessionState)) {
        let mut state = self.state.clone();
        edit(&mut state);
        self.install_resident_state(state)
            .expect("test config formats decode");
    }

    /// Publish the resident authority to the live plugin session: the plugin
    /// configuration its hooks and tool providers run under — the installed
    /// view's, which inside a run is the run's recorded admission
    /// (FIG-4379).
    pub(super) fn publish_resident_authority(&mut self) -> Result<(), crate::FormatRefusal> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        session
            .plugins()
            .publish_plugin_config(self.state.admitted_plugin_config())?;
        Ok(())
    }
    pub fn active_tool_catalog_shared(
        &self,
    ) -> Result<Arc<Vec<serde_json::Value>>, lash_execution_contracts::plugin_error::PluginError>
    {
        match self.resident_session.validity() {
            ResidentSessionState::Invalidated { decision_id } => {
                self.trace_synchronous_resident_state_refusal(
                    decision_id,
                    "active_tool_catalog_shared",
                );
                return Err(
                    lash_execution_contracts::plugin_error::PluginError::Session(
                        "resident session state is invalidated; durable reload is required"
                            .to_string(),
                    ),
                );
            }
            ResidentSessionState::Valid => {}
        }
        self.session
            .as_ref()
            .map(|session| session.shared_tool_catalog())
            .unwrap_or_else(|| Ok(Arc::new(Vec::new())))
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
        .with_transcript_decoders(self.services.plugins.transcript_decoders())
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
        let mut state = self.state.clone();
        if let Some(session) = self.session.as_ref() {
            state.capture_plugin_states(session.plugins(), self.fleet_format())?;
        }
        Ok(state)
    }

    pub async fn await_background_work(&mut self) -> Result<(), SessionError> {
        if self.process_sync_needed.swap(false, Ordering::AcqRel) {
            self.refresh_session_graph_from_store().await?;
        }
        Ok(())
    }

    pub async fn refresh_session_graph_from_store(&mut self) -> Result<(), SessionError> {
        let Some(store) = self.services.store.clone() else {
            return Ok(());
        };
        let requires_hydration = match store.load_session_head_meta().await {
            Ok(Some(head)) => {
                // The config-only head a creating admission writes (FIG-4099)
                // carries no graph: a runtime still at revision 0 has not
                // fallen behind it, whatever unpersisted initial frame it
                // holds.
                let created_only = head.is_created();
                self.state.head_revision != head.head_revision
                    || (!created_only
                        && (head.leaf_node_id != self.state.session_graph.leaf_node_id
                            || head.checkpoint_ref != self.state.checkpoint_ref))
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
                return Ok(());
            }
            // The bounded read is an optimization. If it cannot determine the
            // durable head, retain the canonical full read rather than letting
            // probe failure report the resident graph as fresh.
            Err(error) if error.is_transient() => true,
            // A head the store proves corrupt or refuses is its typed answer,
            // not an indeterminate probe the full read may paper over.
            Err(source) => {
                return Err(SessionError::Store {
                    context: "failed to read the session head".to_string(),
                    source,
                });
            }
        };
        if !requires_hydration {
            self.resident_session.mark_graph_loaded();
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
        let Some(store) = self.services.store.clone() else {
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
            empty.checkpoint_ref = None;
            empty.checkpoint_components = crate::RuntimeCheckpointComponents::complete_empty();
            empty.head_revision = 0;
            empty.turn_index = 0;
            empty.token_usage = crate::LlmUsage::default();
            empty.last_prompt_usage = None;
            Box::pin(self.adopt_resident_state(empty)).await?;
            self.resident_session.mark_graph_loaded();
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
        // override. The provider resolver is live-owned and is not part of the
        // durable head.
        let mut adopted = self.state.clone();
        crate::runtime::state::adopt_durable_head(&mut adopted, read, self.fleet_format())
            .map_err(|source| SessionError::Store {
                context: "failed to adopt the session window".to_string(),
                source,
            })?;
        Box::pin(self.adopt_resident_state(adopted)).await?;
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
    pub(in crate::runtime) async fn adopt_resident_state(
        &mut self,
        mut adopted: crate::RuntimeSessionState,
    ) -> Result<(), SessionError> {
        if self.session.is_none() {
            self.install_resident_state(adopted)?;
            return Ok(());
        }
        Box::pin(self.restore_resident_session_components(&mut adopted))
            .await
            .map_err(|(_stage, error)| {
                if error.cause.is_some() {
                    SessionError::Plugin(
                        lash_execution_contracts::plugin_error::PluginError::Runtime(error),
                    )
                } else {
                    SessionError::Protocol(format!(
                        "failed to restore the adopted session head: {error}"
                    ))
                }
            })?;
        self.install_resident_state(adopted)?;
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
        Ok(Arc::new(RuntimeSessionServices::new(self)?))
    }

    pub(super) fn runtime_session_services_for_turn(
        &self,
        turn_graph_appends: &TurnGraphAppendDraft,
    ) -> Result<Arc<RuntimeSessionServices>, PluginOperationInvokeError> {
        Ok(Arc::new(RuntimeSessionServices::for_turn(
            self,
            turn_graph_appends,
        )?))
    }

    pub fn session_read_service(
        &self,
    ) -> Result<Arc<dyn crate::plugin::SessionReadService>, PluginOperationInvokeError> {
        self.runtime_session_services()
            .map(|services| services.read_service())
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

    /// Returns a lane-less session graph service. Host head writes are
    /// boundary session commands, applied by the session at a turn boundary.
    /// Acceptance returns a durable pending receipt or its recorded result;
    /// it never waits for the applying owner.
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

    pub fn effect_host(&self) -> ActorContext {
        self.host.core.control.effect_host.clone()
    }

    /// Durably accept `input` for this session, caused by `parent`: the
    /// caller's producer or the scope it runs in, or none when the input is
    /// unobserved.
    pub async fn enqueue_turn_input(
        &self,
        input: crate::TurnInput,
        ingress: crate::TurnInputIngress,
        source_key: Option<String>,
        parent: Option<lash_trace::TraceParent>,
    ) -> Result<crate::PendingTurnInput, RuntimeError> {
        let store = self
            .services
            .store
            .clone()
            .ok_or_else(queued_turn_input_store_required)?;
        super::durable_queue::enqueue_turn_input_to_store(
            store,
            input,
            source_key,
            super::HostInputAdmission {
                ingress,
                run_spec: crate::RunSpec::default(),
                pin: false,
                parent,
                max_ingress_bytes: self.host.core.backend().max_ingress_bytes(),
            },
        )
        .await
    }

    /// Frozen prompt declarations. This read never materializes the session.
    pub fn prompt_catalog(
        &self,
    ) -> Result<crate::plugin::prompt::PromptCatalogSnapshot, RuntimeError> {
        self.services
            .plugins
            .host()
            .prompt_catalog_snapshot()
            .map_err(|error| crate::RuntimeEffectControllerError::from(error).into_runtime_error())
    }

    /// Explicitly executable, unadmitted planning. May bind plugins and expand
    /// prompt families, but runs no renderer and persists nothing.
    pub fn preview_prompt_plan(
        &self,
        plan: &crate::prompt_sections::PromptPlan,
        purpose: &crate::prompt_sections::PromptPurpose,
        offered: &crate::plugin::prompt::OfferedTools,
    ) -> Result<
        Result<crate::prompt_sections::ResolvedPromptPlan, crate::prompt_sections::PromptPlanError>,
        RuntimeError,
    > {
        self.services
            .plugins
            .prepare_capabilities()
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            })?;
        Ok(self
            .services
            .plugins
            .prompt_catalog()
            .preview(plan, purpose, offered))
    }

    /// The surface the session's providers offer under its recorded config:
    /// what its next run records before that run's own options. It records
    /// nothing.
    pub fn offered_tool_surface(&self) -> Result<crate::plugin::ResolvedToolSurface, RuntimeError> {
        let plugin = |error: lash_execution_contracts::plugin_error::PluginError| {
            RuntimeError::new(RuntimeErrorCode::Plugin, error.to_string())
        };
        // A session no run materialized yet builds its capabilities first.
        self.services
            .plugins
            .prepare_capabilities()
            .map_err(plugin)?;
        self.services
            .plugins
            .build_tool_surface(&self.state.admitted_plugin_config())
            .map_err(plugin)
    }

    /// The plugin session this runtime was assembled with, whether or not a
    /// run has materialized it.
    #[cfg(any(test, feature = "testing"))]
    pub fn assembled_plugin_session(&self) -> &Arc<crate::PluginSession> {
        &self.services.plugins
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

    pub fn session_policy(&self) -> SessionPolicy {
        self.state.effective_policy().clone()
    }

    /// Deliver the config change the session's head still owes its
    /// observers (FIG-5397) to exactly the observers it records, and take
    /// it from resident state, so the session's next head commit retires it
    /// (FIG-6164). An observer the deployment declared retired has its
    /// share retired as undeliverable. A runtime whose plugins are not built,
    /// or that cannot resolve every observer the change is owed to, delivers
    /// nothing and leaves the change owed: it is never retired against an
    /// empty or partial roster.
    pub(super) async fn deliver_undelivered_config_change(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(change) = self.state.undelivered_config_change.as_deref() else {
            return;
        };
        let services = match self.runtime_session_services() {
            Ok(services) => services,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.state.session_id,
                    %error,
                    "a config change was not delivered: the session's services are unavailable"
                );
                return;
            }
        };
        let revision = change.revision;
        let delivered = session
            .plugins()
            .dispatch(None)
            .deliver_config_change(
                &change.observers,
                SessionConfigChangedContext {
                    session_id: self.state.session_id.clone(),
                    revision,
                    changed: crate::plugin::SessionConfigField::between(
                        &change.previous,
                        &change.current,
                    ),
                    sessions: services.read_service(),
                },
            )
            .await;
        let delivery = match delivered {
            Ok(delivery) => delivery,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.state.session_id,
                    revision,
                    %error,
                    "a config change was not delivered: an observer it is owed to is not installed"
                );
                return;
            }
        };
        if let Some(error) = delivery.failures {
            tracing::warn!(?error, "session config observer failed");
        }
        self.state.undelivered_config_change = None;
    }
}

pub(super) enum AcceptedSessionCommand {
    Inline(crate::SessionCommandReceipt),
    Queued(crate::runtime::SessionCommandSettlementHandle),
}

/// Why a session command was not accepted.
pub(super) enum SessionCommandEnqueueError {
    /// The idempotency key already names a command with other submitted
    /// content (ADR 0101 §8): the store refused the resubmission.
    ChangedContent(RuntimeError),
    /// The runtime or its store could not take the command.
    Runtime(RuntimeError),
}

impl From<RuntimeError> for SessionCommandEnqueueError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<SessionCommandEnqueueError> for RuntimeError {
    fn from(error: SessionCommandEnqueueError) -> Self {
        match error {
            SessionCommandEnqueueError::ChangedContent(error)
            | SessionCommandEnqueueError::Runtime(error) => error,
        }
    }
}

impl LashRuntime {
    async fn accept_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
        parent: Option<lash_trace::TraceParent>,
    ) -> Result<AcceptedSessionCommand, RuntimeError> {
        Ok(self
            .enqueue_session_command(command, idempotency_key, parent)
            .await?)
    }

    /// Accept `command` under `idempotency_key`, caused by `parent`. An
    /// identical resubmission under the key answers the retained
    /// submission's row and cause; one with other content is refused
    /// [`SessionCommandEnqueueError::ChangedContent`].
    pub(super) async fn enqueue_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
        parent: Option<lash_trace::TraceParent>,
    ) -> Result<AcceptedSessionCommand, SessionCommandEnqueueError> {
        self.reload_invalidated_resident_session_state().await?;
        let idempotency_key = idempotency_key.into();
        if idempotency_key.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandIdempotencyKey,
                "session command idempotency key cannot be empty",
            )
            .into());
        }
        let source_key = command.source_key(&idempotency_key);
        let session_id = self.state.session_id.clone();
        let Some(store) = self.services.store.clone() else {
            let receipt = crate::SessionCommandReceipt {
                session_id,
                batch_id: lash_vocabulary::identity::BatchId::prefixed(
                    "inline-command:",
                    uuid::Uuid::new_v4(),
                ),
                source_key,
            };
            self.apply_session_command_after_admission(vec![command], None)
                .await?;
            return Ok(AcceptedSessionCommand::Inline(receipt));
        };
        let receipt = enqueue_stored_session_command(
            &store,
            &session_id,
            command,
            &idempotency_key,
            parent,
            self.host.core.backend().max_ingress_bytes(),
        )
        .await?;
        Ok(AcceptedSessionCommand::Queued(
            crate::runtime::SessionCommandSettlementHandle { receipt },
        ))
    }

    async fn await_session_command_settlement(
        &mut self,
        handle: crate::runtime::SessionCommandSettlementHandle,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        let store = self.services.store.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "accepted session command lost its persistent store",
            )
        })?;
        stored_session_command_status(store, handle.receipt).await
    }

    /// Submit `command` to the session's command lane and return as soon as
    /// it is durable: **before** it is applied (FIG-3600). The session's
    /// actor applies it at its next turn boundary, in order; its outcome is
    /// observed with [`Self::settle_session_command`].
    ///
    /// The resident session state is marked stale: the session commits the
    /// command over the durable head, so the next use of this runtime reloads
    /// the head instead of committing over a pre-command copy.
    pub async fn submit_session_command(
        &mut self,
        command: crate::SessionCommand,
        idempotency_key: impl Into<String>,
        parent: Option<lash_trace::TraceParent>,
    ) -> Result<crate::SessionCommandReceipt, RuntimeError> {
        let accepted = self
            .accept_session_command(command, idempotency_key, parent)
            .await?;
        let receipt = match accepted {
            AcceptedSessionCommand::Inline(receipt) => return Ok(receipt),
            AcceptedSessionCommand::Queued(handle) => handle.receipt,
        };
        self.invalidate_resident_session_state();
        Ok(receipt)
    }

    /// Read the command `receipt`'s settlement and adopt the current durable
    /// head once it settled. `Pending` when the engine has not settled it yet;
    /// the command stays durable and settles later. Callers that need a
    /// settled result await the session actor outside this runtime's writer lock.
    pub async fn settle_session_command(
        &mut self,
        receipt: crate::SessionCommandReceipt,
    ) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
        let settlement = self
            .await_session_command_settlement(crate::runtime::SessionCommandSettlementHandle {
                receipt,
            })
            .await?;
        if matches!(
            settlement,
            crate::runtime::SessionCommandSettlement::Applied { .. }
                | crate::runtime::SessionCommandSettlement::Durable(_)
        ) {
            // Receipt lookup reads only the recorded answer. Settlement on
            // this open runtime also owes head adoption: later commands or
            // writers may have superseded the receipt's committed values.
            // Never replay those values over the current durable head.
            self.refresh_session_graph_from_store()
                .await
                .map_err(runtime_error_from_session_command_refresh)?;
        }
        Ok(settlement)
    }

    pub async fn drain_next_session_command_with_cancellation(
        &mut self,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &crate::ActorContext,
    ) -> Result<Option<crate::SessionCommandReceipt>, RuntimeError> {
        self.drain_next_session_command_run(cancellation, effect_controller)
            .await
            .map_err(CommandDrainStop::into_runtime_error)
    }

    /// Apply the session's leading open command run (FIG-3927 §2.7).
    ///
    /// The command lane takes no binding. The run's rows are read open, and
    /// the commit that applies the run settles them, predicated on each row
    /// still being open. The read admits the run (FIG-4202): a host withdrawal
    /// after it is refused, so the commit's predicate is a backstop, and a
    /// commit it refuses applies nothing and the lane is read again.
    ///
    /// The resident session is reloaded outside any recorded step, so its
    /// outcome never decides what the run journals (FIG-4346): a reload that
    /// failed stops the drain [`CommandDrainStop::Headless`] before the next
    /// recorded read, and the run reads on headless. So does a session that
    /// retired under an execution the run read, whose settlement and commit write
    /// nothing to the journal.
    ///
    /// A host task runs as its own operation run (K8, binding Q2) on the
    /// session actor, which applies the command run its mail drain hands it.
    pub(super) async fn drain_next_session_command_run(
        &mut self,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &crate::ActorContext,
    ) -> Result<Option<crate::SessionCommandReceipt>, CommandDrainStop> {
        loop {
            if let Err(fault) = self.reload_invalidated_resident_session_state().await {
                return Err(CommandDrainStop::Headless(fault));
            }
            let Some(store) = self.services.store.clone() else {
                return Ok(None);
            };
            let batches = execute_session_command_run_read(
                effect_controller,
                &self.state.session_id,
                lash_core_execution::core_internal::owned_runner_executor(
                    Box::new(ReadSessionCommandRunRunner {
                        store: store.clone(),
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
            // What caused the run: a command that applies alone is the run,
            // and the turns its code queues retain its cause.
            let cause = commands
                .first()
                .map(|(batch, _)| batch.trace_cause.clone())
                .unwrap_or_default();
            let commands = commands
                .into_iter()
                .map(|(_, command)| command.clone())
                .collect::<Vec<_>>();
            let task = matches!(
                commands.as_slice(),
                [crate::SessionCommand::RunPluginTask { .. }]
            );
            // A replayed read may name a run this run already applied. An
            // administrative compaction, a config transaction or a host
            // task runs again: it replays the steps it journaled, then finds
            // its command settled and adopts the head its commit published
            // without committing again (FIG-4258, FIG-4379). Any other
            // command journals nothing, so a settled run is simply passed.
            let journaled = task
                || matches!(
                    commands.as_slice(),
                    [crate::SessionCommand::CompactContext { .. }
                        | crate::SessionCommand::ApplyConfigTransaction { .. }]
                );
            // Only a compaction and a config transaction journal their
            // apply. Any other run settles and commits off the journal, so a
            // session that retired under it leaves the run's next recorded
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
                &cause,
                run.completion(),
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
        cause: &lash_trace::TraceCause,
        completion: crate::QueuedWorkCompletion,
        cancellation: tokio_util::sync::CancellationToken,
        effect_controller: &crate::ActorContext,
    ) -> Result<bool, RuntimeError> {
        // The session actor opens the runtime a command run applies in from
        // the committed head, which builds no plugins. A command that runs
        // against the live session builds it first, as a turn does (ADR 0132
        // §4, FIG-5245); one that only writes the head applies without it.
        if matches!(
            commands.as_slice(),
            [crate::SessionCommand::RefreshToolCatalog { .. }
                | crate::SessionCommand::RunPluginCommand { .. }
                | crate::SessionCommand::RunPluginTask { .. }
                | crate::SessionCommand::CompactContext { .. }]
        ) && let Err(error) = Box::pin(self.materialize_command_session(effect_controller)).await
        {
            if error
                .run_shape_refusal()
                .and_then(super::config_ops::catalog_refusal_of)
                .is_some()
            {
                let tx = self.open_head_attempt(effect_controller).await?;
                let committed = Box::pin(self.commit_host_command(
                    effect_controller,
                    tx,
                    &completion,
                    None,
                    None,
                    |_, _| crate::runtime::SessionCommandOutcome::Failed {
                        refusal: error.into(),
                    },
                ))
                .await?;
                return Ok(!matches!(
                    committed,
                    super::host_commands::CommandCommit::Withdrawn
                ));
            }
            return Err(error);
        }
        // Compaction and host commands apply alone, under their own scope,
        // in the commit that settles them (FIG-4201, FIG-4202).
        match commands.as_slice() {
            [crate::SessionCommand::CompactContext { instructions }] => {
                return Box::pin(self.apply_compact_context_command(
                    instructions.clone(),
                    completion,
                    effect_controller,
                ))
                .await;
            }
            [crate::SessionCommand::ApplyConfigTransaction { transaction }] => {
                return Box::pin(self.apply_config_transaction_command(
                    transaction.as_ref().clone(),
                    completion,
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
                    effect_controller,
                ))
                .await;
            }
            [crate::SessionCommand::RunPluginCommand { name, args }] => {
                return Box::pin(self.apply_plugin_operation_command(
                    super::host_commands::HostPluginOperation::Command,
                    name.clone(),
                    args.clone(),
                    cause.as_parent(),
                    completion,
                    effect_controller,
                ))
                .await;
            }
            [crate::SessionCommand::RunPluginTask { name, args }] => {
                return Box::pin(self.apply_plugin_operation_command(
                    super::host_commands::HostPluginOperation::Task,
                    name.clone(),
                    args.clone(),
                    cause.as_parent(),
                    completion,
                    effect_controller,
                ))
                .await;
            }
            [crate::SessionCommand::OpenAgentFrame { request }] => {
                return Box::pin(self.apply_open_agent_frame_command(
                    request.as_ref().clone(),
                    completion,
                    effect_controller,
                ))
                .await;
            }
            _ => {}
        }
        let has_durable_store = self.services.store.is_some();
        if !has_durable_store {
            return self
                .apply_session_command_after_admission(
                    commands,
                    Some((completion, effect_controller)),
                )
                .await;
        }
        let session_id = self.state.session_id.clone();
        let work_identity = completion
            .batch_ids
            .first()
            .map_or_else(|| "session-command".to_string(), ToString::to_string);
        let admission_policy = self.host.core.control.commit_admission;
        let result: Result<bool, RuntimeCommitAdmissionError> =
            super::run_head_advancing_commit_attempt(
                session_id.clone(),
                work_identity.clone(),
                cancellation,
                admission_policy,
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
                        Some((completion, effect_controller)),
                    )
                    .await
                    .map_err(RuntimeCommitAdmissionError)
                },
            )
            .await;
        result.map_err(|error| error.0)
    }

    /// Apply `commands`; a persisted run's `applied` names its rows and the
    /// session actor's context that commits it.
    async fn apply_session_command_after_admission(
        &mut self,
        commands: Vec<crate::SessionCommand>,
        applied: Option<(crate::QueuedWorkCompletion, &crate::ActorContext)>,
    ) -> Result<bool, RuntimeError> {
        let tx = match applied.as_ref() {
            Some((_, owner)) => Some(self.open_head_attempt(owner).await?),
            None => None,
        };
        if applied.is_none() {
            self.refresh_session_graph_from_store()
                .await
                .map_err(|err| {
                    RuntimeError::new(
                        crate::RuntimeErrorCode::SessionCommandRefresh,
                        err.to_string(),
                    )
                })?;
        }
        debug_assert_eq!(commands.len(), 1, "session commands apply alone");
        for command in commands {
            match command {
                crate::SessionCommand::RefreshToolCatalog { .. } => {
                    self.refresh_session_tool_catalog()
                        .await
                        .map_err(|err| match err {
                            SessionError::Plugin(error) => {
                                crate::RuntimeEffectControllerError::from(error)
                                    .into_runtime_error()
                            }
                            err => RuntimeError::new(
                                crate::RuntimeErrorCode::SessionCommandRefreshTools,
                                err.to_string(),
                            ),
                        })?;
                }
                // The session's command lane applies a persisted command that
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
        if self.services.store.is_none() {
            return Ok(true);
        }
        let Some(((completion, owner), tx)) = applied.zip(tx) else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "persisted session commands are applied by the session's command lane",
            ));
        };
        let operation = completion
            .batch_ids
            .first()
            .map(|batch_id| {
                crate::OperationId::new(
                    self.state.session_operation_scope(batch_id),
                    "session-command",
                )
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
            commit_state.capture_plugin_states(session.plugins(), fleet_format)?;
        }
        let (mut commit, _persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                commit_state,
                operation,
                self.host.core.durability.commit_budget,
                fleet_format,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        commit.applied_commands = Some(completion);
        let committed = super::durable::head_commit::commit(
            owner,
            tx,
            self.code_executor().as_ref(),
            commit,
            self.state.clone(),
            lash_durable::CommitLabel::SESSION_COMMAND,
            self.host.core.tracing.metrics(),
        )
        .await;
        if committed.is_err() {
            self.invalidate_resident_session_state();
        }
        match committed {
            Ok(_) => {
                self.adopt_owner_state(owner).await?;
            }
            // A host withdrew a command since the lane was read: the commit
            // applied nothing, and the lane is read again (FIG-3927 §2.7).
            Err(super::durable::head_commit::HeadCommitError::Store(
                crate::StoreError::SessionCommandWithdrawn { .. },
            )) => return Ok(false),
            Err(error) => return Err(error.into_runtime_error()),
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
    match error {
        SessionError::Store { source, .. } => super::runtime_error_from_store_commit(source),
        SessionError::Plugin(error) => {
            crate::RuntimeEffectControllerError::from(error).into_runtime_error()
        }
        error => RuntimeError::new(
            RuntimeErrorCode::SessionCommandPostShiftRefresh,
            error.to_string(),
        ),
    }
}

pub(in crate::runtime) fn queued_turn_input_store_required() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::StoreCommitFailed,
        "queued turn input requires a persistent runtime store",
    )
}

/// Why a command drain stopped without an answer.
pub(in crate::runtime) enum CommandDrainStop {
    /// The drain holds no current head for its next recorded read: the
    /// resident session could not be reloaded (a deleted session's reload
    /// among them), or the session retired under an execution the run read. The
    /// run reads on headless from its next read.
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
/// The first execution reads the lane live. A replay of the run reads
/// back the run it recorded, even after the commit that applied it settled
/// the lane: an administrative compaction replays the base and the summary
/// it journaled and adopts its settled commit, and any other settled run is
/// passed. The live lane would skip the settled command and run the run's
/// next steps where its journal holds the compaction's.
pub(in crate::runtime) async fn execute_session_command_run_read(
    controller: &crate::ActorContext,
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
        .session_effect(
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

/// The first execution of one `ReadSessionCommandRun` step: the live read of
/// the command lane (FIG-4201).
struct ReadSessionCommandRunRunner {
    store: crate::store::SessionStore,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for ReadSessionCommandRunRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::ReadSessionCommandRun { .. } = &envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "session-command-meter executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        match self.store.open_session_command_run().await {
            Ok(batches) => Ok(crate::RuntimeEffectOutcome::ReadSessionCommandRun { batches }),
            // A store that did not answer is this attempt's fault.
            Err(error) => Err(crate::RuntimeEffectControllerError::from(
                super::runtime_error_from_store_commit(error),
            )
            .retryable_uncommitted_derivation()),
        }
    }
}

/// Accept a command through the durable queue without borrowing a runtime.
pub(in crate::runtime) async fn enqueue_stored_session_command(
    store: &crate::store::SessionStore,
    session_id: &SessionId,
    command: crate::SessionCommand,
    idempotency_key: &str,
    parent: Option<lash_trace::TraceParent>,
    max_ingress_bytes: std::num::NonZeroU64,
) -> Result<crate::SessionCommandReceipt, SessionCommandEnqueueError> {
    crate::RuntimeError::validate_host_payload("session_command", &command, max_ingress_bytes)?;
    if store.session_id() != session_id {
        return Err(super::runtime_error_from_store_commit(
            crate::StoreError::ForeignSessionRequest {
                view_session_id: store.session_id().clone(),
                request_session_id: session_id.clone(),
            },
        )
        .into());
    }
    if idempotency_key.trim().is_empty() {
        return Err(RuntimeError::new(
            RuntimeErrorCode::SessionCommandIdempotencyKey,
            "session command idempotency key cannot be empty",
        )
        .into());
    }
    let source_key = command.source_key(idempotency_key);
    let draft = crate::QueuedWorkBatchDraft::new(
        session_id.clone(),
        crate::DeliveryPolicy::AfterCurrentTurnCommit,
        command,
    )
    .with_source_key(source_key.clone())
    .with_trace_parent(parent);
    let enqueued = match store.enqueue_queued_work_with_outcome(draft).await {
        Ok(
            crate::QueuedWorkEnqueueOutcome::Inserted(batch)
            | crate::QueuedWorkEnqueueOutcome::Existing(batch),
        ) => batch,
        Err(error @ crate::StoreError::QueuedWorkSourceKeyConflict { .. }) => {
            return Err(SessionCommandEnqueueError::ChangedContent(
                super::runtime_error_from_store_commit(error),
            ));
        }
        Err(error) => return Err(super::runtime_error_from_store_commit(error).into()),
    };

    // Acceptance and the actor wake share the producer transaction.
    Ok(crate::SessionCommandReceipt {
        session_id: session_id.clone(),
        batch_id: enqueued.batch_id,
        source_key,
    })
}

/// Probe completion once, closing the completion/queue race with a recheck.
pub(in crate::runtime) async fn stored_session_command_status(
    store: &crate::store::SessionStore,
    receipt: crate::SessionCommandReceipt,
) -> Result<crate::runtime::SessionCommandSettlement, RuntimeError> {
    if store.session_id() != receipt.session_id {
        return Err(super::runtime_error_from_store_commit(
            crate::StoreError::ForeignSessionRequest {
                view_session_id: store.session_id().clone(),
                request_session_id: receipt.session_id,
            },
        ));
    }
    // A recorded answer needs neither the open queue nor session state.
    // Probe it first so unrelated queued documents are never loaded for
    // an already settled command.
    let mut completion = store
        .queued_work_batch_completion(&receipt.batch_id)
        .await
        .map_err(super::runtime_error_from_store_commit)?;
    if completion.is_none() {
        let still_pending = store
            .list_queued_work()
            .await
            .map_err(super::runtime_error_from_store_commit)?
            .iter()
            .any(|batch| batch.batch_id == receipt.batch_id);
        if still_pending {
            return Ok(crate::runtime::SessionCommandSettlement::Pending(receipt));
        }
        // A settlement can commit between the first probe and the queue
        // read. Read its evidence again before calling it cancelled.
        completion = store
            .queued_work_batch_completion(&receipt.batch_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
    }
    let Some(completion) = completion else {
        return Ok(crate::runtime::SessionCommandSettlement::Cancelled(receipt));
    };
    Ok(
        match completion.command_outcomes.get(&receipt.batch_id).cloned() {
            Some(outcome) => crate::runtime::SessionCommandSettlement::Applied { receipt, outcome },
            None => crate::runtime::SessionCommandSettlement::Durable(receipt),
        },
    )
}
