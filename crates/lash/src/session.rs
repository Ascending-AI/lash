use lash_core::plugin::PluginSessionRequest;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::durable_session::DurableSession;
use crate::session_binding::BoundSession;
use crate::support::{
    Arc, EffectHost, EmbedError, LashCore, LashRuntime, PluginBinding, PluginFactory,
    PluginOperations, PluginOptions, ProcessHandleView, PromptLayer, PromptLayerSink,
    ProviderHandle, Result, RuntimeErrorCode, RuntimeHandle, RuntimeObservation,
    RuntimeSessionState, SessionAdmin, SessionCreationHead, SessionCursor, SessionError,
    SessionObservation, SessionObservationSubscription, SessionPolicy, SessionReadView,
    SessionResume, SessionScope, SessionSpec, SessionStoreCreateRequest, SessionUsageReport,
    ToolManifest, ToolState, TurnInput, build_plugin_host, refuse_foreign_backend_factories,
};
use futures_util::Stream;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_core::runtime::{UnreportedUsageAttempt, UsageReconciliationReport};
use lash_core::{LiveReplayStoreError, SessionObservationEvent, facade_support::LiveReplayGap};
use lash_remote_protocol::{
    RemoteLiveReplayGap, RemoteSessionCursor, RemoteSessionObservation,
    RemoteSessionObservationEvent,
};

/// Builder for one host-named session.
///
/// Every facade session's store comes from the core's backend catalog;
/// there is no way to hand a session a store from anywhere else.
///
/// The builder carries only what one open supplies — a provider resolver,
/// process-local plugin factories, the tool-source policy and
/// [`enqueue_only`](Self::enqueue_only). A session's config is not among
/// them: it is stated once, in the [`SessionCreation`] passed to
/// [`create`](Self::create), and changed afterwards only through
/// [`update`](crate::admin::SessionConfigAdmin::update).
pub struct SessionBuilder {
    pub(crate) core: LashCore,
    pub(crate) session_id: SessionId,
    pub(crate) provider: Option<ProviderHandle>,
    pub(crate) plugin_factories: Vec<Arc<dyn PluginFactory>>,
    /// Per-open override of the core's tool-source policy (FIG-3367).
    pub(crate) tool_source_policy: Option<lash_core::ToolSourcePolicy>,
    /// Set when the host declares this open will not run a turn (FIG-3353).
    pub(crate) tool_surface_open_mode: Option<lash_core::ToolSurfaceOpenMode>,
}

/// What a session is created with: the argument of
/// [`SessionBuilder::create`], the only verb that creates a session and the
/// only one that takes session config (FIG-4112).
///
/// Creation writes all of it once, with the session's catalog row, in one
/// store transaction. Nothing here is restated on open.
#[derive(Clone, Debug, Default)]
pub struct SessionCreation {
    /// The session's config: model, provider pin, prompt, generation and the
    /// rest of [`SessionSpec`], resolved against the core's policy. Unset
    /// fields take the core's values. The provider pin is
    /// [`SessionSpec::provider_id`]; a provider handle given to a later
    /// [`open`](SessionBuilder::open) only resolves it.
    pub spec: SessionSpec,
    /// The session's parent, recorded as its Session Relation (ADR 0089).
    /// This is the only facade path to a related session: the session is an
    /// ordinary session with its own Session Binding and its own usage
    /// ledger — rolling related sessions together is host policy, not a
    /// facade service. `None` creates a root session.
    pub parent: Option<SessionId>,
    /// Plugin-keyed, serializable creation options. The session's protocol
    /// resolves them at creation — as its first materialization would — and
    /// the result, the RLM and plugin session config, is recorded with the
    /// session's initial config head.
    pub plugin_options: PluginOptions,
}

impl PromptLayerSink for SessionCreation {
    fn prompt_layer_mut(&mut self) -> &mut PromptLayer {
        self.spec.prompt.get_or_insert_with(PromptLayer::new)
    }
}

struct ResolvedSessionStore {
    store: lash_core::store::SessionStore,
    catalog: Arc<dyn lash_core::DeploymentStore>,
}

fn empty_runtime_session_state(
    session_id: impl Into<SessionId>,
    policy: SessionPolicy,
) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: session_id.into(),
        ..RuntimeSessionState::new(policy)
    }
}

impl SessionBuilder {
    /// The provider this open resolves the session's recorded provider pin
    /// with. It is a resolver only and records nothing: an open whose
    /// provider cannot serve the recorded pin is refused with
    /// [`ProviderMismatch`](lash_core::SessionError::ProviderMismatch).
    /// The pin itself is stated at creation, by
    /// [`SessionSpec::provider_id`].
    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Override the core's tool-source policy for this open.
    ///
    /// The core's choice is the deployment default; this states it for one
    /// session — an unattended reopen that must not run without its tools sets
    /// [`Require`](lash_core::ToolSourcePolicy::Require) even on a core that
    /// tolerates loss elsewhere. The refusal is
    /// [`SessionError::ToolSourcesUnavailable`](lash_core::SessionError::ToolSourcesUnavailable),
    /// which carries the report.
    pub fn tool_source_policy(mut self, policy: lash_core::ToolSourcePolicy) -> Self {
        self.tool_source_policy = Some(policy);
        self
    }

    /// Declare that this open will not run a turn — the host is opening the
    /// session only to enqueue input or take a commit, possibly on a core that
    /// does not carry the session's tool sources at all.
    ///
    /// The open skips the persisted-tool reconcile and catalog rebuild: the
    /// durable `ToolState` is not installed, no generation bumps, no
    /// [`ToolRestoreReport`](lash_core::ToolRestoreReport) is produced and no
    /// lost-tools warning fires. Commits the open takes carry the persisted
    /// surface forward byte-for-byte instead of restamping an unreconciled
    /// registry, so the session's tools are still catalog members on the next
    /// ordinary open.
    ///
    /// The declaration is enforced, not advisory: because the surface was
    /// never reconciled and no [`ToolSourcePolicy`](lash_core::ToolSourcePolicy)
    /// was enforced, [`turn`](LashSession::turn),
    /// [`queued_turn`](LashSession::queued_turn) and every other
    /// turn-execution entry fail with
    /// [`RuntimeErrorCode::TurnExecutionRequiresReconciledToolSurface`](lash_core::RuntimeErrorCode)
    /// before admission. Reopen without `enqueue_only` to run a turn. See
    /// [`ToolSurfaceOpenMode::PreservePersisted`](lash_core::ToolSurfaceOpenMode).
    pub fn enqueue_only(mut self) -> Self {
        self.tool_surface_open_mode = Some(lash_core::ToolSurfaceOpenMode::PreservePersisted);
        self
    }

    pub fn plugin<P: PluginBinding>(mut self, config: P::SessionConfig) -> Self {
        self.plugin_factories.push(P::factory(&config));
        self
    }

    /// Open this session's runtime.
    ///
    /// Open never creates: it resolves an existing session through the
    /// catalog's non-creating seam and writes no catalog row. A session id the
    /// catalog has never created is refused with
    /// [`EmbedError::UnknownSession`], and a deleted one with
    /// [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted).
    /// Create the session first with [`create`](Self::create).
    ///
    /// The session runs with the config it recorded at creation, as recorded,
    /// and the open writes nothing (FIG-4099). Change a session's config with
    /// [`update`](crate::admin::SessionConfigAdmin::update).
    pub async fn open(self) -> Result<LashSession> {
        let resolved = self.existing_store().await?;
        self.reconcile_process_observer_intents(Some(&resolved.store))
            .await?;
        let state = self.recorded_state(&resolved.store).await?;
        Box::pin(self.open_resolved(state, resolved, true)).await
    }

    async fn reconcile_process_observer_intents(
        &self,
        store: Option<&lash_core::store::SessionStore>,
    ) -> Result<()> {
        let Some(store) = store else {
            return Ok(());
        };
        lash_core::runtime::reconcile_session_process_observer_intents(
            self.core.env.process_registry().map(Arc::as_ref),
            &self.session_id,
            lash_core::runtime::SessionObserverIntentSource::PersistedIfPresent(
                store.store().as_ref(),
            ),
        )
        .await?;
        Ok(())
    }

    /// Acquire this session's **Durable Session**: store-backed access to its
    /// queue and settled reads with no runtime.
    ///
    /// This is the second terminal verb of the session builder. Unlike
    /// [`open`](Self::open) it builds no runtime at all: no Session Execution
    /// Lease, no plugin session, no tool registry, no lifecycle events, no
    /// observer-intent reconcile and no process admission. Use it whenever the
    /// host only needs to read or edit the queue — including beside a live
    /// writer in another process.
    ///
    /// Acquisition resolves an *existing* store through the catalog's
    /// non-creating seam, at most once per handle. The session id must already be
    /// known; see [`DurableSession`] for the typed refusals.
    pub async fn durable(self) -> Result<DurableSession> {
        let work = self.core.held_work().await;
        let ingress = self.core.ingress_relay(&work);
        let live_replay_store = Arc::clone(&self.core.live_replay_store);
        Ok(DurableSession::from_catalog(
            self.session_id,
            Arc::clone(&self.core.store_factory),
            work,
            ingress,
            Arc::clone(&self.core.env.core.control.effect_host),
            live_replay_store,
            Arc::clone(&self.core.env.core.providers.provider_resolver),
        ))
    }

    /// Create this session, then return its **Durable Session**.
    ///
    /// The only verb that creates a session, and the only one that takes
    /// session config (FIG-4112). It writes the session's catalog row and its
    /// initial config head — `creation`'s spec, relation and resolved plugin
    /// options — in one store transaction, and stops there: no runtime, no
    /// Session Execution Lease, no plugin session, no lifecycle event. Run the
    /// session with [`open`](Self::open), or admit durable input for its first
    /// turn with `create(creation).await?.send(input)`. The builder's open
    /// knobs — provider resolver, plugin factories, tool-source policy,
    /// `enqueue_only` — belong to an open and take no part in creation.
    ///
    /// An id the catalog already holds is refused with
    /// [`EmbedError::SessionAlreadyExists`], always — even when a retry states
    /// exactly the config the session recorded. The host owns its ids; a host
    /// that means create-or-open writes that out, treating
    /// `SessionAlreadyExists` as present:
    ///
    /// ```ignore
    /// match core.session(id.clone()).create(creation).await {
    ///     Ok(_) | Err(EmbedError::SessionAlreadyExists { .. }) => {}
    ///     Err(error) => return Err(error),
    /// }
    /// let session = core.session(id).open().await?;
    /// ```
    ///
    /// A *deleted* id is refused with the store's typed
    /// [`SessionDeleted`](lash_core::StoreError::SessionDeleted); ids are
    /// single-use.
    pub async fn create(self, creation: SessionCreation) -> Result<DurableSession> {
        let SessionCreation {
            spec,
            parent,
            plugin_options,
        } = creation;
        let mut policy = spec.resolve_against(&self.core.policy);
        policy.session_id = Some(self.session_id.clone());
        let mut config = lash_core::PersistedSessionConfig::from(&policy);
        config.protocol_turn_options = creation_protocol_turn_options(
            self.core.protocol_factory.as_ref(),
            &self.session_id,
            parent.clone(),
            &plugin_options,
            self.core.store_factory.fleet_format(),
        )?;
        let request = SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: self.session_id.clone(),
            relation: parent
                .map(|parent_session_id| lash_core::SessionRelation::Child {
                    parent_session_id,
                    caused_by: None,
                })
                .unwrap_or_default(),
            config,
            head: SessionCreationHead::Config,
        };
        let catalog = Arc::clone(&self.core.store_factory);
        // The insert's own answer decides: only the admission that created the
        // row created the session. Every other answer about an existing id —
        // a rebind, or a rebind naming another relation — is the same
        // refusal, so a retry never adopts a session it did not create.
        match catalog.admit_session(&request).await {
            Ok(lash_core::store::SessionAdmission::Created) => {}
            Ok(lash_core::store::SessionAdmission::Rebound)
            | Err(lash_core::StoreError::SessionRelationMismatch { .. }) => {
                return Err(EmbedError::SessionAlreadyExists {
                    session_id: self.session_id,
                });
            }
            Err(error) => return Err(EmbedError::Store(error)),
        }
        let runtime: Arc<dyn lash_core::store::RuntimeStore> = catalog.clone();
        let store = lash_core::store::SessionStore::new(runtime, self.session_id.clone())?;
        let work = self.core.held_work().await;
        let ingress = self.core.ingress_relay(&work);
        Ok(DurableSession::from_binding(
            self.session_id,
            store,
            work,
            ingress,
            Arc::clone(&self.core.env.core.control.effect_host),
            Arc::clone(&self.core.live_replay_store),
            catalog,
            Arc::clone(&self.core.env.core.providers.provider_resolver),
        ))
    }

    /// This is for advanced hosts that already own a complete state snapshot.
    /// Normal embedders should use [`Self::open`] to resume according to Lash's
    /// durable history. Like `open`, it never creates: the session must exist.
    pub async fn open_with_state(self, state: RuntimeSessionState) -> Result<LashSession> {
        self.open_supplied_state(state, true).await
    }

    /// [`open_with_state`](Self::open_with_state) for a reader: the core's
    /// drives never run on this runtime. A host that loads a head without the
    /// session's lease to project or watch it opens it this way, so the
    /// session's turns keep running on an admitted open, or on one the
    /// engine opens itself, and never on the reader's snapshot. Like `open`,
    /// it never creates: the session must exist.
    pub async fn observe_with_state(self, state: RuntimeSessionState) -> Result<LashSession> {
        self.open_supplied_state(state, false).await
    }

    /// The supplied snapshot is the session's state, config included, and
    /// runs as supplied. Only a snapshot that states no config at all — no
    /// model and no provider — takes this open's live policy.
    async fn open_supplied_state(
        self,
        mut state: RuntimeSessionState,
        resident: bool,
    ) -> Result<LashSession> {
        if state.session_id != self.session_id {
            return Err(EmbedError::StoreSessionMismatch {
                loaded: state.session_id,
                requested: self.session_id,
            });
        }
        let resolved = self.existing_store().await?;
        let policy = self.live_policy();
        if state.policy.recorded_provider_id().is_empty() && state.policy.model.id.trim().is_empty()
        {
            state.policy = policy.clone();
        }
        self.reconcile_process_observer_intents(Some(&resolved.store))
            .await?;
        refuse_provider_mismatch(&state, &policy)?;
        adopt_live_policy(&mut state, &policy);
        Box::pin(self.open_resolved(state, resolved, resident)).await
    }

    /// The live policy this open runs with: the core's, with this open's
    /// provider resolver named as the provider it serves. It records nothing;
    /// [`adopt_live_policy`] carries only its live-owned facts onto the
    /// session's recorded config.
    fn live_policy(&self) -> SessionPolicy {
        let mut policy = self.core.policy.clone();
        if let Some(provider) = &self.provider {
            policy.provider_id = provider.kind().to_string();
        }
        policy.session_id = Some(self.session_id.clone());
        policy
    }

    /// The state an existing session opens with: what it recorded, as
    /// recorded (FIG-4099). Only live policy — the turn budget and the host's
    /// execution knobs — follows this open; nothing is written. A catalog row
    /// with no head — one its creator commits itself — starts from this open's
    /// live policy.
    async fn recorded_state(
        &self,
        store: &lash_core::store::SessionStore,
    ) -> Result<RuntimeSessionState> {
        let policy = self.live_policy();
        let Some(loaded) = load_persisted_window(store).await? else {
            return Ok(empty_runtime_session_state(self.session_id.clone(), policy));
        };
        let mut state = loaded.state;
        if state.session_id != self.session_id {
            return Err(EmbedError::StoreSessionMismatch {
                loaded: state.session_id,
                requested: self.session_id.clone(),
            });
        }
        refuse_provider_mismatch(&state, &policy)?;
        adopt_live_policy(&mut state, &policy);
        Ok(state)
    }

    /// Resolve this session's existing store through the catalog's
    /// non-creating seam: no catalog row is written. Absent is
    /// [`EmbedError::UnknownSession`]; deleted is
    /// [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted).
    async fn existing_store(&self) -> Result<ResolvedSessionStore> {
        let catalog = Arc::clone(&self.core.store_factory);
        let store = resolve_existing_session(&catalog, &self.session_id).await?;
        Ok(ResolvedSessionStore { store, catalog })
    }

    async fn open_resolved(
        self,
        state: RuntimeSessionState,
        resolved: ResolvedSessionStore,
        resident: bool,
    ) -> Result<LashSession> {
        let policy = state.effective_policy().clone();
        let session_id = state.session_id.clone();
        let mut env = self.core.env.clone();
        // What this open adds to the core's wiring has no recorded form, so a
        // group tool child of this session cannot be rebuilt without it and
        // waits for its live opener instead (FIG-3712).
        env.core.control.open_sources = lash_core::facade_support::UnrecordedSessionSources {
            open_plugins: !self.plugin_factories.is_empty(),
            open_provider: self.provider.is_some(),
            open_tool_policy: self.tool_source_policy.is_some()
                || self.tool_surface_open_mode.is_some(),
            ..Default::default()
        };
        if let Some(policy) = self.tool_source_policy {
            // Per-open override of the deployment default. It rides the env's
            // host config so every construction this open performs below the
            // facade sees the same choice.
            env.core.control.tool_source_policy = policy;
        }
        if let Some(mode) = self.tool_surface_open_mode {
            env.core.control.tool_surface_open_mode = mode;
        }
        if let Some(provider) = self.provider.clone().or_else(|| self.core.provider.clone()) {
            env.core.providers.provider_resolver = Arc::new(
                lash_core::facade_support::SingleProviderResolver::new(provider),
            );
        }
        refuse_foreign_backend_factories(&self.core.backend, &self.plugin_factories)?;
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
        let ports = self.core.substrate_slot.ports().await;
        env = env.with_work_ports(ports.process.clone(), ports.queued_port());
        let binding = Arc::new(
            BoundSession::new(
                session_id,
                resolved.store.clone(),
                &env,
                ports.process.clone(),
                Arc::clone(&ports.queued),
                Arc::clone(&self.core.residents),
                resolved.catalog,
            )
            .holding_tool_child_context_source(Arc::clone(&self.core.tool_child_context_source)),
        );
        env = binding.apply_owner(env);
        let recorded_parent_session_id =
            crate::session::recorded_parent_session_id(&binding.store()).await?;
        // Plugin options are creation config (FIG-4112): creation resolved
        // them into the recorded protocol options, so an open states none.
        let mut runtime = LashRuntime::from_environment_with_plugin_options(
            &env,
            policy,
            state,
            Some(binding.store()),
            PluginOptions::default(),
            self.core.drive_owner.clone(),
        )
        .await?;
        // Fire the protocol materialization hook: a session that recorded its
        // protocol options keeps them as recorded; one that recorded none
        // takes the protocol's defaults.
        runtime.configure_protocol_on_materialize(
            &PluginOptions::default(),
            recorded_parent_session_id.is_none(),
        )?;
        let handle = RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.core.live_replay_store),
        );
        let process_lifecycle_route = self.core.process_lifecycle_feed.register(&handle);
        if resident {
            binding.register_resident(&handle);
        }
        Ok(LashSession {
            runtime: handle,
            _process_lifecycle_route: process_lifecycle_route,
            binding,
            parent_session_id: recorded_parent_session_id,
        })
    }
}

/// Read the parent named by this session's durable relation.
///
/// The relation is written once at admission and guarded thereafter, so this
/// is the honest read-back the facade handle reports.
pub(crate) async fn recorded_parent_session_id(
    store: &lash_core::store::SessionStore,
) -> Result<Option<SessionId>> {
    Ok(store
        .load_session_meta()
        .await
        .map_err(EmbedError::Store)?
        .and_then(|meta| {
            meta.relation
                .parent_session_id()
                .map(|parent_session_id| SessionId::from(parent_session_id.to_string()))
        }))
}

/// Resolve `session_id`'s existing store through the catalog's non-creating
/// seam (ADR 0112 §2: `lookup_session` plus `SessionStore::new`). Nothing is
/// written: an absent id is [`EmbedError::UnknownSession`] and a deleted one
/// [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted). Every
/// verb but [`SessionBuilder::create`] reaches a session this way (FIG-4112).
pub(crate) async fn resolve_existing_session(
    catalog: &Arc<dyn lash_core::DeploymentStore>,
    session_id: &SessionId,
) -> Result<lash_core::store::SessionStore> {
    match catalog
        .lookup_session(session_id)
        .await
        .map_err(EmbedError::Store)?
    {
        lash_core::store::SessionLookup::Live(_) => {
            let runtime: Arc<dyn lash_core::store::RuntimeStore> = catalog.clone();
            Ok(lash_core::store::SessionStore::new(
                runtime,
                session_id.clone(),
            )?)
        }
        lash_core::store::SessionLookup::Deleted => {
            Err(EmbedError::Store(lash_core::StoreError::SessionDeleted {
                session_id: session_id.clone(),
            }))
        }
        lash_core::store::SessionLookup::Absent => Err(EmbedError::UnknownSession {
            session_id: session_id.clone(),
        }),
    }
}

/// The state the engine opens `session_id` with: what the session recorded,
/// as recorded, with the engine's live policy (FIG-4099). A catalog row with
/// no head starts from `policy`.
pub(crate) async fn load_state_from_store(
    session_id: &SessionId,
    policy: &SessionPolicy,
    store: &lash_core::store::SessionStore,
) -> Result<RuntimeSessionState> {
    let Some(loaded) = lash_core::store::load_session_window_state(
        store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .map_err(EmbedError::Store)?
    else {
        return Ok(empty_runtime_session_state(session_id, policy.clone()));
    };
    let mut state = loaded.state;
    if state.session_id != session_id {
        return Err(EmbedError::StoreSessionMismatch {
            loaded: state.session_id,
            requested: session_id.clone(),
        });
    }
    adopt_live_policy(&mut state, policy);
    Ok(state)
}

/// The protocol turn options a session is created with (FIG-4099): what the
/// session's protocol plugin resolves `plugin_options` to at the session's
/// first materialization — the stated facts and the protocol's defaults —
/// computed before the catalog write so they are baked into the creation
/// head. `None` when the core runs no protocol plugin.
pub(crate) fn creation_protocol_turn_options(
    protocol_factory: Option<&Arc<dyn PluginFactory>>,
    session_id: &SessionId,
    parent_session_id: Option<SessionId>,
    plugin_options: &PluginOptions,
    fleet_format: lash_core::FleetFormat,
) -> Result<Option<lash_core::ProtocolTurnOptions>> {
    let Some(protocol_factory) = protocol_factory else {
        return Ok(None);
    };
    let is_root_session = parent_session_id.is_none();
    let plugin_host = build_plugin_host(Some(protocol_factory), &[], Vec::new())?;
    let plugins = plugin_host
        .build_session(PluginSessionRequest {
            parent_session_id,
            ..PluginSessionRequest::creation(
                session_id.clone(),
                lash_core::plugin::SessionCreationConfig {
                    authority: lash_core::plugin::SessionAuthorityContext {
                        plugin_options: plugin_options.clone(),
                        ..Default::default()
                    },
                    protocol_turn_options: lash_core::ProtocolTurnOptions::default(),
                },
            )
        })
        .map_err(EmbedError::Plugin)?;
    let mut options = lash_core::ProtocolTurnOptions::default();
    plugins
        .protocol_session()
        .configure_runtime_on_materialize(
            lash_core::plugin::ProtocolRuntimeContext::new(&mut options, fleet_format),
            lash_core::plugin::ProtocolSessionMaterialization {
                plugin_options,
                is_root_session,
            },
        )?;
    Ok(Some(options))
}

/// Carry an open's live policy onto recorded state (FIG-4099).
///
/// The recorded config — provider, model, attachment acceptance, prompt,
/// generation — is the session's and stays as recorded. The facts ADR 0030
/// leaves live-owned follow the open: its session binding, the turn budget,
/// autonomy, the no-progress budget and the charge-safety policy.
///
/// A head that records no model or no provider pin — a creator that stated
/// none, or a head written before creation recorded config by a first commit
/// that carried an empty config — has nothing to keep for that fact, so the
/// open's value fills the absence in memory. Nothing is written; the
/// session's next commit records what it ran with.
fn adopt_live_policy(state: &mut RuntimeSessionState, policy: &SessionPolicy) {
    if state.policy.model.id.trim().is_empty() {
        state.policy.model = policy.model.clone();
    }
    if state.policy.recorded_provider_id().is_empty() {
        state.policy.provider_id = policy.provider_id.clone();
    }
    state.policy.session_id = policy.session_id.clone();
    state.policy.autonomous = policy.autonomous;
    state.policy.turn_budget = policy.turn_budget;
    state.policy.no_progress_budget = policy.no_progress_budget;
    state.policy.charge_safety = policy.charge_safety.clone();
}

/// Refuse an open whose live provider cannot serve the session's recorded
/// provider pin (ADR 0066). The pin is only read here, never written: an open
/// that names no provider, or the recorded one, passes.
fn refuse_provider_mismatch(state: &RuntimeSessionState, policy: &SessionPolicy) -> Result<()> {
    SessionPolicy::settle_provider_pin(
        &state.session_id,
        state.policy.recorded_provider_id(),
        policy.recorded_provider_id(),
    )
    .map_err(lash_core::SessionError::from)?;
    Ok(())
}

/// The session's current frame as runtime state, after the store confirms
/// this build can read its session-state version.
async fn load_persisted_window(
    store: &lash_core::store::SessionStore,
) -> Result<Option<lash_core::store::LoadedSessionWindow>> {
    Ok(lash_core::store::load_session_window_state(
        store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .map_err(|source| SessionError::Store {
        context: "failed to admit and load store".to_string(),
        source,
    })?)
}

#[derive(Clone)]
pub struct LashSession {
    pub(crate) runtime: RuntimeHandle,
    pub(crate) _process_lifecycle_route: Arc<crate::process_lifecycle::ProcessLifecycleRoute>,
    pub(crate) binding: Arc<BoundSession>,
    pub(crate) parent_session_id: Option<SessionId>,
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
    pub(crate) inner: lash_core::facade_support::ParkedSession,
    pub(crate) binding: Arc<BoundSession>,
}

impl ParkedSession {
    /// The parked session's id. Use it to key a per-session cache of parked
    /// handles on the host.
    pub fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
}

impl LashSession {
    /// The lifecycle owner of this session: the catalog, effect host,
    /// process services and trigger store its open was bound to.
    pub fn session_administration(&self) -> lash_core::SessionAdministration {
        self.binding.administration()
    }

    /// What this session's open (or its latest internal reload) found when it
    /// installed the persisted Tool Catalog.
    ///
    /// `None` means no persisted tool state was installed — a first open of a
    /// fresh session. A present report says which persisted tools no registered
    /// source resolves, in three classes:
    ///
    /// * `lost_members` — capability loss. Surface these to your user: the
    ///   session opened, but a tool the host had curated in is not callable
    ///   until its source returns. Under
    ///   [`ToolSourcePolicy::Require`](lash_core::ToolSourcePolicy::Require)
    ///   this list is what refuses the open instead.
    /// * `parked_opt_outs` — unresolved tools the host had already opted out
    ///   of. Nothing usable is missing.
    /// * `superseded_identities` — a live tool now owns the old tool's
    ///   model-facing name. The capability is present under a new id.
    ///
    /// The report is replaced by every later host restore, persisted-state
    /// install and resident re-sync on this session, so a host that renders it
    /// after a long-lived session's re-sync sees the current answer.
    pub async fn tool_restore_report(&self) -> Option<crate::tools::ToolRestoreReport> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        runtime.tool_restore_report().cloned()
    }

    /// Durably close this session, then release its in-memory runtime.
    ///
    /// `close` is the honest teardown verb: a persistent session flushes its
    /// dirty state (via a fresh-lease commit) so the store reflects the final
    /// transcript, its in-memory plugin session is unregistered, and the live
    /// runtime is dropped.
    ///
    /// This consumes the session and requires exclusive ownership: any cloned
    /// [`LashSession`] handle or in-flight turn keeps a live reference to the
    /// same runtime, so `close` returns [`EmbedError::SessionStillInUse`] until
    /// those are dropped or finished. Cancel a running send first with
    /// [`SendHandle::cancel`](crate::SendHandle::cancel) if needed.
    ///
    /// To keep a handle for later resumption instead of discarding the session,
    /// use [`park`](Self::park).
    pub async fn close(self) -> Result<()> {
        let runtime = self.into_owned_runtime().await?;
        runtime.unregister_plugin_session()?;
        // Reuse the core parking primitive to flush + release the lease,
        // discarding the returned handle: close does not resume.
        Box::pin(runtime.park()).await?;
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
    /// - **Exclusive ownership required.** `park` consumes the session and drops
    ///   the in-memory runtime, so it needs the sole live reference. A cloned
    ///   [`LashSession`] or an in-flight turn holds another reference and makes
    ///   `park` return [`EmbedError::SessionStillInUse`]. Because an executing
    ///   turn holds such a reference, parking is effectively an *idle-session*
    ///   operation: finish or cancel ([`SendHandle::cancel`](crate::SendHandle::cancel))
    ///   first. The store commit itself does not observe an active turn; the
    ///   exclusive-ownership guard is what makes mid-turn parking an explicit
    ///   error rather than a silent partial flush.
    pub async fn park(self) -> Result<ParkedSession> {
        let binding = Arc::clone(&self.binding);
        let runtime = self.into_owned_runtime().await?;
        // We now own the runtime exclusively; release the in-memory plugin
        // session registration before flushing and dropping it.
        runtime.unregister_plugin_session()?;
        let parked = Box::pin(runtime.park()).await?;
        Ok(ParkedSession {
            inner: parked,
            binding,
        })
    }

    /// Consume the session and take sole ownership of the underlying runtime.
    ///
    /// Fails with [`EmbedError::SessionStillInUse`] when another live handle
    /// (a cloned session or an in-flight turn) shares the runtime, so
    /// consuming operations never proceed on a still-shared runtime.
    ///
    /// The core's session driver runs drives on an open session's runtime, so
    /// the session is first withdrawn from the drives and a drive already
    /// running on it is let stop; a failed take lends it to them again.
    async fn into_owned_runtime(self) -> Result<LashRuntime> {
        let LashSession {
            runtime, binding, ..
        } = self;
        let was_resident = binding.release_resident(&runtime).await;
        let weak = runtime.downgrade();
        // `writer()` clones the shared `Arc<Mutex<LashRuntime>>`; dropping the
        // handle then leaves this clone as the sole strong reference iff no
        // other handle exists, so `try_unwrap` doubles as the exclusive-owner
        // check.
        let writer = runtime.writer();
        drop(runtime);
        match Arc::try_unwrap(writer) {
            Ok(mutex) => Ok(mutex.into_inner()),
            Err(writer) => {
                drop(writer);
                if was_resident && let Some(handle) = weak.upgrade() {
                    binding.register_resident(&handle);
                }
                Err(EmbedError::SessionStillInUse)
            }
        }
    }

    pub fn session_id(&self) -> SessionId {
        SessionId::from(self.runtime.observe().session_id())
    }

    /// The scope uses the exact store-backed session identity owned by this
    /// facade handle's Session Binding.
    pub fn turn_scope(&self, turn_id: impl Into<TurnId>) -> lash_core::ExecutionScope {
        self.runtime.observe().turn_scope(turn_id)
    }

    /// Build the cancellation and terminal-observation address for a turn.
    pub fn turn_address(
        &self,
        turn_id: impl Into<TurnId>,
    ) -> lash_core::facade_support::TurnAddress {
        let observation = self.runtime.observe();
        lash_core::facade_support::TurnAddress::new(observation.session_id(), turn_id)
    }

    /// Returns a snapshot of the session policy.
    pub fn policy_snapshot(&self) -> SessionPolicy {
        self.runtime.observe().read_view.policy().clone()
    }

    /// Returns an observable handle for session read models and replay.
    pub fn observe(&self) -> ObservableSession {
        ObservableSession {
            runtime: self.runtime.clone(),
        }
    }

    /// Returns the parent session identifier recorded in this session's
    /// durable metadata, if any.
    ///
    /// This is the store's answer, read at open, not the `.parent(..)` request
    /// this handle was built from: a reopen that named no parent still reports
    /// the recorded one, and a conflicting `.parent(..)` is refused at open
    /// rather than shadowing the durable relation.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.parent_session_id.as_deref()
    }

    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        self.binding.effect_host()
    }

    /// Accept `input` durably and ask the engine to drive the session: the
    /// one way a turn starts (FIG-3600).
    ///
    /// Awaiting the builder commits the acceptance and yields a
    /// [`SendHandle`](crate::SendHandle); `send(input).output().await` is the
    /// one-call form. The turn runs on the session's engine, not in the
    /// caller's future: dropping the handle stops nothing.
    pub fn send(&self, input: TurnInput) -> crate::SendBuilder {
        crate::SendBuilder::new(crate::send::SendTarget::Live(self.clone()), input)
    }

    /// Accept `inputs` durably as one request under one shared spec, and ask
    /// the engine to drive the session (FIG-3842).
    ///
    /// Awaiting the builder yields one [`SendHandle`](crate::SendHandle) per
    /// input, in request order. New ids are enqueued in request order as one
    /// contiguous block; an id already accepted with the same content returns
    /// its existing handle; an id accepted with other content, or one id
    /// named twice, refuses the whole request and accepts nothing.
    pub fn send_batch<I>(&self, inputs: impl IntoIterator<Item = I>) -> crate::SendBatchBuilder
    where
        I: Into<crate::BatchInput>,
    {
        crate::SendBatchBuilder::new(
            crate::send::SendTarget::Live(self.clone()),
            inputs.into_iter().map(Into::into).collect(),
        )
    }

    /// Re-attach to an input accepted earlier: after a restart, or from
    /// another handle. Never commits anything.
    pub fn attach(&self, input_id: lash_core::InputId) -> crate::SendHandle {
        crate::send::attach(crate::send::SendTarget::Live(self.clone()), input_id)
    }

    /// Re-attach to the input a send accepted under host id `id`
    /// ([`SendBuilder::id`](crate::SendBuilder::id)): after a restart, with
    /// nothing but the id. It follows the input wherever it went, including
    /// into another root, and never commits anything.
    pub fn attach_id(&self, id: impl Into<TurnId>) -> crate::SendHandle {
        crate::send::attach_id(crate::send::SendTarget::Live(self.clone()), id.into())
    }

    /// Re-await a logical root: after a park verb, or by the host id a send
    /// named.
    pub fn root(&self, root: impl Into<TurnId>) -> crate::RootHandle {
        crate::send::root(crate::send::SendTarget::Live(self.clone()), root.into())
    }

    /// Withdraw a queued input, or cooperatively cancel a running root
    /// (ADR 0039).
    pub fn cancel(&self, target: crate::CancelTarget) -> crate::CancelBuilder {
        crate::CancelBuilder::new(crate::send::SendTarget::Live(self.clone()), target)
    }

    pub fn admin(&self) -> SessionAdmin {
        SessionAdmin {
            runtime: self.runtime.clone(),
            process_work: Arc::clone(self.binding.process().port()),
            work: self.binding.queued(),
            ingress: self.binding.ingress_relay(),
        }
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

    /// This session's **Durable Session**: store-backed access to its queue
    /// and settled reads.
    ///
    /// The queue and read operations that stay correct beside another
    /// process's writer live on [`DurableSession`], not here, so the type says
    /// which authority a host is using. This handle is derived from the
    /// session's Session Binding: it reuses the binding's admitted store and
    /// owner-issued ports and never manufactures a catalog, so catalog-only
    /// reads stay optional with the same typed errors the rest of the facade
    /// returns.
    ///
    /// ```ignore
    /// let pending = session.durable().pending_turn_inputs().await?;
    /// session.durable().send(input).id("draft-1").await?;
    /// ```
    pub fn durable(&self) -> DurableSession {
        DurableSession::from_binding(
            SessionId::from(self.runtime.observe().session_id()),
            self.binding.store(),
            self.binding.work(),
            self.binding.ingress_relay(),
            self.binding.effect_host(),
            Arc::clone(&self.runtime.live_replay_store),
            self.binding.catalog(),
            self.binding.provider_resolver(),
        )
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
        self.binding
            .effect_host()
            .cancel_await_events_for_session(&session_id)
            .await
            .map_err(EmbedError::Runtime)
    }

    pub fn read_view(&self) -> SessionReadView {
        self.runtime.observe().read_view.clone()
    }

    pub fn usage_report(&self) -> SessionUsageReport {
        self.runtime.observe().usage_report.clone()
    }

    /// Attempts of finished turns whose provider usage never arrived after a
    /// protocol abort or a failure, not yet reconciled. Each is already
    /// counted in [`usage_report`](Self::usage_report) as an unreported row;
    /// [`reconcile_unreported_usage`](Self::reconcile_unreported_usage) fills
    /// them.
    pub async fn unreported_usage_attempts(&self) -> Vec<UnreportedUsageAttempt> {
        let writer = self.runtime.writer();
        let runtime = writer.lock().await;
        runtime.unreported_usage_attempts().to_vec()
    }

    /// Ask the session's provider for the usage of every unreported attempt
    /// and append one correction row per recovered generation. Host-invoked
    /// (a billing sweep, an idle hook), never on the turn's hot path; each
    /// lookup is bounded by the provider. Attempts the provider cannot resolve
    /// stay registered and return as `unresolved`.
    pub async fn reconcile_unreported_usage(&self) -> Result<UsageReconciliationReport> {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let report = runtime.reconcile_unreported_usage().await?;
        self.runtime.publish_resident_from(&runtime);
        Ok(report)
    }

    pub async fn set_turn_phase_probe(
        &self,
        probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>,
    ) {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let changed = runtime.set_turn_phase_probe_if_changed(probe);
        if changed {
            self.runtime.publish_resident_from(&runtime);
        }
    }
}

#[derive(Clone)]
/// Exposes read models and replayable observations for an active session.
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

    /// Resumes local observations from the supplied replay cursor.
    pub fn resume_from_cursor(&self, cursor: &SessionCursor) -> Result<SessionResume> {
        self.runtime
            .resume_session_observation(cursor)
            .map_err(live_replay_error)
    }

    /// Subscribes to local observations from the supplied replay cursor.
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

    pub fn session_id(&self) -> SessionId {
        SessionId::from(self.snapshot().session_id())
    }

    /// Returns a snapshot of the session policy.
    pub fn policy_snapshot(&self) -> SessionPolicy {
        self.snapshot().read_view.policy().clone()
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

    /// Lists process handles.
    pub async fn list_process_handles(&self) -> Vec<ProcessHandleView> {
        self.snapshot().list_process_handles().await
    }

    /// Lists all process handles.
    pub async fn list_all_process_handles(&self) -> Vec<ProcessHandleView> {
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
/// Item delivered by a local session-observation stream.
pub enum SessionObservationStreamItem {
    /// A replayed or live session observation event.
    Event(Arc<SessionObservationEvent>),
    /// A recoverable replay gap with a fresh durable observation.
    Gap {
        /// Fresh durable observation used to replace a stale projection.
        observation: SessionObservation,
        /// Replay gap that required snapshot replacement.
        gap: LiveReplayGap,
    },
}

/// Result of subscribing to remote observations from a replay cursor.
pub enum RemoteSessionObservationSubscription {
    /// Carries a successfully established remote observation stream.
    Subscribed(RemoteSessionObservationEventStream),
    /// Carries the snapshot and replay gap encountered while subscribing.
    Gap {
        /// Fresh remote observation used to replace a stale projection.
        observation: RemoteSessionObservation,
        /// Replay gap that required snapshot replacement.
        gap: RemoteLiveReplayGap,
    },
}

#[derive(Clone, Debug)]
/// Item delivered by a remote session-observation stream.
pub enum RemoteSessionObservationStreamItem {
    /// A replayed or live session observation event encoded as remote DTOs.
    Event(RemoteSessionObservationEvent),
    /// A recoverable replay gap with a fresh remote observation snapshot.
    Gap {
        /// Fresh remote observation used to replace a stale projection.
        observation: RemoteSessionObservation,
        /// Replay gap that required snapshot replacement.
        gap: RemoteLiveReplayGap,
    },
}

/// Stream of remote session observation event activity.
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
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.saturating_add(1);
                let remote = match RemoteSessionObservationEvent::from_core(sequence, event) {
                    Ok(remote) => remote,
                    Err(err) => return Poll::Ready(Some(Err(err.into()))),
                };
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
    /// Returns the stream's current replay cursor.
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
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.saturating_add(1);
                let remote = match RemoteSessionObservationEvent::from_core(sequence, event) {
                    Ok(remote) => remote,
                    Err(err) => return Poll::Ready(Some(Err(err.into()))),
                };
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
    #[cfg(test)]
    pub(crate) fn live_receiver_installed(&self) -> bool {
        self.subscription.is_some()
    }

    /// Returns the stream's current replay cursor.
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
        RuntimeErrorCode::LiveReplay,
        err.to_string(),
    ))
}

#[cfg(test)]
mod observation_stream_tests {
    use super::*;

    #[tokio::test]
    async fn remote_observation_event_stream_advances_sequence_past_events() {
        use lash_core::LiveReplayStore;

        let store = lash_core::facade_support::InMemoryLiveReplayStore::default();
        let cursor = store.current_cursor(
            &SessionId::from("session-seq-test"),
            lash_core::SessionRevision::new(0),
        );
        let activity1 = lash_core::TurnActivity {
            id: lash_core::TurnActivityId::new("act-1"),
            correlation_id: lash_core::TurnActivityId::new("corr-1"),
            event: lash_core::TurnEvent::AssistantProseDelta {
                text: "hello".into(),
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        };
        let activity2 = lash_core::TurnActivity {
            id: lash_core::TurnActivityId::new("act-2"),
            correlation_id: lash_core::TurnActivityId::new("corr-2"),
            event: lash_core::TurnEvent::AssistantProseDelta {
                text: "world".into(),
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        };
        for (revision, activity) in [(1, activity1), (2, activity2)] {
            let prepared = store
                .prepare_publication(
                    &SessionId::from("session-seq-test"),
                    lash_core::SessionRevision::new(revision),
                    vec![lash_core::LiveReplayEventDraft::new(
                        Some("turn-1"),
                        lash_core::SessionObservationEventPayload::TurnActivity(activity),
                    )],
                )
                .expect("prepare event");
            store.publish_prepared(prepared).expect("publish event");
        }

        let subscription = store.subscribe_after_cursor(&cursor).expect("subscribe");
        let lash_core::LiveReplaySubscribeOutcome::Subscribed(sub) = subscription else {
            panic!("expected subscribed");
        };

        let mut stream = RemoteSessionObservationEventStream::new(sub);
        assert_eq!(stream.next_sequence, 0);

        let event1 = stream.next_event().await.expect("first event");
        let lash_remote_protocol::RemoteSessionObservationEventPayload::TurnActivity {
            activity: act1,
        } = event1.event
        else {
            panic!("expected turn activity");
        };
        assert_eq!(act1.sequence, 0);
        assert_eq!(stream.next_sequence, 1);

        let event2 = stream.next_event().await.expect("second event");
        let lash_remote_protocol::RemoteSessionObservationEventPayload::TurnActivity {
            activity: act2,
        } = event2.event
        else {
            panic!("expected turn activity");
        };
        assert_eq!(act2.sequence, 1);
        assert_eq!(stream.next_sequence, 2);
    }
}
