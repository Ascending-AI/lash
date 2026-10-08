use crate::support::{
    Arc, EmbedError, InputItem, LashCore, LashRuntime, PluginMessage, Result, RuntimeHandle,
    RuntimeSessionState, SessionError, SessionStateService, ToolManifest, ToolRestoreReport,
    ToolState, TurnInput,
};
use lash_core::ActorContext;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
// `PluginQuery` / `PluginCommand` / `PluginTask` bound the operation runners
// below, but their home is `crate::plugins`: authoring surface a plugin
// implements, not a name a host writes to invoke one (ADR 0051, FIG-1921).
pub use lash_core::facade_support::AcceptedInjectedTurnInput;
pub use lash_core::waits::PinnedEngineKey;
pub use lash_core::{CallOwner, ParkedCall};

#[derive(Clone)]
pub struct Completions {
    pub(crate) core: LashCore,
}

impl Completions {
    /// Snapshot the owner's pending admitted calls and their pinned identities and deadlines.
    ///
    /// Returned keys are bearer capabilities. The host authorizes this read and
    /// shares keys only with trusted callers. A concurrent resolver may settle a
    /// returned key, so callers handle `AlreadyResolved`, `Conflict` and `Revoked`.
    pub async fn parked(&self, owner: CallOwner) -> Result<Vec<ParkedCall>> {
        lash_core::parked(self.core.env.core.control.effect_host.backend(), owner)
            .await
            .map_err(EmbedError::from)
    }

    /// Snapshot the pending keys `process`'s engine pinned, with the name it pinned each under
    /// and its deadline.
    ///
    /// The keys are read from the process's durable waits, so any node answers, before and
    /// after a restart or a handover. They are bearer capabilities under the same rules as
    /// [`parked`](Self::parked).
    pub async fn pinned_keys(&self, process: &ProcessId) -> Result<Vec<PinnedEngineKey>> {
        lash_core::waits::pinned_keys(self.core.env.core.control.effect_host.backend(), process)
            .await
            .map_err(EmbedError::from)
    }

    /// Resolve the wait `key` names, first writer wins.
    ///
    /// A completion key is its wait's random 128-bit id, and a bearer
    /// capability: whoever holds it may resolve its wait. Lash keeps no
    /// completion secret and does not decide who may call this; the host
    /// authenticates and authorizes its callers (its API authentication, its
    /// webhook signatures) and hands a key only to callers it has
    /// authorized.
    ///
    /// A key that names no wait answers `Unknown`; a wait that was revoked or
    /// timed out answers `Revoked`; a key of a kind a host may not resolve
    /// (anything but `tool_completion` and `engine_key`) answers `ReservedKind`.
    /// None of them writes anything.
    pub async fn resolve(
        &self,
        key: &str,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveAnswer> {
        let backend = self.core.env.core.control.effect_host.backend();
        lash_core::waits::resolve_host(backend, key, resolution)
            .await
            .map_err(|error| durable_error(error.to_string()))
    }
}

fn durable_error(message: String) -> EmbedError {
    EmbedError::Runtime(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::EngineAwaitEventResolve,
        message,
    ))
}

/// A host mutation either applied or was durably accepted for later settlement.
/// A pending receipt can be followed through [`SessionCommandAdmin::settle`].
#[derive(Clone, Debug, PartialEq)]
pub enum AdminMutation<T> {
    Applied(T),
    Pending(lash_core::runtime::SessionCommandReceipt),
}

impl<T> AdminMutation<T> {
    /// Explicitly await this mutation, decoding its recorded command outcome.
    /// The host can wrap this wait in its own timeout; dropping it withdraws nothing.
    pub async fn settle_with(
        self,
        commands: &SessionCommandAdmin,
        decode: impl FnOnce(lash_core::runtime::SessionCommandOutcome) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Applied(value) => Ok(value),
            Self::Pending(receipt) => match commands.settle(receipt).await? {
                lash_core::runtime::SessionCommandSettlement::Applied { outcome, .. } => {
                    decode(outcome)
                }
                settlement => Err(unsettled_command_error(settlement)),
            },
        }
    }

    fn try_map<U>(self, map: impl FnOnce(T) -> Result<U>) -> Result<AdminMutation<U>> {
        match self {
            Self::Applied(value) => map(value).map(AdminMutation::Applied),
            Self::Pending(receipt) => Ok(AdminMutation::Pending(receipt)),
        }
    }
}

#[derive(Clone)]
/// Facade handle for session administration.
pub struct SessionAdmin {
    pub(crate) target: crate::send::SendTarget,
    pub(crate) runtime: RuntimeHandle,
    pub(crate) process_work: Arc<dyn lash_core::ProcessWorkSubstrate>,
}

/// The first and the longest pause between a pending command's settlement
/// reads.
const COMMAND_SETTLEMENT_POLL_FLOOR: std::time::Duration = std::time::Duration::from_millis(25);
const COMMAND_SETTLEMENT_POLL_CEILING: std::time::Duration = std::time::Duration::from_secs(1);

impl SessionAdmin {
    pub fn config(&self) -> SessionConfigAdmin {
        SessionConfigAdmin {
            control: self.clone(),
        }
    }

    /// The session's prompt sections: the recorded plan, the registered
    /// catalog and an unadmitted preview of their resolution (ADR 0133).
    pub fn prompt(&self) -> SessionPromptAdmin {
        SessionPromptAdmin {
            control: self.clone(),
        }
    }

    pub fn tools(&self) -> ToolAdmin {
        ToolAdmin {
            control: self.clone(),
        }
    }

    pub fn commands(&self) -> SessionCommandAdmin {
        SessionCommandAdmin {
            control: self.clone(),
        }
    }

    pub fn state(&self) -> SessionStateAdmin {
        SessionStateAdmin {
            control: self.clone(),
        }
    }

    /// Returns the turn-input injection administration facade.
    pub fn injection(&self) -> InjectionAdmin {
        InjectionAdmin {
            control: self.clone(),
        }
    }

    pub fn protocol(&self) -> ProtocolAdmin {
        ProtocolAdmin {
            control: self.clone(),
        }
    }

    pub fn processes(&self) -> SessionProcessAdmin {
        SessionProcessAdmin {
            control: self.clone(),
        }
    }

    /// The body is the canonical `lock → call → publish_from` stamp shared by nearly every
    /// mutating control method; publish happens unconditionally once the closure returns.
    async fn with_writer<F, T>(&self, f: F) -> T
    where
        F: AsyncFnOnce(&mut LashRuntime) -> T,
    {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let value = f(&mut runtime).await;
        self.runtime.publish_from(&runtime).await;
        value
    }

    /// Wait for the session actor to apply the command `receipt` names, then
    /// read how it settled. The settlement is read from the store, with the
    /// writer released between reads. The host owns any timeout around this wait.
    async fn await_command_settlement(
        &self,
        receipt: lash_core::runtime::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        let mut pause = COMMAND_SETTLEMENT_POLL_FLOOR;
        loop {
            let settlement = {
                let writer = self.runtime.writer();
                let mut runtime = writer.lock().await;
                let settlement = runtime
                    .settle_session_command(receipt.clone())
                    .await
                    .map_err(EmbedError::from)?;
                self.runtime.publish_from(&runtime).await;
                settlement
            };
            let pending = matches!(
                settlement,
                lash_core::runtime::SessionCommandSettlement::Pending(_)
            );
            if !pending {
                return Ok(settlement);
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(COMMAND_SETTLEMENT_POLL_CEILING);
        }
    }

    async fn command_status(
        &self,
        receipt: lash_core::runtime::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let status = runtime
            .settle_session_command(receipt)
            .await
            .map_err(EmbedError::from)?;
        self.runtime.publish_from(&runtime).await;
        Ok(status)
    }

    async fn export_state(&self) -> lash_core::SessionSnapshot {
        self.runtime.observe().read_view.to_snapshot()
    }

    async fn append_messages(
        &self,
        messages: Vec<PluginMessage>,
        idempotency_key: String,
    ) -> Result<AdminMutation<()>> {
        Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: idempotency_key,
                nodes: messages
                    .into_iter()
                    .map(lash_core::SessionAppendNode::message)
                    .collect(),
                requires_ancestor_node_id: None,
            }),
        )
        .await
        .and_then(|outcome| outcome.try_map(|_| Ok(())))
    }

    async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
        idempotency_key: String,
    ) -> Result<AdminMutation<()>> {
        Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: idempotency_key,
                nodes: vec![lash_core::SessionAppendNode::plugin(plugin_type, body)],
                requires_ancestor_node_id: None,
            }),
        )
        .await
        .and_then(|outcome| outcome.try_map(|_| Ok(())))
    }

    #[cfg(any(test, feature = "testing"))]
    async fn set_persisted_state(&self, state: RuntimeSessionState) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.apply_persistence_state(state).map_err(Into::into)
        })
        .await
    }

    /// Record `extension` durably (FIG-5134): its session nodes are a host
    /// append the command lane applies, so its protocol reads them when the
    /// append lands and replays them on every rebuild of the session.
    async fn apply_protocol_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtension,
        idempotency_key: String,
    ) -> Result<AdminMutation<()>> {
        let fleet = self
            .runtime
            .observe()
            .queue_store
            .as_ref()
            .map(lash_core::store::SessionStore::fleet_format)
            .ok_or_else(|| {
                EmbedError::Session(SessionError::Protocol(
                    "a session extension is recorded on a store-backed session".to_string(),
                ))
            })?;
        match Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: idempotency_key,
                nodes: extension.session_nodes(fleet),
                requires_ancestor_node_id: None,
            }),
        )
        .await?
        {
            AdminMutation::Applied(lash_core::AppendSessionNodesOutcome::Appended { .. }) => {
                Ok(AdminMutation::Applied(()))
            }
            AdminMutation::Pending(receipt) => Ok(AdminMutation::Pending(receipt)),
            outcome => Err(EmbedError::Session(SessionError::Protocol(format!(
                "a session extension requires no ancestor, yet its append settled {outcome:?}"
            )))),
        }
    }

    /// Refresh the session graph from durable state
    /// changed. This is the honest name for what the core `await_background_work`
    /// call does — a session-graph resync, **not** a terminal wait on background
    /// work (that lives on the process admin's `await_output`). Renamed off the
    /// old `SessionProcessAdmin::await_all` misnomer per the ADR 0014 grill.
    pub(crate) async fn refresh_background_graph(&self) -> Result<()> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.await_background_work().await.map_err(Into::into)
        })
        .await
    }

    fn process_registry(&self) -> Result<Arc<dyn lash_core::ProcessRegistry>> {
        self.runtime
            .observe()
            .process_registry
            .clone()
            .ok_or_else(|| {
                EmbedError::Plugin(lash_core::PluginError::Session(
                    "process registry is unavailable in this runtime".to_string(),
                ))
            })
    }

    /// An observer over the session's process registry, or `None` when this
    /// runtime has no registry. Session-scoped reads use this so a registry-less
    /// runtime observes an empty process set rather than erroring, matching the
    /// pre-unification `list_process_handles` behavior.
    fn process_observer_opt(&self) -> Option<lash_core::facade_support::ProcessWorkObserver> {
        self.runtime
            .observe()
            .process_registry
            .clone()
            .map(lash_core::facade_support::ProcessWorkObserver::new)
    }

    /// Observer edges are session-scoped and deliberately frame-less.
    fn process_observer_scope(&self) -> lash_core::SessionScope {
        self.runtime.observe().process_scope()
    }

    async fn transfer_process_handles(
        &self,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scoped_effect_controller: ActorContext,
    ) -> Result<()> {
        let (session_id, processes) = {
            let writer = self.runtime.writer();
            let runtime = writer.lock().await;
            (
                SessionId::from(runtime.session_id()),
                runtime.process_service()?,
            )
        };
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        processes
            .transfer(&session_id, to_session_id, process_ids, scope)
            .await
            .map_err(EmbedError::Plugin)
    }

    async fn await_process_output(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core::ProcessAwaitOutput> {
        let process_work = &self.process_work;
        let process_id = self
            .process_registry()?
            .require_process_id(process_id)
            .await?;
        loop {
            match process_work.await_process_terminal(&process_id).await? {
                lash_core::ProcessTerminalWait::Terminal(output) => return Ok(output),
                lash_core::ProcessTerminalWait::Reattach => {}
            }
        }
    }

    async fn submit_session_command(
        &self,
        command: lash_core::facade_support::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::facade_support::SessionCommandReceipt> {
        let idempotency_key = idempotency_key.into();
        self.with_writer(async |runtime: &mut LashRuntime| {
            Box::pin(runtime.submit_session_command(command, idempotency_key))
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn query_plugin_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(String, serde_json::Value)> {
        let observation = self.runtime.observe();
        let session_id = SessionId::from(observation.session_id());
        observation
            .query_plugin(name, args, Some(session_id))
            .await
            .map_err(Into::into)
    }

    async fn record_plugin_operation_observations(
        &self,
        events: &[lash_core::facade_support::PluginOwned<lash_core::PluginRuntimeEvent>],
        pending_turn_inputs: &[lash_core::PendingTurnInput],
    ) {
        for owned in events {
            self.runtime
                .record_turn_activity(
                    None,
                    lash_core::TurnActivity::independent(lash_core::TurnEvent::PluginRuntime {
                        plugin_id: owned.plugin_id.clone(),
                        event: owned.value.clone(),
                    }),
                )
                .await;
        }
        if !pending_turn_inputs.is_empty() {
            self.runtime
                .record_queue_changed(
                    lash_core::SessionQueueEventKind::Enqueued,
                    pending_turn_inputs
                        .iter()
                        .map(|input| input.input_id.to_string())
                        .collect(),
                )
                .await;
        }
    }

    /// Submit an administrative compaction to the session's command lane and
    /// return its status (FIG-4201). The writer is held only to submit:
    /// the engine's shift applies the command at the next turn boundary, on
    /// whichever runtime works the session, and the submitter reads the
    /// outcome that shift committed.
    ///
    /// Every facade session is catalog-backed and submits through this lane
    /// under its immutable session binding (ADR 0088).
    async fn compact_context(
        &self,
        instructions: Option<String>,
        idempotency_key: String,
    ) -> Result<AdminMutation<bool>> {
        let submitted = self
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    return Box::pin(runtime.submit_session_command(
                        lash_core::facade_support::SessionCommand::CompactContext { instructions },
                        idempotency_key,
                    ))
                    .await
                    .map(SubmittedCommand::Queued)
                    .map_err(EmbedError::Runtime);
                }
                let host = runtime.effect_host();
                let controller = host
                    .scoped(lash_core::AdmittedScope::session_operation(
                        SessionId::from(runtime.session_id()),
                        idempotency_key,
                    ))
                    .map_err(EmbedError::Runtime)?;
                Box::pin(runtime.compact_storeless_context(instructions, controller))
                    .await
                    .map(SubmittedCommand::Applied)
                    .map_err(EmbedError::Runtime)
            })
            .await?;
        let outcome = match submitted {
            SubmittedCommand::Applied(outcome) => outcome,
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(self.command_status(receipt)).await? {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::CompactContext { outcome },
                        ..
                    } => outcome,
                    lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
                        return Ok(AdminMutation::Pending(receipt));
                    }
                    settlement => return Err(unsettled_command_error(settlement)),
                }
            }
        };
        match outcome {
            lash_core::runtime::CompactContextOutcome::Opened { .. } => {
                Ok(AdminMutation::Applied(true))
            }
            lash_core::runtime::CompactContextOutcome::NothingToCompact => {
                Ok(AdminMutation::Applied(false))
            }
            lash_core::runtime::CompactContextOutcome::Failed { refusal } => {
                Err(EmbedError::Runtime(refusal.into()))
            }
        }
    }

    async fn persist_current_state(&self) -> Result<RuntimeSessionState> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime.await_background_work().await?;
            runtime.export_persisted_state().await.map_err(Into::into)
        })
        .await
    }

    async fn start_process(
        &self,
        request: lash_core::ProcessStartRequest,
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_core::ProcessHandleView> {
        let (session_id, processes) = {
            let writer = self.runtime.writer();
            let runtime = writer.lock().await;
            (
                SessionId::from(runtime.session_id()),
                runtime.process_service()?,
            )
        };
        let request = request
            .keyed_in(&scoped_effect_controller)
            .map_err(EmbedError::Plugin)?;
        let start_key = request.start_key().cloned();
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summary = processes
            .start_from_request(&session_id, request, scope)
            .await
            .map_err(|error| {
                EmbedError::Plugin(match error {
                    lash_core::PluginError::RuntimeEffectController(error) => {
                        crate::process_admin::host_start_refusal(start_key.as_ref(), error)
                    }
                    lash_core::PluginError::Runtime(error) => {
                        crate::process_admin::host_start_refusal(start_key.as_ref(), error.into())
                    }
                    error => error,
                })
            })?;
        Ok(summary)
    }

    async fn session_state_service(&self) -> Result<Arc<dyn SessionStateService>> {
        self.runtime
            .writer()
            .lock()
            .await
            .session_state_service()
            .map_err(Into::into)
    }

    async fn cancel_process(
        &self,
        process_id: &ProcessId,
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_core::ProcessCancelReceipt> {
        let (owner, processes) = {
            let writer = self.runtime.writer();
            let runtime = writer.lock().await;
            (
                lash_core::RuntimeOwner::Session(SessionId::from(runtime.session_id())),
                runtime.process_service()?,
            )
        };
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        processes
            .validate_visible(&owner, std::slice::from_ref(process_id), scope.clone())
            .await
            .map_err(EmbedError::Plugin)?;
        let summary = processes
            .cancel(&owner, process_id, scope)
            .await
            .and_then(lash_core::ProcessCancelReceipt::from_record)
            .map_err(EmbedError::Plugin)?;
        Ok(summary)
    }

    async fn cancel_visible_processes(
        &self,
        scoped_effect_controller: ActorContext,
    ) -> Result<Vec<lash_core::ProcessCancelReceipt>> {
        let (session_id, processes) = {
            let writer = self.runtime.writer();
            let runtime = writer.lock().await;
            (
                SessionId::from(runtime.session_id()),
                runtime.process_service()?,
            )
        };
        let scope = lash_core::ProcessOpScope::new(scoped_effect_controller);
        let summaries = processes
            .cancel_all_visible(&session_id, scope)
            .await
            .map_err(EmbedError::Plugin)?;
        Ok(summaries)
    }

    /// The execution state the session's durable head recorded: its
    /// protocol's root and leaves, read from the head's checkpoint without
    /// building the session's capabilities (FIG-5139). `None` when the head
    /// records none: the session never ran a code-executing turn, or its
    /// current frame cleared it.
    async fn snapshot_execution_state(
        &self,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>> {
        let store = self.head_store()?;
        let Some(loaded) = lash_core::store::load_session_window_state(
            &store,
            lash_core::store::WindowSelector::Current,
        )
        .await
        .map_err(EmbedError::Store)?
        else {
            return Ok(None);
        };
        loaded
            .state
            .execution_state_hydration()
            .map_err(EmbedError::Store)
    }

    async fn set_tool_membership(
        &self,
        tool_id: lash_core::ToolId,
        present: bool,
        idempotency_key: String,
    ) -> Result<AdminMutation<u64>> {
        self.set_tool_membership_many(&[(tool_id, present)], idempotency_key)
            .await
    }

    async fn inject_turn_input(
        &self,
        turn_id: &TurnId,
        id: Option<String>,
        message: PluginMessage,
    ) -> Result<()> {
        self.inject_turn_inputs_for_turn(
            turn_id,
            vec![lash_core::facade_support::InjectedTurnInput { id, message }],
        )
        .await
    }

    async fn inject_turn_inputs_for_turn(
        &self,
        turn_id: &TurnId,
        messages: Vec<lash_core::facade_support::InjectedTurnInput>,
    ) -> Result<()> {
        for input in messages {
            let source_key = input.id.map(|id| format!("injection:{id}"));
            let turn_input = turn_input_from_plugin_message(input.message);
            self.runtime
                .enqueue_turn_input(
                    turn_input,
                    lash_core::TurnInputIngress::active_turn(
                        turn_id,
                        lash_core::TurnInputCheckpointBoundary::AfterWork,
                    ),
                    source_key,
                )
                .await
                .map(|_| ())
                .map_err(EmbedError::Runtime)?;
        }
        Ok(())
    }
}

fn turn_input_from_plugin_message(message: PluginMessage) -> TurnInput {
    let mut input = TurnInput::empty();
    for part in message.parts {
        if let Some(attachment) = part.attachment() {
            input
                .items
                .push(InputItem::attachment(attachment.reference.clone()));
        } else if !part.content().is_empty() {
            input.items.push(InputItem::text(part.content()));
        }
    }
    input
}

#[derive(Clone)]
/// Facade handle for session config administration.
pub struct SessionConfigAdmin {
    control: SessionAdmin,
}

#[derive(Clone)]
/// Facade handle for tool administration.
pub struct ToolAdmin {
    control: SessionAdmin,
}

impl ToolAdmin {
    /// The session's tool state as its durable records state it, read
    /// without building the session's capabilities (FIG-5134): what the
    /// durable head recorded, and the changes this admin's writers submitted
    /// that no run has applied yet.
    pub async fn state(&self) -> Result<SessionToolState> {
        self.control.tool_state().await
    }

    pub fn advanced(&self) -> AdvancedToolAdmin {
        AdvancedToolAdmin {
            control: self.control.clone(),
        }
    }

    /// Toggle Tool Catalog membership for a tool. `present` adds it as a
    /// member; `!present` removes it. Membership is the execution gate.
    ///
    /// The change is a durable session command the next command run applies
    /// against the capabilities its transition builds (FIG-5134); this awaits
    /// its settlement and answers the generation it landed at. A tool id the
    /// head's recorded tool state does not name is refused at once, with a
    /// typed [`ReconfigureError`](crate::tools::ReconfigureError), and nothing
    /// is submitted.
    pub async fn set_membership(
        &self,
        tool_id: impl Into<lash_core::ToolId>,
        present: bool,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<u64>> {
        self.control
            .set_tool_membership(tool_id.into(), present, idempotency_key.into())
            .await
    }

    /// Applies multiple tool-membership updates atomically, as one durable
    /// command; see [`Self::set_membership`].
    pub async fn set_membership_many(
        &self,
        updates: &[(lash_core::ToolId, bool)],
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<u64>> {
        self.control
            .set_tool_membership_many(updates, idempotency_key.into())
            .await
    }

    /// The manifests of the Tool Catalog members the session's durable head
    /// recorded; empty before a run recorded any.
    pub async fn active_manifests(&self) -> Result<Vec<ToolManifest>> {
        self.control.active_tool_manifests().await
    }
}

#[derive(Clone)]
/// Facade handle for advanced tool administration.
pub struct AdvancedToolAdmin {
    control: SessionAdmin,
}

impl AdvancedToolAdmin {
    /// Replace the entire tool-state snapshot.
    ///
    /// This is a generation-checked escape hatch for hosts that intentionally
    /// edit the full snapshot. Prefer `ToolAdmin` membership methods for
    /// ordinary tool policy changes. Like them it is a durable command,
    /// returning its status; a snapshot whose generation is not the
    /// head's recorded one is refused at once with
    /// [`ReconfigureError::GenerationMismatch`](crate::tools::ReconfigureError::GenerationMismatch).
    pub async fn apply_state(
        &self,
        state: ToolState,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<u64>> {
        self.control
            .apply_tool_state(state, idempotency_key.into())
            .await
    }

    /// Restore a persisted tool-state snapshot, adopting its generation.
    ///
    /// Use this when re-applying a snapshot read from durable storage (session
    /// resume), not an edited delta: it reconstructs the exact persisted surface
    /// idempotently rather than requiring the snapshot to match the current
    /// generation. A cold resume of a session whose surface reached generation
    /// ≥ 2 needs this — [`apply_state`](Self::apply_state) would reject it.
    ///
    /// Persisted tools whose source is not currently registered (e.g. a
    /// detached MCP server) do not fail the restore: they are kept as orphaned
    /// non-members, listed in the returned [`ToolRestoreReport`], and rebind
    /// automatically when a source re-advertises the same tool. The restore is
    /// a durable command returning its status, and never refuses under
    /// [`ToolSourcePolicy::Require`](crate::tools::ToolSourcePolicy::Require).
    pub async fn restore_state(
        &self,
        state: ToolState,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<ToolRestoreReport>> {
        self.control
            .restore_tool_state(state, idempotency_key.into())
            .await
    }
}

#[derive(Clone)]
/// Facade handle for session command administration.
pub struct SessionCommandAdmin {
    control: SessionAdmin,
}

impl SessionCommandAdmin {
    /// Submit `command` to the session's command lane under a stable
    /// `idempotency_key` and return its durable receipt, before it applies
    /// (FIG-4202). The session's shift applies it at a turn boundary; a
    /// resubmission under the same key names the same command. Await its
    /// outcome with [`Self::settle`], from this handle or any other that holds
    /// the receipt.
    pub async fn submit(
        &self,
        command: lash_core::facade_support::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::facade_support::SessionCommandReceipt> {
        self.control
            .submit_session_command(command, idempotency_key)
            .await
    }

    /// Await the settlement of the command `receipt` names, reattaching by
    /// its receipt (FIG-4202): applied with its typed outcome, a refusal
    /// among them; or cancelled when it was withdrawn. Dropping this
    /// await withdraws nothing: the command stays durable and settles.
    pub async fn settle(
        &self,
        receipt: lash_core::facade_support::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        Box::pin(self.control.await_command_settlement(receipt)).await
    }

    /// Withdraw the command `receipt` names (FIG-4202). A command no shift
    /// has admitted is withdrawn transactionally and never applies; one a
    /// shift already read, or that already settled, answers
    /// [`SessionCommandWithdrawal::AlreadyAdmitted`] and settles as that
    /// shift applies it.
    pub async fn withdraw(
        &self,
        receipt: &lash_core::facade_support::SessionCommandReceipt,
    ) -> Result<SessionCommandWithdrawal> {
        self.control.withdraw_session_command(receipt).await
    }

    /// The command drains asynchronously and recomputes the surface from live sources, so it
    /// takes no generation guard — any generation observed at enqueue time could legitimately
    /// have advanced by drain time.
    pub async fn refresh_tool_catalog(
        &self,
        reason: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::facade_support::SessionCommandReceipt> {
        self.control
            .submit_session_command(
                lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                    reason: reason.into(),
                },
                idempotency_key,
            )
            .await
    }
}

#[derive(Clone)]
/// Facade handle for session process administration.
pub struct SessionProcessAdmin {
    control: SessionAdmin,
}

mod process_admin;

pub(crate) mod config_transactions;
mod host_commands;
pub(crate) mod prompt;
pub(crate) use prompt::SessionPromptAdmin;
mod tool_state;
use host_commands::{SubmittedCommand, unsettled_command_error};
pub use tool_state::{PendingToolStateChange, SessionToolState};

/// What withdrawing a submitted session command did (FIG-4202).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommandWithdrawal {
    /// No shift had admitted the command: it is withdrawn, transactionally,
    /// and never applies.
    Withdrawn,
    /// A shift already read the command, or it already settled: the
    /// withdrawal lost the race, and the command settles as that execute
    /// applies it.
    AlreadyAdmitted,
}

#[derive(Clone)]
/// Facade handle for session state administration.
pub struct SessionStateAdmin {
    control: SessionAdmin,
}

impl SessionStateAdmin {
    /// Exports the session's current state as a snapshot.
    pub async fn export(&self) -> lash_core::SessionSnapshot {
        self.control.export_state().await
    }

    pub async fn append_messages(
        &self,
        messages: Vec<PluginMessage>,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<()>> {
        Box::pin(
            self.control
                .append_messages(messages, idempotency_key.into()),
        )
        .await
    }

    pub async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<()>> {
        Box::pin(
            self.control
                .append_plugin_body(plugin_type, body, idempotency_key.into()),
        )
        .await
    }

    /// Submit a keyed append to the session command lane. A pending result
    /// retains its receipt for a host-chosen settlement wait.
    pub async fn append_session_nodes(
        &self,
        request: lash_core::AppendSessionNodesRequest,
    ) -> Result<AdminMutation<lash_core::AppendSessionNodesOutcome>> {
        Box::pin(self.control.append_session_nodes(request)).await
    }

    /// Submit `request`'s frame open durably and return its status
    /// (FIG-4202): a session command, keyed by `idempotency_key`, that the
    /// session's shift opens and commits at the next turn boundary,
    /// restarting its live interpreter from the frame's seed. A refused open
    /// answers its typed runtime error.
    pub async fn open_agent_frame(
        &self,
        request: lash_core::OpenAgentFrameRequest,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<lash_core::OpenAgentFrameOutcome>> {
        Box::pin(
            self.control
                .open_agent_frame(request, idempotency_key.into()),
        )
        .await
    }

    /// Replaces resident state WITHOUT durable publication; test and recovery
    /// tooling only, never a product path.
    #[cfg(any(test, feature = "testing"))]
    pub async fn set_persisted(&self, state: RuntimeSessionState) -> Result<()> {
        self.control.set_persisted_state(state).await
    }

    /// Persists and returns the session's current runtime state.
    pub async fn persist_current(&self) -> Result<RuntimeSessionState> {
        self.control.persist_current_state().await
    }

    pub async fn session_state_service(&self) -> Result<Arc<dyn SessionStateService>> {
        self.control.session_state_service().await
    }

    /// The protocol execution state (root and leaves) the session's durable
    /// head recorded, read from the store without building the session's
    /// capabilities (FIG-5139): the state the head's last commit captured,
    /// on any process, whether or not a run built this handle's runtime.
    /// `None` when the head records none, which includes a protocol without
    /// a code executor and a frame switch that cleared it.
    ///
    /// Execution state moves only with the session's history: a host seeds
    /// a fresh interpreter by opening a frame with a seed
    /// ([`Self::open_agent_frame`]), never by writing a snapshot over the
    /// head.
    pub async fn snapshot_execution(
        &self,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>> {
        self.control.snapshot_execution_state().await
    }

    /// Submit a keyed compaction. An applied result says whether a frame opened;
    /// a pending result carries the durable receipt for settlement.
    pub async fn compact_context(
        &self,
        instructions: Option<String>,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<bool>> {
        // Boxed at the facade seam: the settlement read adopts a whole head,
        // which puts the inline future past the size bound.
        Box::pin(
            self.control
                .compact_context(instructions, idempotency_key.into()),
        )
        .await
    }
}

mod plugin_operations;
pub use plugin_operations::{PluginOperations, PluginTaskResultError};

#[derive(Clone)]
/// Facade handle for injection administration.
pub struct InjectionAdmin {
    control: SessionAdmin,
}

impl InjectionAdmin {
    /// Injects input for the session's next turn.
    pub async fn inject_turn_input(
        &self,
        turn_id: &TurnId,
        id: Option<String>,
        message: PluginMessage,
    ) -> Result<()> {
        self.control.inject_turn_input(turn_id, id, message).await
    }
}

#[derive(Clone)]
/// Facade handle for protocol administration.
pub struct ProtocolAdmin {
    control: SessionAdmin,
}

impl ProtocolAdmin {
    /// Record a protocol session extension durably (FIG-5134). It is a host
    /// append of the extension's session nodes, applied by the session's
    /// command lane; a pending receipt follows its settlement. The protocol reads the
    /// nodes when they land and replays them whenever it rebuilds the
    /// session, so the extension holds for every later run.
    pub async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtension,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<()>> {
        self.control
            .apply_protocol_session_extension(extension, idempotency_key.into())
            .await
    }
}

#[cfg(test)]
mod injected_message_tests {
    use super::*;

    #[test]
    fn mixed_parts_injection_preserves_order_and_attachments() {
        let source = lash_core::AttachmentRef::new(
            lash_core::AttachmentId::parse(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .expect("digest"),
            "image/png".parse().unwrap(),
            3,
            None,
            None,
        );
        let mut message = PluginMessage::text(lash_core::MessageRole::User, "before");
        message.parts.push(lash_core::Part::attachment_part(
            String::new(),
            String::new(),
            Some(lash_core::session_model::message::PartAttachment {
                reference: source.clone(),
            }),
        ));
        message
            .parts
            .push(lash_core::Part::text(String::new(), "after".into(), None));
        let input = turn_input_from_plugin_message(message);
        assert!(matches!(input.items.as_slice(),
            [InputItem::Text { text: before }, InputItem::Attachment { reference: actual }, InputItem::Text { text: after }]
                if before == "before" && actual == &source && after == "after"));
    }
}
