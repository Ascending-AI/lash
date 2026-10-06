use crate::support::{
    Arc, CancellationToken, EmbedError, InputItem, LashCore, LashRuntime, PluginMessage, Result,
    RuntimeHandle, RuntimeSessionState, SessionError, SessionStateService, ToolManifest,
    ToolRestoreReport, ToolState, TurnInput,
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

#[derive(Clone)]
pub struct Completions {
    pub(crate) core: LashCore,
}

impl Completions {
    /// The completion keys of `session_id`'s unresolved host-resolvable
    /// waits (`tool_completion` and `custom`), rebuilt from their rows under
    /// each row's key version.
    ///
    /// This administrative read is scoped to exactly `session_id`. It returns
    /// a snapshot: another resolver may settle a returned key concurrently,
    /// so callers must handle [`lash_core::ResolveAnswer::AlreadyResolved`],
    /// `Conflict` or `UnknownOrRevoked` from [`Self::resolve`]. A returned
    /// key carries the authority needed to resolve its wait; the caller is
    /// responsible for authorizing this read and the later resolution.
    pub async fn outstanding(&self, session_id: &SessionId) -> Result<Vec<lash_core::PinnedKey>> {
        let backend = self.core.env.core.control.effect_host.backend();
        let owner = lash_core::durable_port::ActorKey::session(session_id.as_str())
            .map_err(|error| durable_error(error.to_string()))?;
        lash_core::waits::outstanding_keys(backend, &owner)
            .await
            .map_err(|error| durable_error(error.to_string()))
    }

    /// Resolve the wait `key` names, first writer wins.
    ///
    /// The key's MAC is verified under the deployment's completion secret of
    /// the version its wait was minted under. A key that does not verify, or
    /// whose wait was revoked, timed out or is unknown, answers
    /// `UnknownOrRevoked`; a verified key of a kind a host may not resolve
    /// answers `ReservedKind`. Neither writes anything.
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

/// Host-scoped trigger surface: emit occurrences and read registrations across
/// every owner scope.
///
/// This is the read and ingress half. Changing a subscription (register,
/// update, enable, disable, delete) goes through
/// [`TriggerCommand`](crate::triggers::TriggerCommand) executed by
/// [`TriggerStore::execute_command`](crate::triggers::TriggerStore::execute_command)
/// on the trigger store the host installed, so the mutation keeps its revision
/// fence and its operation receipt. Never write the store's `lash_*` tables
/// directly.
#[derive(Clone)]
pub struct CoreTriggerAdmin {
    pub(crate) core: LashCore,
}

impl CoreTriggerAdmin {
    fn store(&self) -> Result<Arc<dyn lash_core::TriggerStore>> {
        Ok(self.core.env.core.trigger_store())
    }

    pub async fn emit(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_core::facade_support::TriggerEmitReport> {
        // The producer's context is snapshotted here, before the first
        // await, unless the request states its own: the fire links it.
        let request = if request.trace.is_empty() {
            let captured = self.core.env.core.tracing.scopes().capture_current();
            request.with_trace(lash_core::TraceScopeOffer::caused_by(
                lash_core::TraceCause::linked_to(captured),
            ))
        } else {
            request
        };
        let store = self.store()?;
        let ports = self.core.substrate_slot.ports().await;
        let process_work = ports.process;
        let mut router = lash_core::facade_support::TriggerRouter::new(store, process_work)
            .with_process_artifacts(
                Arc::clone(&self.core.env.core.durability.process_env_store),
                self.core.host_process_engines.clone(),
            );
        if let Some(restorer) = &self.core.env.core.control.trigger_route_restorer {
            router = router.with_route_restorer(std::sync::Arc::clone(restorer));
        }
        router
            .emit(request, &scoped_effect_controller)
            .await
            .map_err(Into::into)
    }

    /// Read the latest desired source states in durable change order.
    /// Apply idempotently by subscription id. Within an incarnation, ignore
    /// revisions older than the one already applied. A new incarnation replaces
    /// the prior source. Commit the cursor after applying the page, or atomically
    /// with your own records. Re-reading an earlier cursor is safe.
    ///
    /// On `PluginError::TriggerSubscriptionChangeCursorPruned`, resync through
    /// `subscriptions_snapshot`, remove sources absent from that snapshot, and
    /// continue from its cursor. Hosts send work through the engine as usual.
    pub async fn changed_since(
        &self,
        cursor: lash_core::TriggerSubscriptionChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        crate::ChangePage<
            lash_core::TriggerSubscriptionChange,
            lash_core::TriggerSubscriptionChangeCursor,
        >,
    > {
        let (changes, next) = self
            .store()?
            .subscriptions_changed_since(cursor, limit.get())
            .await?;
        Ok(crate::ChangePage {
            changes,
            next,
            retained_after: None,
        })
    }

    /// Atomically read all live subscriptions and their continuation cursor.
    pub async fn subscriptions_snapshot(
        &self,
    ) -> Result<(
        Vec<lash_core::TriggerSubscriptionRecord>,
        lash_core::TriggerSubscriptionChangeCursor,
    )> {
        self.store()?
            .list_subscriptions_with_cursor()
            .await
            .map_err(Into::into)
    }

    /// Retain tombstones until this host-chosen cutoff. Consumers behind the
    /// removed evidence receive a typed refusal and must resync.
    pub async fn compact_subscription_tombstones(
        &self,
        cutoff: std::time::SystemTime,
    ) -> Result<usize> {
        let cutoff_epoch_ms = cutoff
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| EmbedError::Session(SessionError::Protocol(error.to_string())))?
            .as_millis();
        let cutoff_epoch_ms = u64::try_from(cutoff_epoch_ms)
            .map_err(|error| EmbedError::Session(SessionError::Protocol(error.to_string())))?;
        self.store()?
            .compact_subscription_tombstones(cutoff_epoch_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn subscriptions(
        &self,
        filter: lash_core::TriggerSubscriptionFilter,
    ) -> Result<Vec<lash_core::facade_support::TriggerRegistration>> {
        let store = self.store()?;
        let records = store.list_subscriptions(filter).await?;
        Ok(records
            .iter()
            .map(lash_core::facade_support::TriggerRegistration::from)
            .collect())
    }
}

#[derive(Clone)]
/// Facade handle for session administration.
pub struct SessionAdmin {
    pub(crate) target: crate::send::SendTarget,
    pub(crate) runtime: RuntimeHandle,
    pub(crate) process_work: Arc<dyn lash_core::ProcessWorkSubstrate>,
    pub(crate) work: Arc<dyn lash_core::SessionWorkEngine>,
    pub(crate) ingress: lash_core::shift::IngressRelay,
}

impl SessionAdmin {
    pub fn config(&self) -> SessionConfigAdmin {
        SessionConfigAdmin {
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

    pub fn triggers(&self) -> SessionTriggerAdmin {
        SessionTriggerAdmin {
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

    /// Wait, with the writer released, for the engine shift that applies the
    /// command `receipt` names, then read how it settled. A command the
    /// shift has not settled by the deadline answers `Pending` with its
    /// receipt; the command stays durable and settles later.
    async fn await_command_settlement(
        &self,
        receipt: lash_core::runtime::SessionCommandReceipt,
        previous_policy: Option<lash_core::SessionPolicy>,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        let request = self
            .ingress
            .current_ask(&receipt.session_id, receipt.batch_id.as_str())
            .await
            .map_err(|source| {
                EmbedError::Session(SessionError::Store {
                    context: "failed to read session command shift".into(),
                    source,
                })
            })?;
        if let Some(request) = request {
            let wait = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.work.await_shift(&receipt.session_id, &request),
            )
            .await;
            match wait {
                Ok(Ok(_)) | Err(_) => {}
                Ok(Err(abort)) => return Err(EmbedError::Runtime(abort.into_error())),
            }
        }
        // Cancellation removes the ask and the queued batch together. Read
        // the batch's recorded state after the ask read or shift wait; only a
        // still-open batch may be reported as pending.
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let settlement = match previous_policy {
            Some(previous_policy) => {
                runtime
                    .settle_session_command_from_policy(receipt.clone(), previous_policy)
                    .await
            }
            None => runtime.settle_session_command(receipt.clone()).await,
        }
        .map_err(EmbedError::from)?;
        self.runtime.publish_from(&runtime).await;
        Ok(settlement)
    }

    async fn export_state(&self) -> lash_core::SessionSnapshot {
        self.runtime.observe().read_view.to_snapshot()
    }

    async fn append_messages(&self, messages: Vec<PluginMessage>) -> Result<()> {
        Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: uuid::Uuid::new_v4().to_string(),
                nodes: messages
                    .into_iter()
                    .map(lash_core::SessionAppendNode::message)
                    .collect(),
                requires_ancestor_node_id: None,
            }),
        )
        .await
        .map(|_| ())
    }

    async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
    ) -> Result<()> {
        Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: uuid::Uuid::new_v4().to_string(),
                nodes: vec![lash_core::SessionAppendNode::plugin(plugin_type, body)],
                requires_ancestor_node_id: None,
            }),
        )
        .await
        .map(|_| ())
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
    ) -> Result<()> {
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
                operation_id: format!("session-extension:{}", uuid::Uuid::new_v4()),
                nodes: extension.session_nodes(fleet),
                requires_ancestor_node_id: None,
            }),
        )
        .await?
        {
            lash_core::AppendSessionNodesOutcome::Appended { .. } => Ok(()),
            outcome => Err(EmbedError::Session(SessionError::Protocol(format!(
                "a session extension requires no ancestor, yet its append settled {outcome:?}"
            )))),
        }
    }

    /// Refresh the session graph from any background process that signalled it
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

    async fn signal_process(
        &self,
        process_id: &ProcessId,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_core::ProcessEvent> {
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
        processes
            .signal_possessed(&owner, process_id, signal_name, signal_id, payload, scope)
            .await
            .map_err(EmbedError::Plugin)
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

    async fn list_trigger_registrations(
        &self,
    ) -> Result<Vec<lash_core::facade_support::TriggerRegistration>> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .list_trigger_registrations()
                .await
                .map_err(Into::into)
        })
        .await
    }

    async fn trigger_registrations_by_source_type(
        &self,
        source_type: impl Into<lash_core::facade_support::TriggerEventType>,
    ) -> Result<Vec<lash_core::facade_support::TriggerRegistration>> {
        self.with_writer(async |runtime: &mut LashRuntime| {
            runtime
                .trigger_registrations_by_source_type(source_type)
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
    /// await its settlement (FIG-4201). The writer is held only to submit:
    /// the engine's shift applies the command at the next turn boundary, on
    /// whichever runtime works the session, and the submitter reads the
    /// outcome that shift committed.
    ///
    /// Every facade session is catalog-backed and submits through this lane
    /// under its immutable session binding (ADR 0088).
    async fn compact_context(&self, instructions: Option<String>) -> Result<bool> {
        let submitted = self
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    return Box::pin(runtime.submit_session_command(
                        lash_core::facade_support::SessionCommand::CompactContext { instructions },
                        format!("compact-context:{}", uuid::Uuid::new_v4()),
                    ))
                    .await
                    .map(SubmittedCommand::Queued)
                    .map_err(EmbedError::Runtime);
                }
                let host = runtime.effect_host();
                let controller = host
                    .scoped(lash_core::AdmittedScope::session_operation(
                        SessionId::from(runtime.session_id()),
                        format!("compact-context:{}", uuid::Uuid::new_v4()),
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
                match Box::pin(self.await_command_settlement(receipt, None)).await? {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::CompactContext { outcome },
                        ..
                    } => outcome,
                    settlement => return Err(unsettled_command_error(settlement)),
                }
            }
        };
        match outcome {
            lash_core::runtime::CompactContextOutcome::Opened { .. } => Ok(true),
            lash_core::runtime::CompactContextOutcome::NothingToCompact => Ok(false),
            lash_core::runtime::CompactContextOutcome::Failed { code, message } => Err(
                EmbedError::Runtime(lash_core::RuntimeError::new(code, message)),
            ),
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

    async fn set_tool_membership(&self, tool_id: lash_core::ToolId, present: bool) -> Result<u64> {
        self.set_tool_membership_many(&[(tool_id, present)]).await
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
                .push(InputItem::attachment(attachment.source.clone()));
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
    ) -> Result<u64> {
        self.control
            .set_tool_membership(tool_id.into(), present)
            .await
    }

    /// Applies multiple tool-membership updates atomically, as one durable
    /// command; see [`Self::set_membership`].
    pub async fn set_membership_many(&self, updates: &[(lash_core::ToolId, bool)]) -> Result<u64> {
        self.control.set_tool_membership_many(updates).await
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
    /// awaited to its settlement; a snapshot whose generation is not the
    /// head's recorded one is refused at once with
    /// [`ReconfigureError::GenerationMismatch`](crate::tools::ReconfigureError::GenerationMismatch).
    pub async fn apply_state(&self, state: ToolState) -> Result<u64> {
        self.control.apply_tool_state(state).await
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
    /// a durable command, awaited to its settlement, and never refuses under
    /// [`ToolSourcePolicy::Require`](crate::tools::ToolSourcePolicy::Require).
    pub async fn restore_state(&self, state: ToolState) -> Result<ToolRestoreReport> {
        self.control.restore_tool_state(state).await
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
    /// among them; cancelled when it was withdrawn; or pending, with the
    /// receipt, when the settlement deadline passes first. Dropping this
    /// await withdraws nothing: the command stays durable and settles.
    pub async fn settle(
        &self,
        receipt: lash_core::facade_support::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        Box::pin(self.control.await_command_settlement(receipt, None)).await
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

/// Session-scoped read controls for Lashlang trigger registrations.
#[derive(Clone)]
pub struct SessionTriggerAdmin {
    control: SessionAdmin,
}

impl SessionTriggerAdmin {
    /// This is an admin/introspection view. Source owners should prefer
    /// [`Self::by_source_type`] so they only inspect registrations for the
    /// concrete source type they own.
    pub async fn list_all(&self) -> Result<Vec<lash_core::facade_support::TriggerRegistration>> {
        self.control.list_trigger_registrations().await
    }

    /// This is the source-owner API: a timer, UI, webhook, or other host-owned
    /// source uses it to inspect registrations for keys it may schedule and emit.
    pub async fn by_source_type(
        &self,
        source_type: impl Into<lash_core::facade_support::TriggerEventType>,
    ) -> Result<Vec<lash_core::facade_support::TriggerRegistration>> {
        self.control
            .trigger_registrations_by_source_type(source_type)
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
mod tool_state;
use host_commands::{HostPluginOperation, SubmittedCommand, unsettled_command_error};
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

    pub async fn append_messages(&self, messages: Vec<PluginMessage>) -> Result<()> {
        Box::pin(self.control.append_messages(messages)).await
    }

    pub async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
    ) -> Result<()> {
        Box::pin(self.control.append_plugin_body(plugin_type, body)).await
    }

    /// Append `request`'s nodes to the session graph and await the append's
    /// settlement (FIG-4202).
    ///
    /// The session's bound turn owns its head, so the append is a session
    /// command its shift applies at the next turn boundary, after everything
    /// a running turn commits. The request's `operation_id` is its
    /// idempotency key. It answers
    /// [`StaleBranch`](lash_core::AppendSessionNodesOutcome::StaleBranch)
    /// when its required ancestor left the active path, and a
    /// [`SessionError::SessionCommandPending`](crate::support::SessionError::SessionCommandPending)
    /// with its receipt when the shift has not applied it by the settlement
    /// deadline; the append stays durable and applies later.
    pub async fn append_session_nodes(
        &self,
        request: lash_core::AppendSessionNodesRequest,
    ) -> Result<lash_core::AppendSessionNodesOutcome> {
        Box::pin(self.control.append_session_nodes(request)).await
    }

    /// Open `request`'s frame durably and await the open's settlement
    /// (FIG-4202): a session command, keyed by `idempotency_key`, that the
    /// session's shift opens and commits at the next turn boundary,
    /// restarting its live interpreter from the frame's seed. A refused open
    /// answers its typed runtime error.
    pub async fn open_agent_frame(
        &self,
        request: lash_core::OpenAgentFrameRequest,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::OpenAgentFrameOutcome> {
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

    /// Compacts the session's context: an administrative compaction that
    /// opens a compaction frame seeded with a summary of the frame it leaves.
    ///
    /// The compaction is a session command applied at a turn boundary
    /// (FIG-4201): a turn running when it is submitted finishes first, and
    /// the compaction applies before any input queued after it. The call
    /// awaits the settlement: `true` when the frame opened, `false` when the
    /// compactor found nothing to compact. A compaction that failed answers
    /// its typed runtime error. One the engine has not applied by the
    /// settlement deadline answers
    /// [`SessionError::SessionCommandPending`](crate::support::SessionError::SessionCommandPending)
    /// with its receipt; it stays durable and applies later.
    pub async fn compact_context(&self, instructions: Option<String>) -> Result<bool> {
        // Boxed at the facade seam: the settlement read adopts a whole head,
        // which puts the inline future past the size bound.
        Box::pin(self.control.compact_context(instructions)).await
    }
}

/// A task's decoded operation failure or a facade refusal. The complete
/// failure envelope retains classification, provenance and unknown data.
#[derive(Debug, thiserror::Error)]
pub enum PluginTaskResultError<Error> {
    /// The operation's declared error, decoded using its registered codec.
    #[error("plugin operation failed: {failure}")]
    Failed {
        error: Error,
        failure: Box<lash_core::plugin::PluginOperationFailure>,
    },
    /// Storage, cancellation, protocol, or an unrecognized operation failure.
    #[error(transparent)]
    Host(Box<EmbedError>),
}

impl<Error> From<PluginTaskResultError<Error>> for EmbedError {
    fn from(error: PluginTaskResultError<Error>) -> Self {
        match error {
            PluginTaskResultError::Failed { failure, .. } => Self::Control(
                lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
            ),
            PluginTaskResultError::Host(error) => *error,
        }
    }
}

#[derive(Clone)]
pub struct PluginOperations {
    pub(crate) control: SessionAdmin,
}

impl PluginOperations {
    /// Durably submit a host task under a stable key and return its operation
    /// Run. The engine owns execution; the handle follows, cancels and reads
    /// the result, including after a restart.
    pub async fn start_task<Op: lash_core::facade_support::PluginTask>(
        &self,
        args: Op::Args,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::RunHandle<Op::Output, Op::Error>> {
        Ok(self
            .start_task_raw(Op::NAME, encode_plugin_args::<Op>(args)?, idempotency_key)
            .await?
            .typed::<Op>())
    }

    /// Submit a task by its registered name. Equal key and content reattach
    /// to the same operation Run.
    pub async fn start_task_raw(
        &self,
        name: &str,
        args: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::RunHandle> {
        let receipt = self
            .control
            .submit_session_command(
                lash_core::facade_support::SessionCommand::RunPluginTask {
                    name: name.into(),
                    args,
                },
                idempotency_key,
            )
            .await?;
        let operation = lash_core::tool_run::OperationRun {
            session_id: receipt.session_id,
            operation_id: receipt.batch_id.to_string(),
        };
        Ok(crate::send::run(
            self.control.target.clone(),
            operation.run_id(),
        ))
    }

    /// Run query `Op` over the plugin view a run or command published on
    /// this process. A query is not an admin read: it runs plugin code, so it
    /// needs the session's built plugins and never builds them (FIG-5139).
    /// Where none is published here (the session never ran or commanded on
    /// this process, as on a replica that has not served it) it is refused
    /// with [`PluginOperationInvokeError::NotPublished`](lash_core::facade_support::PluginOperationInvokeError::NotPublished):
    /// publish one with a session command, such as
    /// [`SessionCommandAdmin::refresh_tool_catalog`], then retry.
    pub async fn query<Op: lash_core::facade_support::PluginQuery>(
        &self,
        args: Op::Args,
    ) -> Result<Op::Output> {
        let (_plugin_id, output) = self
            .control
            .query_plugin_raw(Op::NAME, encode_plugin_args::<Op>(args)?)
            .await?;
        decode_plugin_output::<Op>(output)
    }

    /// [`Self::query`] by the query's registered name.
    pub async fn query_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(String, serde_json::Value)> {
        self.control.query_plugin_raw(name, args).await
    }

    pub async fn run_command<Op: lash_core::facade_support::PluginCommand>(
        &self,
        args: Op::Args,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<Op::Output>> {
        let receipt = Box::pin(self.control.run_plugin_operation(
            HostPluginOperation::Command,
            Op::NAME,
            encode_plugin_args::<Op>(args)?,
            CancellationToken::new(),
        ))
        .await?;
        Ok(lash_core::facade_support::PluginOperationReceipt {
            output: decode_plugin_output::<Op>(receipt.output)?,
            events: receipt.events,
            pending_turn_inputs: receipt.pending_turn_inputs,
        })
    }

    pub async fn run_command_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>> {
        Box::pin(self.control.run_plugin_operation(
            HostPluginOperation::Command,
            name,
            args,
            CancellationToken::new(),
        ))
        .await
    }

    pub async fn run_task<Op: lash_core::facade_support::PluginTask>(
        &self,
        args: Op::Args,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<Op::Output>> {
        self.run_task_with_cancel::<Op>(args, CancellationToken::new())
            .await
    }

    /// Invokes a typed task operation with cancellation support.
    ///
    /// Firing `cancellation_token` withdraws a task no shift has admitted,
    /// and requests cancellation of an admitted operation Run. Its recorded
    /// completion decides the outcome; a cancelled task answers
    /// [`crate::SendError::NotSettled`] with [`crate::TurnStatus::Cancelled`].
    pub async fn run_task_with_cancel<Op: lash_core::facade_support::PluginTask>(
        &self,
        args: Op::Args,
        cancellation_token: CancellationToken,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<Op::Output>> {
        let receipt = Box::pin(self.control.run_plugin_operation(
            HostPluginOperation::Task,
            Op::NAME,
            encode_plugin_args::<Op>(args)?,
            cancellation_token,
        ))
        .await?;
        Ok(lash_core::facade_support::PluginOperationReceipt {
            output: decode_plugin_output::<Op>(receipt.output)?,
            events: receipt.events,
            pending_turn_inputs: receipt.pending_turn_inputs,
        })
    }

    pub async fn run_task_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>> {
        self.run_task_raw_with_cancel(name, args, CancellationToken::new())
            .await
    }

    /// Invokes a raw task operation with cancellation support.
    ///
    /// Firing `cancellation_token` withdraws a task no shift has admitted,
    /// and requests cancellation of an admitted operation Run. Its recorded
    /// completion decides the outcome; a cancelled task answers
    /// [`crate::SendError::NotSettled`] with [`crate::TurnStatus::Cancelled`].
    pub async fn run_task_raw_with_cancel(
        &self,
        name: &str,
        args: serde_json::Value,
        cancellation_token: CancellationToken,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>> {
        Box::pin(self.control.run_plugin_operation(
            HostPluginOperation::Task,
            name,
            args,
            cancellation_token,
        ))
        .await
    }
}

fn encode_plugin_args<Op: lash_core::facade_support::PluginOperation>(
    args: Op::Args,
) -> Result<serde_json::Value> {
    serde_json::to_value(args).map_err(|err| {
        EmbedError::Plugin(lash_core::PluginError::Invoke(format!(
            "invalid {} args: {err}",
            Op::NAME
        )))
    })
}

fn decode_plugin_output<Op: lash_core::facade_support::PluginOperation>(
    output: serde_json::Value,
) -> Result<Op::Output> {
    serde_json::from_value(output).map_err(|err| {
        EmbedError::Plugin(lash_core::PluginError::Invoke(format!(
            "invalid {} output: {err}",
            Op::NAME
        )))
    })
}

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
    /// command lane and awaited to its settlement; the protocol reads the
    /// nodes when they land and replays them whenever it rebuilds the
    /// session, so the extension holds for every later run.
    pub async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtension,
    ) -> Result<()> {
        self.control
            .apply_protocol_session_extension(extension)
            .await
    }
}

#[cfg(test)]
mod injected_message_tests {
    use super::*;

    #[test]
    fn mixed_parts_injection_preserves_order_and_attachment_sources() {
        let source = lash_core::AttachmentSource::Inline {
            media_type: "image/png".parse().unwrap(),
            bytes: vec![0, 255, 42],
        };
        let mut message = PluginMessage::text(lash_core::MessageRole::User, "before");
        message.parts.push(lash_core::Part::attachment_part(
            String::new(),
            String::new(),
            Some(lash_core::session_model::message::PartAttachment {
                source: source.clone(),
            }),
        ));
        message
            .parts
            .push(lash_core::Part::text(String::new(), "after".into(), None));
        let input = turn_input_from_plugin_message(message);
        assert!(matches!(input.items.as_slice(),
            [InputItem::Text { text: before }, InputItem::Attachment { source: actual }, InputItem::Text { text: after }]
                if before == "before" && actual == &source && after == "after"));
    }
}
