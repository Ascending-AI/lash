use std::pin::Pin;
use std::task::{Context, Poll};

use crate::support::*;
use futures_util::Stream;
use lash_core::runtime::{
    PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputCancelResult,
    PendingTurnInputCancelTarget, PendingTurnInputSuffixCancelOutcome, QueuedWorkBatch,
    QueuedWorkClaim, TurnInputAcceptanceReceipt, TurnInputClaim, TurnInputIngress,
};
use lash_core::{LiveReplayGap, LiveReplayStoreError, SessionObservationEvent};
use lash_remote_protocol::{
    RemoteLiveReplayGap, RemoteSessionCursor, RemoteSessionObservation,
    RemoteSessionObservationEvent,
};

pub struct SessionBuilder {
    pub(crate) core: LashCore,
    pub(crate) session_id: String,
    pub(crate) spec: SessionSpec,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) session_execution_owner: Option<lash_core::LeaseOwnerIdentity>,
    pub(crate) store: Option<Arc<dyn RuntimePersistence>>,
    pub(crate) provider: Option<ProviderHandle>,
    pub(crate) active_plugins: Vec<ActivePluginBinding>,
    pub(crate) plugin_factories: Vec<Arc<dyn PluginFactory>>,
    /// Plugin-keyed, serializable open-time options. They ride the protocol
    /// materialization seam (the same `PluginOptions` bag a child
    /// `SessionCreateRequest` carries) so every plugin gets open-time options
    /// through one hook.
    pub(crate) plugin_options: PluginOptions,
}

impl SessionBuilder {
    /// Set the plugin-keyed open-time options bag wholesale.
    pub fn plugin_options(mut self, plugin_options: PluginOptions) -> Self {
        self.plugin_options = plugin_options;
        self
    }

    /// Merge a single plugin's typed options into the open-time options bag,
    /// preserving any options already set for other plugin keys.
    pub fn plugin_option<T: serde::Serialize>(
        mut self,
        plugin_id: impl Into<String>,
        extras: T,
    ) -> Result<Self> {
        self.plugin_options
            .insert_typed(plugin_id, extras)
            .map_err(EmbedError::ProtocolTurnOptions)?;
        Ok(self)
    }

    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.spec = self.spec.provider_id(provider.kind());
        self.provider = Some(provider);
        self
    }

    pub fn session_spec(mut self, spec: SessionSpec) -> Self {
        self.spec = spec;
        self
    }

    pub fn parent(mut self, parent_session_id: impl Into<String>) -> Self {
        self.parent_session_id = Some(parent_session_id.into());
        self
    }

    /// Use an explicit owner identity for durable session execution leases.
    ///
    /// This is only for hosts that already serialize one logical execution lane
    /// and intentionally choose stable owner + incarnation values. Normal
    /// embedders should keep the default per-open identity.
    pub fn session_execution_owner(mut self, owner: lash_core::LeaseOwnerIdentity) -> Self {
        self.session_execution_owner = Some(owner);
        self
    }

    /// Use a specific persistence store for this root session.
    ///
    /// This is the right API for a host-owned, pre-opened session database.
    /// Managed child sessions never reuse this store; configure
    /// `LashCoreBuilder::child_store_factory` when child sessions should also
    /// persist.
    pub fn store(mut self, store: Arc<dyn RuntimePersistence>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn plugin<P: PluginBinding>(mut self, config: P::SessionConfig) -> Self {
        self.active_plugins.push(ActivePluginBinding {
            id: P::ID,
            requires_turn_input: P::requires_turn_input(&config),
        });
        self.plugin_factories.push(P::factory(&config));
        self
    }

    pub async fn open(self) -> Result<LashSession> {
        let policy = self.session_policy();
        let store = self.create_store(&policy).await?;
        self.reconcile_fork_process_grants(store.as_deref()).await?;
        let state = self
            .load_or_default_state(&policy, store.as_deref())
            .await?;
        Box::pin(self.open_resolved(policy, state, store)).await
    }

    async fn reconcile_fork_process_grants(
        &self,
        store: Option<&dyn RuntimePersistence>,
    ) -> Result<()> {
        let (Some(store), Some(process_registry)) =
            (store, self.core.env.process_registry.as_ref())
        else {
            return Ok(());
        };
        let Some(mut meta) = store.load_session_meta().await? else {
            return Ok(());
        };
        let lash_core::SessionRelation::Fork { process_grants, .. } = &meta.relation else {
            return Ok(());
        };
        let target_scope = lash_core::SessionScope::new(self.session_id.clone());
        for grant in process_grants {
            process_registry
                .grant_handle(&target_scope, &grant.process_id, grant.descriptor.clone())
                .await?;
        }
        let lash_core::SessionRelation::Fork { process_grants, .. } = &mut meta.relation else {
            unreachable!("relation was checked above");
        };
        process_grants.clear();
        store.save_session_meta(meta).await?;
        Ok(())
    }

    /// Open with an explicitly supplied runtime state.
    ///
    /// This is for advanced hosts that already own a complete state snapshot.
    /// Normal embedders should use [`Self::open`] to resume according to Lash's
    /// residency policy.
    pub async fn open_with_state(self, mut state: RuntimeSessionState) -> Result<LashSession> {
        let policy = self.session_policy();
        let store = self.create_store(&policy).await?;
        if state.session_id != self.session_id {
            return Err(EmbedError::StoreSessionMismatch {
                loaded: state.session_id,
                requested: self.session_id,
            });
        }
        if let Some(store) = store.as_deref()
            && let Some(meta) = store.load_session_meta().await?
        {
            state.incarnation_id = meta.incarnation_id;
        }
        reconcile_loaded_state_policy(&mut state, &policy);
        Box::pin(self.open_resolved(policy, state, store)).await
    }

    fn session_policy(&self) -> SessionPolicy {
        let mut policy = self.spec.resolve_against(&self.core.policy);
        policy.session_id = Some(self.session_id.clone());
        policy
    }

    async fn load_or_default_state(
        &self,
        policy: &SessionPolicy,
        store: Option<&dyn RuntimePersistence>,
    ) -> Result<RuntimeSessionState> {
        let state = match store {
            Some(store) => {
                let loaded = self.load_persisted_state_for_residency(store).await?;
                let mut state = loaded.unwrap_or_else(|| RuntimeSessionState {
                    session_id: self.session_id.clone(),
                    policy: policy.clone(),
                    ..RuntimeSessionState::default()
                });
                if let Some(meta) = store.load_session_meta().await? {
                    state.incarnation_id = meta.incarnation_id;
                }
                if state.session_id != self.session_id {
                    return Err(EmbedError::StoreSessionMismatch {
                        loaded: state.session_id,
                        requested: self.session_id.clone(),
                    });
                }
                reconcile_loaded_state_policy(&mut state, policy);
                state
            }
            None => RuntimeSessionState {
                session_id: self.session_id.clone(),
                policy: policy.clone(),
                ..RuntimeSessionState::default()
            },
        };
        Ok(state)
    }

    async fn load_persisted_state_for_residency(
        &self,
        store: &dyn RuntimePersistence,
    ) -> Result<Option<RuntimeSessionState>> {
        load_persisted_state_for_residency(self.core.env.residency, store).await
    }

    async fn open_resolved(
        self,
        policy: SessionPolicy,
        state: RuntimeSessionState,
        store: Option<Arc<dyn RuntimePersistence>>,
    ) -> Result<LashSession> {
        let mut env = self.core.env.clone();
        if let Some(provider) = self.provider.clone().or_else(|| self.core.provider.clone()) {
            env.core.providers.provider_resolver =
                Arc::new(lash_core::SingleProviderResolver::new(provider));
        }
        let plugin_host = build_plugin_host(
            self.core.protocol_factory.as_ref(),
            self.core.plugin_factories.as_ref(),
            self.plugin_factories,
        )?;
        env.core = plugin_host.install_process_engine_contributions(
            env.core.clone(),
            self.core.process_lifecycle_available,
        )?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let effect_host = Arc::clone(&env.core.control.effect_host);
        let drivers = self.core.work_driver.drivers().await;
        env.process_work_driver = drivers.process.clone();
        env.queued_work_driver = drivers.queued.clone();
        let mut runtime = LashRuntime::from_environment(&env, policy, state, store).await?;
        // Fire the protocol materialization hook for this root/builder open
        // (including resume): the protocol plugin applies and defaults its
        // per-session options at open time.
        runtime.configure_protocol_on_materialize(
            &self.plugin_options,
            self.parent_session_id.is_none(),
        )?;
        if let Some(owner) = self.session_execution_owner {
            runtime.set_runtime_lease_owner(owner);
        }
        if drivers.drive_process_on_open
            && let Some(driver) = drivers.process.as_ref()
        {
            driver.claim_and_run_pending("session_open").await?;
        }
        let handle = RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.core.live_replay_store),
        );
        Ok(LashSession {
            runtime: handle,
            effect_host,
            parent_session_id: self.parent_session_id,
            active_plugins: self.active_plugins,
            process_phase_probe_slot: self.core.work_driver.phase_probe_slot(),
            turn_cancels: crate::turn::TurnCancelRegistry::default(),
        })
    }

    async fn create_store(
        &self,
        policy: &SessionPolicy,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>> {
        if let Some(store) = self.store.as_ref() {
            return Ok(Some(Arc::clone(store)));
        }
        let Some(factory) = self.core.store_factory.as_ref() else {
            return Ok(None);
        };
        let request = SessionStoreCreateRequest {
            session_id: self.session_id.clone(),
            relation: self
                .parent_session_id
                .as_ref()
                .map(|parent_session_id| lash_core::SessionRelation::Child {
                    parent_session_id: parent_session_id.clone(),
                    caused_by: None,
                })
                .unwrap_or_default(),
            policy: policy.clone(),
        };
        factory
            .create_store(&request)
            .await
            .map(Some)
            .map_err(|message| EmbedError::StoreFactory {
                session_id: self.session_id.clone(),
                message,
            })
    }
}

pub(crate) async fn load_state_for_residency(
    residency: Residency,
    session_id: &str,
    policy: &SessionPolicy,
    store: &dyn RuntimePersistence,
) -> Result<RuntimeSessionState> {
    let mut state = load_persisted_state_for_residency(residency, store)
        .await?
        .unwrap_or_else(|| RuntimeSessionState {
            session_id: session_id.to_string(),
            policy: policy.clone(),
            ..RuntimeSessionState::default()
        });
    if let Some(meta) = store.load_session_meta().await? {
        state.incarnation_id = meta.incarnation_id;
    }
    if state.session_id != session_id {
        return Err(EmbedError::StoreSessionMismatch {
            loaded: state.session_id,
            requested: session_id.to_string(),
        });
    }
    reconcile_loaded_state_policy(&mut state, policy);
    Ok(state)
}

fn reconcile_loaded_state_policy(state: &mut RuntimeSessionState, policy: &SessionPolicy) {
    let recorded_provider_id = state.policy.recorded_provider_id().to_string();
    state.policy = policy.clone();
    state.policy.provider_id = recorded_provider_id;
}

async fn load_persisted_state_for_residency(
    residency: Residency,
    store: &dyn RuntimePersistence,
) -> Result<Option<RuntimeSessionState>> {
    match residency {
        Residency::KeepAll => {
            let loaded = lash_core::store::load_persisted_session_state(store)
                .await
                .map_err(|err| SessionError::Protocol(format!("failed to load store: {err}")))?;
            Ok(loaded)
        }
        Residency::ActivePathOnly => Ok(
            lash_core::store::load_persisted_session_state_active_path(store, None)
                .await
                .map_err(|err| {
                    SessionError::Protocol(format!("failed to load active-path store: {err}"))
                })?,
        ),
    }
}

impl PromptLayerSink for SessionBuilder {
    fn prompt_layer_mut(&mut self) -> &mut PromptLayer {
        self.spec.prompt.get_or_insert_with(PromptLayer::new)
    }
}

#[derive(Clone)]
pub struct LashSession {
    pub(crate) runtime: RuntimeHandle,
    pub(crate) effect_host: Arc<dyn EffectHost>,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) active_plugins: Vec<ActivePluginBinding>,
    pub(crate) process_phase_probe_slot: Option<lash_core::runtime::RuntimeTurnPhaseProbeSlot>,
    pub(crate) turn_cancels: crate::turn::TurnCancelRegistry,
}

#[derive(Clone, Debug, Default)]
pub struct SessionConfigPatch {
    pub provider: Option<ProviderHandle>,
    pub model: Option<ModelSpec>,
    pub prompt: Option<PromptLayer>,
}

/// Lightweight, consuming handle returned by [`LashSession::park`].
///
/// Parking flushes a session's dirty state to its store and drops the live
/// in-memory runtime, keeping only enough to rebuild it: the session id, its
/// policy, and the store reference. This is the webserver-embedder quiesce /
/// handoff primitive — cache one of these per idle session at bounded memory
/// cost regardless of transcript size, then rebuild with
/// [`LashCore::resume`](crate::LashCore::resume).
///
/// The facade owns this vocabulary; it wraps the core parking handle so the
/// resume path stays a facade capability rather than exposing `lash-core`
/// environment plumbing to hosts.
pub struct ParkedSession {
    pub(crate) inner: lash_core::ParkedSession,
}

impl ParkedSession {
    /// The parked session's id. Use it to key a per-session cache of parked
    /// handles on the host.
    pub fn session_id(&self) -> &str {
        self.inner.session_id()
    }
}

impl LashSession {
    /// Durably close this session, then release its in-memory runtime.
    ///
    /// `close` is the honest teardown verb: a persistent session flushes its
    /// dirty state (via a fresh-lease commit) so the store reflects the final
    /// transcript, its in-memory plugin session is unregistered, and the live
    /// runtime is dropped. A store-less (ephemeral) session has nothing to
    /// persist, so closing it is exactly the plugin-session unregister plus
    /// dropping the runtime.
    ///
    /// This consumes the session and requires exclusive ownership: any cloned
    /// [`LashSession`] handle or in-flight turn keeps a live reference to the
    /// same runtime, so `close` returns [`EmbedError::SessionStillInUse`] until
    /// those are dropped or finished. Cancel running turns first with
    /// [`cancel_running_turns`](Self::cancel_running_turns) if needed.
    ///
    /// To keep a handle for later resumption instead of discarding the session,
    /// use [`park`](Self::park).
    pub async fn close(self) -> Result<()> {
        // Persistence is decided before we consume `self`; the observation's
        // queue store is the facade's canonical "is this session persistent"
        // signal (the same source `park`'s commit uses).
        let persistent = self.runtime.observe().queue_store.is_some();
        let runtime = self.into_owned_runtime()?;
        runtime.unregister_plugin_session()?;
        if persistent {
            // Reuse the core parking primitive to flush + release the lease,
            // discarding the returned handle: close does not resume.
            runtime.park().await?;
        }
        // Ephemeral sessions: `runtime` drops here, releasing in-memory state.
        Ok(())
    }

    /// Quiesce this session for later resumption, returning a lightweight
    /// [`ParkedSession`] handle.
    ///
    /// Parking flushes dirty state to the store (a fresh-lease commit), drops
    /// the live runtime and its plugin session, and hands back a cheap handle
    /// the host can cache and later rebuild with
    /// [`LashCore::resume`](crate::LashCore::resume). This is the
    /// quiesce/handoff lever for webserver embedders that hold many idle
    /// sessions: it bounds resident memory per session without deleting durable
    /// state.
    ///
    /// Contract:
    /// - **Persistent runtime required.** Parking flushes to the store, so a
    ///   store-less session cannot be parked and returns an error. Use
    ///   [`close`](Self::close) to tear down an ephemeral session.
    /// - **Exclusive ownership required.** `park` consumes the session and drops
    ///   the in-memory runtime, so it needs the sole live reference. A cloned
    ///   [`LashSession`] or an in-flight turn holds another reference and makes
    ///   `park` return [`EmbedError::SessionStillInUse`]. Because an executing
    ///   turn holds such a reference, parking is effectively an *idle-session*
    ///   operation: finish or [`cancel_running_turns`](Self::cancel_running_turns)
    ///   first. The store commit itself does not observe an active turn; the
    ///   exclusive-ownership guard is what makes mid-turn parking an explicit
    ///   error rather than a silent partial flush.
    pub async fn park(self) -> Result<ParkedSession> {
        let runtime = self.into_owned_runtime()?;
        // We now own the runtime exclusively; release the in-memory plugin
        // session registration before flushing and dropping it.
        runtime.unregister_plugin_session()?;
        let parked = runtime.park().await?;
        Ok(ParkedSession { inner: parked })
    }

    /// Consume the session and take sole ownership of the underlying runtime.
    ///
    /// Fails with [`EmbedError::SessionStillInUse`] when another live handle
    /// (a cloned session or an in-flight turn) shares the runtime, so
    /// consuming operations never proceed on a still-shared runtime.
    fn into_owned_runtime(self) -> Result<LashRuntime> {
        let LashSession { runtime, .. } = self;
        // `writer()` clones the shared `Arc<Mutex<LashRuntime>>`; dropping the
        // handle then leaves this clone as the sole strong reference iff no
        // other handle exists, so `try_unwrap` doubles as the exclusive-owner
        // check.
        let writer = runtime.writer();
        drop(runtime);
        Arc::try_unwrap(writer)
            .map(|mutex| mutex.into_inner())
            .map_err(|_| EmbedError::SessionStillInUse)
    }

    pub fn session_id(&self) -> String {
        self.runtime.observe().session_id().to_string()
    }

    pub fn policy_snapshot(&self) -> SessionPolicy {
        self.runtime.observe().policy.clone()
    }

    pub fn observe(&self) -> ObservableSession {
        ObservableSession {
            runtime: self.runtime.clone(),
        }
    }

    pub fn parent_session_id(&self) -> Option<&str> {
        self.parent_session_id.as_deref()
    }

    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub fn turn(&self, input: TurnInput) -> TurnBuilder {
        TurnBuilder {
            runtime: self.runtime.clone(),
            effect_host: Arc::clone(&self.effect_host),
            active_plugins: self.active_plugins.clone(),
            input,
            cancel: CancellationToken::new(),
            cancel_origin_hint: lash_core::TurnCancelOriginHint::default(),
            cancels: self.turn_cancels.clone(),
            protocol_turn_options: None,
            provider: None,
            turn_id: None,
        }
    }

    pub fn queued_turn(&self) -> QueuedTurnBuilder {
        QueuedTurnBuilder {
            runtime: self.runtime.clone(),
            effect_host: Arc::clone(&self.effect_host),
            cancel: CancellationToken::new(),
            cancel_origin_hint: lash_core::TurnCancelOriginHint::default(),
            cancels: self.turn_cancels.clone(),
            batch_ids: Vec::new(),
            drain_id: None,
        }
    }

    /// Request cooperative cancellation of exactly one turn in this session.
    ///
    /// The request is compiled onto the deployment's keyed-promise control
    /// seam. An inline effect host is process-local; another process or a
    /// replayed owner can observe the request only with a controller-owned
    /// replay deployment. `origin` is opaque host-domain data that Lash records
    /// without interpretation. Detached effects are not guaranteed to stop.
    /// `turn_id` is routing identity, not authorization; hosts must authorize
    /// callers before invoking this API.
    pub async fn request_turn_cancel(
        &self,
        turn_id: &str,
        request_id: impl Into<String>,
        origin: Option<String>,
        reason: Option<String>,
    ) -> Result<lash_core::TurnCancelReceipt> {
        let mut request = lash_core::TurnCancelRequest::new(
            lash_core::TurnAddress::new(self.session_id(), turn_id),
            request_id,
            origin,
        );
        request.reason = reason;
        lash_core::TurnWorkDriver::new(self.effect_host())
            .request_cancel(request)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Cancel every turn currently executing through this opened session
    /// (including its clones) and report how many were signalled.
    ///
    /// This raw process-local compatibility lever records no origin. User
    /// controls, shutdown, and provider plumbing should call
    /// [`cancel_running_turns_with_origin`](Self::cancel_running_turns_with_origin).
    /// Host-facing durable stop controls should retain an
    /// exact turn id and call [`request_turn_cancel`](Self::request_turn_cancel)
    /// so cancellation survives separately opened handles and durable replay.
    /// A cancelled turn finishes with
    /// `TurnOutcome::Stopped(TurnStop::Cancelled)` and commits like any other
    /// turn; the session stays usable.
    ///
    /// Scope: turns started from this `LashSession` instance and its clones.
    /// A handle opened separately for the same session id has its own
    /// registry and is not reached.
    pub fn cancel_running_turns(&self) -> usize {
        self.cancel_running_turns_with_origin(None)
    }

    /// Cancel active process-local turns with an opaque host-defined origin.
    pub fn cancel_running_turns_with_origin(&self, origin: Option<String>) -> usize {
        self.turn_cancels.cancel_all(origin)
    }

    pub fn admin(&self) -> SessionAdmin {
        SessionAdmin {
            runtime: self.runtime.clone(),
        }
    }

    pub async fn configure(&self, patch: SessionConfigPatch) -> Result<()> {
        self.admin().config().update(patch).await
    }

    pub fn tools(&self) -> ToolAdmin {
        ToolAdmin::new(self.admin())
    }

    pub fn commands(&self) -> SessionCommandAdmin {
        self.admin().commands()
    }

    pub fn triggers(&self) -> SessionTriggerAdmin {
        self.admin().triggers()
    }

    pub fn processes(&self) -> SessionProcessAdmin {
        SessionProcessAdmin::new(self.admin())
    }

    /// Refresh the session graph from any background process that signalled it
    /// changed. This is the honest name for the former
    /// `processes().await_all()` misnomer (ADR 0019 grill): a session-graph
    /// resync, not a terminal wait on background work — wait on a process with
    /// [`SessionProcessAdmin::await_output`]. It lives on the session surface
    /// because it refreshes the session graph, not the global process registry.
    pub async fn refresh_background_graph(&self) -> Result<()> {
        self.admin().refresh_background_graph().await
    }

    pub fn plugin_operations(&self) -> PluginOperations {
        PluginOperations {
            control: self.admin(),
        }
    }

    pub fn enqueue(&self, input: TurnInput) -> EnqueueTurnBuilder<'_> {
        EnqueueTurnBuilder {
            session: self,
            input,
            id: None,
            ingress: TurnInputIngress::NextTurn,
        }
    }

    /// Return all pending durable queued-work batches for this session.
    ///
    /// This is an admin/introspection view for non-user queued work such as
    /// process wakes and session commands. User-visible model input is stored
    /// separately as pending turn input and is exposed by
    /// [`pending_turn_inputs`](Self::pending_turn_inputs).
    pub async fn queued_work(&self) -> Result<Vec<QueuedWorkBatch>> {
        let observation = self.runtime.observe();
        let store = observation.queue_store.as_ref().ok_or_else(|| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitFailed,
                "queued work inspection requires a persistent runtime store",
            ))
        })?;
        store
            .list_pending_queued_work(observation.session_id())
            .await
            .map_err(|err| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                ))
            })
    }

    pub async fn pending_turn_inputs(&self) -> Result<Vec<PendingTurnInput>> {
        let observation = self.runtime.observe();
        let store = observation.queue_store.as_ref().ok_or_else(|| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitFailed,
                "pending turn input inspection requires a persistent runtime store",
            ))
        })?;
        store
            .list_pending_turn_inputs(observation.session_id())
            .await
            .map_err(|err| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                ))
            })
    }

    /// Read settled canonical input applications from durable turn commits.
    ///
    /// This is the reconciliation surface for hosts whose live observation
    /// cursor fell outside the bounded replay window.
    pub async fn turn_input_applications(&self) -> Result<Vec<lash_core::TurnInputApplication>> {
        let observation = self.runtime.observe();
        let store = observation.queue_store.as_ref().ok_or_else(|| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitFailed,
                "turn input application reconciliation requires a persistent runtime store",
            ))
        })?;
        store
            .list_turn_input_applications(observation.session_id())
            .await
            .map_err(|err| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                ))
            })
    }

    /// Versioned wire form of [`turn_input_applications`](Self::turn_input_applications).
    pub async fn remote_turn_input_applications(
        &self,
    ) -> Result<Vec<lash_remote_protocol::RemoteTurnInputApplication>> {
        Ok(self
            .turn_input_applications()
            .await?
            .iter()
            .map(Into::into)
            .collect())
    }

    pub async fn cancel_pending_turn_input(
        &self,
        input_id: &str,
    ) -> Result<PendingTurnInputCancelOutcome> {
        let session_id = self.session_id();
        self.runtime
            .cancel_pending_turn_input(&session_id, input_id)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Atomically cancel a set of pending user inputs by runtime input id or
    /// app source key.
    ///
    /// This is the app reconciliation path for explicit selections such as
    /// "remove these pending drafts". Returned outcomes distinguish newly
    /// cancelled input from input that was already claimed, completed,
    /// cancelled, or missing.
    pub async fn cancel_pending_turn_inputs(
        &self,
        targets: impl IntoIterator<Item = PendingTurnInputCancelTarget>,
    ) -> Result<Vec<PendingTurnInputCancelResult>> {
        let session_id = self.session_id();
        let targets = targets.into_iter().collect::<Vec<_>>();
        self.runtime
            .cancel_pending_turn_inputs(&session_id, &targets)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Atomically cancel the same-session pending-input suffix from `anchor`.
    ///
    /// Apps that let users edit previously submitted product messages should
    /// map the edited message to the stored pending-input `input_id` or
    /// `source_key`, call this method, and only restore/edit drafts that return
    /// [`PendingTurnInputCancelOutcome::Cancelled`]. Claimed or completed
    /// inputs have already crossed the runtime boundary and should be treated
    /// as reconciliation state, not local editable drafts.
    pub async fn cancel_pending_turn_input_suffix(
        &self,
        anchor: PendingTurnInputCancelTarget,
    ) -> Result<PendingTurnInputSuffixCancelOutcome> {
        let session_id = self.session_id();
        self.runtime
            .cancel_pending_turn_input_suffix(&session_id, &anchor)
            .await
            .map_err(EmbedError::Runtime)
    }

    pub async fn cancel_queued_work_batch(
        &self,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>> {
        let session_id = self.session_id();
        self.runtime
            .cancel_queued_work_batch(&session_id, batch_id)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Release a held queued-work claim without completing it, returning its
    /// batches to the pending queue immediately.
    ///
    /// A host stopping an external queued-work driver mid-claim calls this
    /// with the claims that driver still holds so the work becomes claimable
    /// again at once instead of waiting out the claim's lease TTL.
    pub async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<()> {
        self.runtime
            .abandon_queued_work_claim(claim)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Release a held pending-turn-input claim without completing it, returning
    /// its inputs to the pending queue immediately. The turn-input counterpart
    /// of [`abandon_queued_work_claim`](Self::abandon_queued_work_claim).
    pub async fn abandon_turn_input_claim(&self, claim: &TurnInputClaim) -> Result<()> {
        self.runtime
            .abandon_turn_input_claim(claim)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Cancel every outstanding durable wait for this session without deleting
    /// the session.
    ///
    /// Each waiter receives a terminal [`Resolution::Cancelled`](crate::Resolution)
    /// instead of hanging until an external completion arrives, and late
    /// resolves observe that terminal. The session itself stays usable: new
    /// durable waits registered afterwards behave normally, unlike the
    /// tombstoning revocation [`LashCore::delete_session`](crate::LashCore::delete_session)
    /// performs.
    pub async fn revoke_durable_waits(&self) -> Result<()> {
        let session_id = self.session_id();
        self.effect_host
            .cancel_await_events_for_session(&session_id)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Resolve once `batch_id` is no longer pending in the queue store —
    /// drained by whoever runs queued work (a queued-work runner, a durable
    /// worker, or another handle's [`queued_turn`](Self::queued_turn)) or
    /// cancelled. This is the enqueue-and-observe side of the queue: the
    /// caller never claims the work itself.
    ///
    /// Completion is read from the persistent queue store, so it observes
    /// drains performed by other session handles and other processes alike.
    /// There is no built-in deadline — nothing resolves if nothing drains the
    /// queue, so bound it with `tokio::time::timeout` when the worker may be
    /// unavailable. A batch id the store has never seen resolves immediately.
    pub async fn await_queued_work_batch(&self, batch_id: &str) -> Result<()> {
        let observation = self.runtime.observe();
        let store = observation.queue_store.clone().ok_or_else(|| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitFailed,
                "queued work inspection requires a persistent runtime store",
            ))
        })?;
        let session_id = observation.session_id().to_string();
        drop(observation);
        let mut delay = std::time::Duration::from_millis(25);
        loop {
            let pending = store
                .list_pending_queued_work(&session_id)
                .await
                .map_err(|err| {
                    EmbedError::Runtime(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::StoreCommitFailed,
                        err.to_string(),
                    ))
                })?;
            if !pending.iter().any(|batch| batch.batch_id == batch_id) {
                return Ok(());
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_millis(400));
        }
    }

    pub fn read_view(&self) -> SessionReadView {
        self.runtime.observe().read_view.clone()
    }

    pub fn usage_report(&self) -> SessionUsageReport {
        self.runtime.observe().usage_report.clone()
    }

    pub async fn set_turn_phase_probe(
        &self,
        probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>,
    ) {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        runtime.set_turn_phase_probe(Arc::clone(&probe));
        self.runtime.publish_from(&runtime);
        if let Some(slot) = &self.process_phase_probe_slot {
            let observation = self.runtime.observe();
            slot.set_for_session(observation.session_id(), Arc::clone(&probe));
            if let Some(current_frame) =
                observation.persisted_state.current_frame_node_id.as_deref()
            {
                let scope = lash_core::SessionScope::for_agent_frame(
                    observation.session_id(),
                    current_frame,
                );
                slot.set_for_scope(&scope, probe);
            }
        }
    }
}

#[derive(Clone)]
pub struct ObservableSession {
    pub(crate) runtime: RuntimeHandle,
}

impl ObservableSession {
    fn snapshot(&self) -> Arc<RuntimeObservation> {
        self.runtime.observe()
    }

    pub fn current_observation(&self) -> SessionObservation {
        self.runtime.current_session_observation()
    }

    pub fn current_remote_observation(&self) -> RemoteSessionObservation {
        RemoteSessionObservation::from_core(self.current_observation())
    }

    pub fn resume_from_cursor(&self, cursor: &SessionCursor) -> Result<SessionResume> {
        self.runtime
            .resume_session_observation(cursor)
            .map_err(live_replay_error)
    }

    pub fn subscribe_from_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<SessionObservationSubscription> {
        self.runtime
            .subscribe_session_observation(cursor)
            .map_err(live_replay_error)
    }

    pub fn subscribe_from_remote_cursor(
        &self,
        cursor: &RemoteSessionCursor,
    ) -> Result<RemoteSessionObservationSubscription> {
        cursor.validate()?;
        let cursor = lash_core::SessionCursor::try_from(cursor.clone())?;
        match self.subscribe_from_cursor(&cursor)? {
            SessionObservationSubscription::Subscribed(subscription) => {
                Ok(RemoteSessionObservationSubscription::Subscribed(
                    RemoteSessionObservationEventStream::new(subscription),
                ))
            }
            SessionObservationSubscription::Gap { observation, gap } => {
                Ok(RemoteSessionObservationSubscription::Gap {
                    observation: observation.into(),
                    gap: gap.into(),
                })
            }
        }
    }

    /// Subscribe to session observation events and keep the subscription alive
    /// across recoverable live-replay gaps.
    ///
    /// The returned stream yields [`SessionObservationStreamItem::Gap`] when
    /// the cursor missed the bounded replay window. Callers should replace
    /// their UI/projection from the included fresh observation, persist
    /// `gap.latest_cursor`, and keep polling the same stream; it resubscribes
    /// from that cursor internally.
    pub fn subscribe_and_recover(&self, cursor: SessionCursor) -> SessionObservationStream {
        SessionObservationStream {
            observable: self.clone(),
            cursor,
            subscription: None,
            done: false,
        }
    }

    /// Subscribe to remote DTO session observation events and keep the
    /// subscription alive across recoverable live-replay gaps.
    pub fn subscribe_and_recover_remote(
        &self,
        cursor: RemoteSessionCursor,
    ) -> Result<RemoteSessionObservationStream> {
        cursor.validate()?;
        let cursor = lash_core::SessionCursor::try_from(cursor)?;
        Ok(RemoteSessionObservationStream {
            inner: self.subscribe_and_recover(cursor),
            next_sequence: 0,
        })
    }

    pub fn session_id(&self) -> String {
        self.snapshot().session_id().to_string()
    }

    pub fn policy_snapshot(&self) -> SessionPolicy {
        self.snapshot().policy.clone()
    }

    pub fn read_view(&self) -> SessionReadView {
        self.snapshot().read_view.clone()
    }

    pub fn usage_report(&self) -> SessionUsageReport {
        self.snapshot().usage_report.clone()
    }

    pub fn tool_state(&self) -> Option<ToolState> {
        self.snapshot().tool_state.clone()
    }

    pub fn active_tool_manifests(&self) -> Vec<ToolManifest> {
        self.snapshot()
            .tool_state
            .as_ref()
            .map(ToolState::tool_manifests)
            .unwrap_or_default()
    }

    pub async fn list_process_handles(&self) -> Vec<ProcessHandleSummary> {
        self.snapshot().list_process_handles().await
    }

    pub async fn list_all_process_handles(&self) -> Vec<ProcessHandleSummary> {
        self.snapshot().list_all_process_handles().await
    }

    pub fn process_scope(&self) -> SessionScope {
        self.snapshot().process_scope()
    }
}

// A public streaming yield produced one item at a time by `Stream::poll_next`;
// the variant-size spread is transient (never accumulated in a collection), so
// boxing would only add a per-event heap allocation on the observation hot path
// and force `*`-deref churn on every SDK consumer. The sibling
// `RemoteSessionObservationStreamItem` keeps the same inline shape.
// justification: the gap is transient and inline to avoid allocation and preserve the public stream-item API.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum SessionObservationStreamItem {
    /// A replayed or live session observation event.
    Event(Arc<SessionObservationEvent>),
    /// A recoverable replay gap with a fresh durable observation.
    Gap {
        observation: SessionObservation,
        gap: LiveReplayGap,
    },
}

pub enum RemoteSessionObservationSubscription {
    Subscribed(RemoteSessionObservationEventStream),
    Gap {
        observation: RemoteSessionObservation,
        gap: RemoteLiveReplayGap,
    },
}

#[derive(Clone, Debug)]
pub enum RemoteSessionObservationStreamItem {
    /// A replayed or live session observation event encoded as remote DTOs.
    Event(RemoteSessionObservationEvent),
    /// A recoverable replay gap with a fresh remote observation snapshot.
    Gap {
        observation: RemoteSessionObservation,
        gap: RemoteLiveReplayGap,
    },
}

pub struct RemoteSessionObservationEventStream {
    inner: lash_core::LiveReplaySubscription,
    next_sequence: u64,
}

impl RemoteSessionObservationEventStream {
    fn new(inner: lash_core::LiveReplaySubscription) -> Self {
        Self {
            inner,
            next_sequence: 0,
        }
    }

    pub async fn next_event(&mut self) -> Result<RemoteSessionObservationEvent> {
        futures_util::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx))
            .await
            .transpose()?
            .ok_or_else(|| live_replay_error(LiveReplayStoreError::Closed))
    }
}

impl Stream for RemoteSessionObservationEventStream {
    type Item = Result<RemoteSessionObservationEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(event))) => {
                let remote = RemoteSessionObservationEvent::from_core(self.next_sequence, event);
                self.next_sequence = self.next_sequence.saturating_add(1);
                Poll::Ready(Some(Ok(remote)))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(live_replay_error(err)))),
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

/// Remote DTO stream returned by [`ObservableSession::subscribe_and_recover_remote`].
pub struct RemoteSessionObservationStream {
    inner: SessionObservationStream,
    next_sequence: u64,
}

impl RemoteSessionObservationStream {
    pub fn cursor(&self) -> RemoteSessionCursor {
        RemoteSessionCursor::from(self.inner.cursor())
    }
}

impl Stream for RemoteSessionObservationStream {
    type Item = Result<RemoteSessionObservationStreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(SessionObservationStreamItem::Event(event)))) => {
                let remote = RemoteSessionObservationEvent::from_core(self.next_sequence, event);
                self.next_sequence = self.next_sequence.saturating_add(1);
                Poll::Ready(Some(Ok(RemoteSessionObservationStreamItem::Event(remote))))
            }
            Poll::Ready(Some(Ok(SessionObservationStreamItem::Gap { observation, gap }))) => {
                Poll::Ready(Some(Ok(RemoteSessionObservationStreamItem::Gap {
                    observation: observation.into(),
                    gap: gap.into(),
                })))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

/// Stream returned by [`ObservableSession::subscribe_and_recover`].
pub struct SessionObservationStream {
    observable: ObservableSession,
    cursor: SessionCursor,
    subscription: Option<lash_core::LiveReplaySubscription>,
    done: bool,
}

impl SessionObservationStream {
    pub fn cursor(&self) -> &SessionCursor {
        &self.cursor
    }
}

impl Stream for SessionObservationStream {
    type Item = Result<SessionObservationStreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.done {
                return Poll::Ready(None);
            }
            if self.subscription.is_none() {
                match self.observable.subscribe_from_cursor(&self.cursor) {
                    Ok(SessionObservationSubscription::Subscribed(subscription)) => {
                        self.subscription = Some(subscription);
                    }
                    Ok(SessionObservationSubscription::Gap { observation, gap }) => {
                        self.cursor = gap.latest_cursor.clone();
                        return Poll::Ready(Some(Ok(SessionObservationStreamItem::Gap {
                            observation,
                            gap,
                        })));
                    }
                    Err(err) => {
                        self.done = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                }
            }

            let Some(subscription) = self.subscription.as_mut() else {
                continue;
            };
            match Pin::new(subscription).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(event))) => {
                    self.cursor = event.cursor.clone();
                    return Poll::Ready(Some(Ok(SessionObservationStreamItem::Event(event))));
                }
                Poll::Ready(Some(Err(LiveReplayStoreError::SubscriberLagged(_)))) => {
                    self.subscription = None;
                    continue;
                }
                Poll::Ready(Some(Err(err))) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(live_replay_error(err))));
                }
                Poll::Ready(None) => {
                    self.done = true;
                    return Poll::Ready(None);
                }
            }
        }
    }
}

fn live_replay_error(err: lash_core::LiveReplayStoreError) -> EmbedError {
    EmbedError::Runtime(lash_core::RuntimeError::new(
        RuntimeErrorCode::Other("live_replay".to_string()),
        err.to_string(),
    ))
}

pub struct EnqueueTurnBuilder<'a> {
    session: &'a LashSession,
    input: TurnInput,
    id: Option<String>,
    ingress: TurnInputIngress,
}

impl<'a> EnqueueTurnBuilder<'a> {
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn ingress(mut self, ingress: TurnInputIngress) -> Self {
        self.ingress = ingress;
        self
    }

    /// Persist the input and return stable durable-acceptance identity.
    ///
    /// For retryable host requests, supply [`id`](Self::id) again after an
    /// ambiguous transport failure; its source key is the idempotency identity.
    /// Mutable queue lifecycle state is available from
    /// [`LashSession::pending_turn_inputs`].
    pub async fn send(self) -> Result<TurnInputAcceptanceReceipt> {
        let source_key = self.id.map(|id| format!("host:{id}"));
        self.session
            .runtime
            .enqueue_turn_input(self.input, self.ingress, source_key)
            .await
            .map(|pending| TurnInputAcceptanceReceipt::from(&pending))
            .map_err(EmbedError::Runtime)
    }
}

impl<'a> std::future::IntoFuture for EnqueueTurnBuilder<'a> {
    type Output = Result<TurnInputAcceptanceReceipt>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}
