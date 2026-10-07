use lash_core::ActorContext;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::durable_session::DurableSession;
pub use crate::observation_feed::SessionObservationStream;
use crate::observation_feed::live_replay_error;
use crate::session_binding::BoundSession;
use crate::support::{
    Arc, EmbedError, LashCore, LashRuntime, PluginOperations, ProcessHandleView, Result,
    RuntimeHandle, RuntimeObservation, RuntimeSessionState, SessionAdmin, SessionCreationHead,
    SessionCursor, SessionError, SessionObservation, SessionObservationSubscription, SessionPolicy,
    SessionReadView, SessionResume, SessionScope, SessionSpec, SessionStoreCreateRequest,
    ToolManifest, ToolState, TurnInput, build_plugin_host,
};
use futures_util::Stream;
use lash_core::facade_support::ToolStateFacadeOps;
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
/// The builder carries only what one open supplies — the tool-source policy:
/// physical binding and acquisition, never behaviour. A session's behaviour is recorded config:
/// the model it runs is the binding it recorded, and the plugins it runs are
/// the core's, configured by the plugin config it recorded at creation
/// (FIG-4396). It is stated once, in the [`SessionCreation`] passed to
/// [`create`](Self::create), and changed afterwards only through a config
/// transaction ([`SessionConfigAdmin::apply`](crate::admin::SessionConfigAdmin::apply)).
pub struct SessionBuilder {
    pub(crate) core: LashCore,
    pub(crate) session_id: SessionId,

    /// Per-open override of the core's tool-source policy (FIG-3367).
    pub(crate) tool_source_policy: Option<lash_core::ToolSourcePolicy>,
}

/// What a session is created with: the argument of
/// [`SessionBuilder::create`], the only verb that creates a session and the
/// only one that takes session config (FIG-4112).
///
/// Creation writes all of it once, with the session's catalog row, in one
/// store transaction. Nothing here is restated on open.
#[derive(Clone, Debug)]
pub struct SessionCreation {
    /// The session's whole config, stated by its creator (FIG-4594): the
    /// model key, turn budget and tool-call limit [`SessionSpec::new`] takes, and reasoning,
    /// attachment acceptance, generation, the other execution controls and
    /// plugin creation options as its setters state them. Nothing of the
    /// core stands beneath it; a field left unstated takes the neutral value
    /// lash documents for it. A host that wants a default keeps its own
    /// `SessionSpec` value and passes it. The model key is minted into a
    /// recorded binding by the core's models when the session is created;
    /// every open runs that recorded binding.
    ///
    /// Every plugin the core installs, the protocol among them, creates its
    /// own namespace from its key of the spec's
    /// [`plugin_options`](SessionSpec::plugin_options), the plugin's own
    /// built-in defaults included, and the result is recorded with the
    /// session's initial config head: every open delivers it unchanged, and
    /// only its owner's typed config commands change it ([`crate::config`]).
    /// The protocol plugin's prompt config is one of these options. A key no
    /// installed plugin owns, or a value its owner refuses, fails the
    /// creation typed as
    /// [`SessionConfigRefused`](lash_core::SessionError::SessionConfigRefused).
    pub spec: SessionSpec,
    /// The session's parent, recorded as its Session Relation (ADR 0089).
    /// This is the only facade path to a related session: the session is an
    /// ordinary session with its own Session Binding and its own usage
    /// ledger — rolling related sessions together is host policy, not a
    /// facade service. `None` creates a root session.
    pub parent: Option<SessionId>,
}

impl SessionCreation {
    /// A root session created from `spec`.
    pub fn root(spec: SessionSpec) -> Self {
        Self { spec, parent: None }
    }

    /// A session created from `spec` and recorded as `parent`'s child
    /// (ADR 0089). It is an ordinary session: its config is `spec`, not its
    /// parent's.
    pub fn child_of(parent: SessionId, spec: SessionSpec) -> Self {
        Self {
            spec,
            parent: Some(parent),
        }
    }
}

struct ResolvedSessionStore {
    store: lash_core::store::SessionStore,
    catalog: Arc<dyn lash_core::DeploymentStore>,
}

impl SessionBuilder {
    /// Override the core's tool-source policy for the runs this open hosts.
    ///
    /// The core's choice is the deployment default; this states it for one
    /// session — an unattended reopen that must not run without its tools sets
    /// [`Require`](lash_core::ToolSourcePolicy::Require) even on a core that
    /// tolerates loss elsewhere. The open itself builds no capabilities and
    /// never refuses: a run this open hosts that would lose a persisted
    /// member is refused at its plugin transition, and its sender reads
    /// [`SendOutcome::Refused`](crate::SendOutcome::Refused) with a
    /// [`RuntimeErrorCode::ToolSourcesUnavailable`](lash_core::RuntimeErrorCode::ToolSourcesUnavailable)
    /// refusal that carries the restore report (FIG-5134).
    pub fn tool_source_policy(mut self, policy: lash_core::ToolSourcePolicy) -> Self {
        self.tool_source_policy = Some(policy);
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
    /// a config transaction ([`SessionConfigAdmin::apply`](crate::admin::SessionConfigAdmin::apply)).
    pub async fn open(self) -> Result<LashSession> {
        let resolved = self.existing_store().await?;
        self.reconcile_process_observer_intents(Some(&resolved.store))
            .await?;
        let state = self.recorded_state(&resolved.store).await?;
        Box::pin(self.open_resolved(state, resolved)).await
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
        Ok(self.catalog_durable().await)
    }

    /// The Durable Session resolved through the catalog on first use: building
    /// the handle reads nothing, so a Restate host may build it before its
    /// journal answers.
    pub(crate) async fn catalog_durable(self) -> DurableSession {
        let live_replay_store = Arc::clone(&self.core.live_replay_store);
        DurableSession::from_catalog(
            self.session_id,
            Arc::clone(&self.core.store_factory),
            self.core.env.core.control.effect_host.clone(),
            live_replay_store,
            Arc::clone(&self.core.env.core.providers.models),
            Arc::clone(self.core.env.core.tracing.scopes()),
        )
        .with_transcript_options(self.core.transcript_options())
    }

    /// Create this session, then return its **Durable Session**.
    ///
    /// The only verb that creates a session, and the only one that takes
    /// session config (FIG-4112). It writes the session's catalog row and its
    /// initial config head — `creation`'s spec, relation and the plugin
    /// configuration its owners resolved — in one store transaction, and stops there: no runtime, no
    /// Session Execution Lease, no plugin session, no lifecycle event. Run the
    /// session with [`open`](Self::open), or admit durable input for its first
    /// turn with `create(creation).await?.send(input)`. The builder's open
    /// knob, the tool-source policy, belongs to an open and takes no part in
    /// creation.
    ///
    /// The creation spec's model key is minted into a recorded binding here,
    /// through the core's models; a key they do not register is refused with
    /// [`EmbedError::LlmProfileUnknown`] and nothing is created. A spec that
    /// states no model, no turn budget or no tool-call limit (a
    /// [`SessionSpec::inherit`] overlay) is refused with
    /// [`EmbedError::MissingLlmProfile`], [`EmbedError::MissingTurnBudget`] or
    /// [`EmbedError::MissingMaxToolCalls`]: a creation has no base to take
    /// them from.
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
    ///
    /// A config whose created head no commit fits under the core's commit
    /// budget is refused with the store's typed
    /// [`CommitByteBudgetExceeded`](lash_core::StoreError::CommitByteBudgetExceeded),
    /// and nothing is written.
    ///
    /// Charge safety above [`ChargeSafetyPolicy::MAX_UNSAFE_RETRIES`](crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES)
    /// is refused before admission as [`CoreConfigRefusal::UnsafeRetriesAboveCeiling`](crate::config::CoreConfigRefusal::UnsafeRetriesAboveCeiling)
    /// inside [`SessionError::SessionConfigRefused`].
    pub async fn create(self, creation: SessionCreation) -> Result<DurableSession> {
        let SessionCreation { spec, parent } = creation;
        // A session created here is an ordinary session even when it names
        // a parent (ADR 0089): it records the spec its creator states,
        // unlike a child a running parent creates, which copies its parent's
        // recorded config.
        let policy = self.minted_policy(&spec)?;
        let plugin_options = spec.plugin_options;
        lash_core::CoreConfigOwner::validate_charge_safety(&policy.charge_safety)
            .map_err(lash_core::CoreConfigOwner::creation_refusal)
            .map_err(lash_core::SessionError::SessionConfigRefused)?;
        let mut config = lash_core::PersistedSessionConfig::from(&policy);
        // Every plugin the core installs resolves its recorded namespace —
        // the protocol's among them — from what the creator stated (FIG-4379).
        let plugin_host = build_plugin_host(
            self.core.protocol_factory.as_ref(),
            self.core.plugin_factories.as_ref(),
            &self.core.env.core,
        )?;
        // Creation is an adoption point of its own (FIG-4747): the created
        // head's namespaces are written in the formats the fleet record
        // permits now.
        let admission = plugin_host
            .admit_plugins(self.core.store_factory.as_ref())
            .await
            .map_err(EmbedError::Store)?;
        config.plugin_config = plugin_host
            .resolve_creation_plugin_config(
                self.core
                    .protocol_factory
                    .as_ref()
                    .map(|protocol_factory| protocol_factory.id()),
                &plugin_options,
                None,
                parent.is_none(),
                &admission,
            )
            .map_err(lash_core::SessionError::from)?;
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
        // The created head is written outside any runtime commit, so its
        // creation is measured against the commit budget (FIG-4393).
        match lash_core::store::admit_created_session(
            catalog.as_ref(),
            &request,
            self.core.env.core.durability.commit_budget,
            catalog.fleet_format(),
        )
        .await
        {
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
        Ok(DurableSession::from_binding(
            self.session_id,
            store,
            self.core.env.core.control.effect_host.clone(),
            Arc::clone(&self.core.live_replay_store),
            catalog,
            Arc::clone(&self.core.env.core.providers.models),
            Arc::clone(self.core.env.core.tracing.scopes()),
        )
        .with_transcript_options(self.core.transcript_options()))
    }

    /// This is for advanced hosts that already own a complete state snapshot.
    /// Normal embedders should use [`Self::open`] to resume according to Lash's
    /// durable history. Like `open`, it never creates: the session must exist.
    pub async fn open_with_state(self, state: RuntimeSessionState) -> Result<LashSession> {
        self.open_supplied_state(state).await
    }

    /// [`open_with_state`](Self::open_with_state) for a reader: a host that
    /// loads a head without the session's lease to project or watch it opens
    /// it this way. The session's turns run on its session actor, never on
    /// the reader's snapshot. Like `open`, it never creates: the session must
    /// exist.
    pub async fn observe_with_state(self, state: RuntimeSessionState) -> Result<LashSession> {
        self.open_supplied_state(state).await
    }

    /// The supplied snapshot is the session's state, config included, and
    /// runs as supplied: its recorded model is never replaced or filled.
    async fn open_supplied_state(self, state: RuntimeSessionState) -> Result<LashSession> {
        if state.session_id != self.session_id {
            return Err(EmbedError::Store(
                lash_core::StoreError::StoreSessionMismatch {
                    loaded: state.session_id,
                    requested: self.session_id,
                },
            ));
        }
        let resolved = self.existing_store().await?;
        self.reconcile_process_observer_intents(Some(&resolved.store))
            .await?;
        Box::pin(self.open_resolved(state, resolved)).await
    }

    /// The policy a run records from `spec` alone (FIG-4594): nothing of
    /// this core stands beneath it. The model key is minted into a recorded
    /// binding by the core's models now.
    fn minted_policy(&self, spec: &SessionSpec) -> Result<SessionPolicy> {
        let policy = spec
            .resolve_root(self.core.env.core.providers.models.as_ref())
            .map_err(|error| match error {
                lash_core::facade_support::SpecResolveError::Model(error) => {
                    EmbedError::LlmProfileUnknown(error)
                }
                lash_core::facade_support::SpecResolveError::ReasoningWithoutLlmProfile
                | lash_core::facade_support::SpecResolveError::RootWithoutLlmProfile => {
                    EmbedError::MissingLlmProfile
                }
                lash_core::facade_support::SpecResolveError::RootWithoutTurnBudget => {
                    EmbedError::MissingTurnBudget
                }
                lash_core::facade_support::SpecResolveError::RootWithoutMaxToolCalls => {
                    EmbedError::MissingMaxToolCalls
                }
                lash_core::facade_support::SpecResolveError::Reasoning(error) => {
                    EmbedError::ReasoningRefused(error)
                }
            })?;
        Ok(policy)
    }

    /// The state an existing session opens with: what it recorded, as
    /// recorded (FIG-4099), its execution controls included (FIG-4376).
    /// Nothing is written. A catalog row with no head recorded no config is
    /// refused with
    /// [`EmbedError::SessionCreationUnrecorded`]: no open stands defaults in
    /// for it (FIG-4553).
    async fn recorded_state(
        &self,
        store: &lash_core::store::SessionStore,
    ) -> Result<RuntimeSessionState> {
        let Some(loaded) = load_persisted_window(store).await? else {
            return Err(EmbedError::SessionCreationUnrecorded {
                session_id: self.session_id.clone(),
            });
        };
        let state = loaded.state;
        if state.session_id != self.session_id {
            return Err(EmbedError::Store(
                lash_core::StoreError::StoreSessionMismatch {
                    loaded: state.session_id,
                    requested: self.session_id.clone(),
                },
            ));
        }
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
    ) -> Result<LashSession> {
        let policy = state.effective_policy().clone();
        let mut env = self.core.env.clone();
        if let Some(policy) = self.tool_source_policy {
            // Per-open override of the deployment default. It rides the env's
            // host config so every construction this open performs below the
            // facade sees the same choice.
            env.core.control.tool_source_policy = policy;
        }

        let plugin_host = build_plugin_host(
            self.core.protocol_factory.as_ref(),
            self.core.plugin_factories.as_ref(),
            &self.core.env.core,
        )?;
        env.core = plugin_host.install_process_engine_contributions(
            env.core.clone(),
            self.core.process_lifecycle_available,
        )?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let ports = self.core.substrate_slot.ports().await;
        env = env.with_work_ports(ports.process.clone());
        let binding = Arc::new(BoundSession::new(
            resolved.store.clone(),
            &env,
            ports.process.clone(),
            resolved.catalog,
        ));
        env = binding.apply_owner(env);
        let recorded_parent_session_id =
            crate::session::recorded_parent_session_id(&binding.store()).await?;
        // Plugin configuration is creation config (FIG-4112, FIG-4379): the
        // session runs what it recorded, delivered unchanged; an open states
        // none.
        let runtime = LashRuntime::from_environment(
            &env,
            policy,
            state,
            Some(binding.store()),
            self.core.runtime_owner.clone(),
        )
        .await?;
        let handle = RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.core.live_replay_store),
        );
        let process_lifecycle_route = self.core.process_lifecycle_feed.register(&handle);
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
        .and_then(|meta| meta.relation.parent_session_id().cloned()))
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

/// A [`LashSession::park`] or [`LashSession::close`] that did not complete
/// (FIG-4202). It loses nothing: when the refusal left the session intact,
/// it hands the session back with its runtime and its pending usage, and the
/// host keeps using it or parks it again.
pub struct SessionParkRefused {
    session: Option<Box<LashSession>>,
    error: Box<EmbedError>,
}

impl SessionParkRefused {
    /// Why the park did not complete.
    pub fn error(&self) -> &EmbedError {
        &self.error
    }

    /// The shift that owns the session head, when the refusal is a busy one:
    /// the park's flush met a bound turn, an owed follow-on or an open
    /// session command. The same park lands once that owner's boundary
    /// passes.
    pub fn busy_owner(&self) -> Option<&lash_core::store::SessionHeadOwner> {
        match self.error.as_ref() {
            EmbedError::Session(SessionError::Store {
                source: lash_core::StoreError::SessionHeadOwned { owner, .. },
                ..
            }) => Some(owner),
            _ => None,
        }
    }

    /// The session, handed back intact, when the refusal left it so: a busy
    /// or failed flush. `None` when another handle still shares the runtime
    /// ([`EmbedError::SessionStillInUse`] leaves that handle in place).
    pub fn into_session(self) -> Option<LashSession> {
        self.session.map(|session| *session)
    }
}

impl std::fmt::Debug for SessionParkRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionParkRefused")
            .field("session_returned", &self.session.is_some())
            .field("error", &self.error)
            .finish()
    }
}

impl std::fmt::Display for SessionParkRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the session did not park: {}", self.error)
    }
}

impl std::error::Error for SessionParkRefused {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl From<SessionParkRefused> for EmbedError {
    fn from(refused: SessionParkRefused) -> Self {
        *refused.error
    }
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

    /// Durably close this session, then release its in-memory runtime.
    ///
    /// `close` is the honest teardown verb: a persistent session flushes its
    /// dirty state so the store reflects the final transcript, its in-memory
    /// plugin session is unregistered, and the live runtime is dropped.
    ///
    /// It requires exclusive ownership: any cloned [`LashSession`] handle or
    /// in-flight turn keeps a live reference to the same runtime, so `close`
    /// refuses with [`EmbedError::SessionStillInUse`] until those are
    /// dropped or finished. Cancel a running send first with
    /// [`SendHandle::cancel`](crate::SendHandle::cancel) if needed.
    ///
    /// A close whose flush does not land is recoverable and loses nothing
    /// (FIG-4202): the refusal hands the session back with its runtime and
    /// its pending usage, and [`SessionParkRefused::busy_owner`] names the
    /// shift that owns the session head when that is why. Close it again once
    /// that owner's boundary passes.
    ///
    /// To keep a handle for later resumption instead of discarding the session,
    /// use [`park`](Self::park).
    pub async fn close(self) -> std::result::Result<(), SessionParkRefused> {
        // Close does not resume: the parked handle is discarded.
        Box::pin(self.park()).await.map(drop)
    }

    /// Quiesce this session for later resumption, returning a lightweight
    /// [`ParkedSession`] handle.
    ///
    /// Parking flushes dirty state to the store, drops the live runtime and
    /// its plugin session, and hands back a cheap handle the host can cache
    /// and later rebuild with [`LashCore::resume`](crate::LashCore::resume).
    /// This is the quiesce/handoff lever for webserver embedders that hold
    /// many idle sessions: it bounds resident memory per session without
    /// deleting durable state.
    ///
    /// Contract:
    /// - **Exclusive ownership required.** A cloned [`LashSession`] or an
    ///   in-flight turn holds another reference to the runtime, and `park`
    ///   refuses with [`EmbedError::SessionStillInUse`] before it writes
    ///   anything. Parking is therefore an *idle-session* operation: finish
    ///   or cancel ([`SendHandle::cancel`](crate::SendHandle::cancel)) first.
    /// - **Busy is recoverable (FIG-4202).** The session's bound turn owns its
    ///   head. A dirty park while a run owns it (a turn running on another
    ///   runtime, an owed follow-on, a session command not yet applied)
    ///   writes nothing and hands the session back, with its runtime and its
    ///   pending usage, in [`SessionParkRefused`]. Park it again once that
    ///   owner's boundary passes. A clean park writes nothing and is never
    ///   busy.
    pub async fn park(self) -> std::result::Result<ParkedSession, SessionParkRefused> {
        let refuse = |session: Self, error: EmbedError| {
            Err(SessionParkRefused {
                session: Some(Box::new(session)),
                error: Box::new(error),
            })
        };
        if !self.is_sole_handle() {
            return refuse(self, EmbedError::SessionStillInUse);
        }
        let flushed = {
            let writer = self.runtime.writer();
            let mut runtime = writer.lock().await;
            let flushed = Box::pin(runtime.flush_for_park()).await;
            self.runtime.publish_from(&runtime).await;
            flushed
        };
        if let Err(error) = flushed {
            return refuse(self, error.into());
        }
        let binding = Arc::clone(&self.binding);
        let runtime = match self.into_owned_runtime().await {
            Ok(runtime) => runtime,
            Err(error) => {
                return Err(SessionParkRefused {
                    session: None,
                    error: Box::new(error),
                });
            }
        };
        // The flush landed and this handle owns the runtime alone: release
        // its in-memory plugin session and drop it.
        let parked = runtime
            .unregister_plugin_session()
            .map_err(EmbedError::from)
            .and_then(|()| runtime.parked_handle().map_err(EmbedError::from));
        match parked {
            Ok(inner) => Ok(ParkedSession { inner, binding }),
            Err(error) => Err(SessionParkRefused {
                session: None,
                error: Box::new(error),
            }),
        }
    }

    /// Whether this handle is the runtime's only live reference: no cloned
    /// session and no in-flight turn shares it.
    fn is_sole_handle(&self) -> bool {
        // `writer()` clones the shared `Arc<Mutex<LashRuntime>>`, so the
        // handle's own reference and this clone are the only two when no
        // other handle exists.
        Arc::strong_count(&self.runtime.writer()) == 2
    }

    /// Consume the session and take sole ownership of the underlying runtime.
    ///
    /// Fails with [`EmbedError::SessionStillInUse`] when another live handle
    /// (a cloned session or an in-flight turn) shares the runtime, so
    /// consuming operations never proceed on a still-shared runtime.
    async fn into_owned_runtime(self) -> Result<LashRuntime> {
        let LashSession { runtime, .. } = self;
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
                Err(EmbedError::SessionStillInUse)
            }
        }
    }

    pub fn session_id(&self) -> SessionId {
        SessionId::from(self.runtime.observe().session_id())
    }

    /// The scope uses the exact store-backed session identity owned by this
    /// facade handle's Session Binding.
    pub fn turn_scope(&self, turn_id: TurnId) -> lash_core::ExecutionScope {
        self.runtime.observe().turn_scope(turn_id)
    }

    /// Build the cancellation and terminal-observation address for a turn.
    pub fn turn_address(&self, turn_id: TurnId) -> lash_core::facade_support::TurnAddress {
        let observation = self.runtime.observe();
        lash_core::facade_support::TurnAddress::new(observation.session_id(), turn_id)
    }

    /// Returns a snapshot of the session's recorded policy: the config its
    /// commits write to the durable head. A run's per-run overrides (a
    /// send's model key, prompt or generation options) are that run's
    /// execution view and never show here, while it runs, after it settles
    /// or on a replay.
    pub fn policy_snapshot(&self) -> SessionPolicy {
        self.runtime.observe().read_view.policy().clone()
    }

    /// Returns an observable handle for session read models and replay.
    pub fn observe(&self) -> ObservableSession {
        ObservableSession {
            runtime: self.runtime.clone(),
            store: self.binding.store(),
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

    pub fn effect_host(&self) -> ActorContext {
        self.binding.effect_host()
    }

    /// Accept `input` durably and ask the engine to work the session: the
    /// one way a turn starts (FIG-3600).
    ///
    /// Awaiting the builder commits the acceptance and yields a
    /// [`SendHandle`](crate::SendHandle); `send(input).output().await` is the
    /// one-call form. The turn runs on the session's engine, not in the
    /// caller's future: dropping the handle stops nothing.
    ///
    /// A send whose response was lost is sent again under the same
    /// [`id`](crate::SendBuilder::id) with the same content: the retry
    /// accepts the input exactly once and answers its run.
    pub fn send(&self, input: TurnInput) -> crate::SendBuilder {
        crate::SendBuilder::new(crate::send::SendTarget::Live(self.clone()), input)
    }

    /// Accept `inputs` durably as one request under one shared spec, and ask
    /// the engine to work the session (FIG-3842).
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
    /// into another run, and never commits anything.
    ///
    /// An id lash holds no record of answers
    /// [`SendOutcome::NotAccepted`](crate::SendOutcome::NotAccepted), and a
    /// withdrawn input answers
    /// [`Withdrawn`](crate::SendOutcome::Withdrawn). A host that lost a
    /// send's response does not attach to learn whether it was accepted: it
    /// sends the same id with the same content again, which accepts the input
    /// exactly once and answers its run.
    pub fn attach_id(&self, id: TurnId) -> crate::SendHandle {
        crate::send::attach_id(crate::send::SendTarget::Live(self.clone()), id)
    }

    /// Re-await a logical run: after a park verb, or by the host id a send
    /// named.
    pub fn run(&self, run: crate::RunId) -> crate::RunHandle {
        crate::send::run(crate::send::SendTarget::Live(self.clone()), run.into())
    }

    /// Withdraw a queued input, or cooperatively cancel a running run
    /// (ADR 0039).
    pub fn cancel(&self, target: crate::CancelTarget) -> crate::CancelBuilder {
        crate::CancelBuilder::new(crate::send::SendTarget::Live(self.clone()), target)
    }

    /// The session's unfinished run, if a turn is under way: the name a
    /// host pins out of band while the turn runs
    /// ([`Target::Turn`](lash_core::Target::Turn)).
    pub async fn current_turn(&self) -> Result<Option<TurnId>> {
        self.durable().current_turn().await
    }

    /// Pin `target`: the revision it resolves to is retained through every
    /// collection until it is unpinned or the session is deleted.
    ///
    /// A pin names an input, a turn or a head revision, and may be written
    /// before its target exists, while it runs or after it ended. Writing it
    /// is idempotent, takes no execution authority and never reads or moves
    /// the head. Pins and retention decide only what a collection keeps.
    pub async fn pin(&self, target: lash_core::Target) -> Result<()> {
        self.durable().pin(target).await
    }

    /// Release the pin on `target`. The revision stays retained while
    /// another pin, the head or the retention policy holds it.
    pub async fn unpin(&self, target: lash_core::Target) -> Result<()> {
        self.durable().unpin(target).await
    }

    /// Every revision the session retains, oldest first: the points
    /// [`fork_at`](crate::LashCore::fork_at) accepts.
    pub async fn revisions(&self) -> Result<Vec<lash_core::RetainedRevision>> {
        self.durable().revisions().await
    }

    /// What the session keeps besides its head and its pins.
    pub async fn retention(&self) -> Result<lash_core::Retention> {
        self.durable().retention().await
    }

    /// Set what the session keeps besides its head and its pins
    /// ([`Retention`](lash_core::Retention)). It takes effect at the
    /// session's next commit and at the host's next collection.
    pub async fn set_retention(&self, retention: lash_core::Retention) -> Result<()> {
        self.durable().set_retention(retention).await
    }

    pub fn admin(&self) -> SessionAdmin {
        SessionAdmin {
            target: crate::send::SendTarget::Live(self.clone()),
            runtime: self.runtime.clone(),
            process_work: Arc::clone(self.binding.process().port()),
        }
    }

    /// Refresh the session graph from any background process that signalled it
    /// changed. This is the honest name for the former
    /// `processes().await_all()` misnomer (ADR 0014 grill): a session-graph
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
    /// let id = lash::TurnId::parse("draft-1")?;
    /// session.durable().send(input).id(id).await?;
    /// ```
    pub fn durable(&self) -> DurableSession {
        DurableSession::from_binding(
            SessionId::from(self.runtime.observe().session_id()),
            self.binding.store(),
            self.binding.effect_host(),
            Arc::clone(&self.runtime.live_replay_store),
            self.binding.catalog(),
            self.binding.llm_profiles(),
            self.binding.trace_scopes(),
        )
        .with_transcript_options(self.read_view().transcript_options().clone())
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

    /// Install turn-phase instrumentation for a test or performance harness.
    /// Phase names follow the runtime implementation.
    #[cfg(any(test, feature = "testing"))]
    pub async fn set_turn_phase_probe(
        &self,
        probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe>,
    ) {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let changed = runtime.set_turn_phase_probe_if_changed(probe);
        if changed {
            self.runtime.publish_resident_from(&runtime).await;
        }
    }
}

#[derive(Clone)]
/// Exposes read models and replayable observations for an active session.
///
/// The session feed is durable-anchored: [`snapshot`](Self::snapshot) is the
/// durable head with a cursor bound to its revision, and
/// [`subscribe_and_recover`](Self::subscribe_and_recover) tails every
/// durable commit past a cursor, whichever process made it. The raw cursor
/// reads ([`resume_from_cursor`](Self::resume_from_cursor),
/// [`subscribe_from_cursor`](Self::subscribe_from_cursor)) judge their cursor
/// against the same durable head. The synchronous reads
/// ([`read_view`](Self::read_view), [`tool_state`](Self::tool_state)) answer
/// from this process's resident runtime, which trails a commit another
/// process made until the resident adopts the durable head.
pub struct ObservableSession {
    pub(crate) runtime: RuntimeHandle,
    store: lash_core::store::SessionStore,
}

impl ObservableSession {
    fn resident(&self) -> Arc<RuntimeObservation> {
        self.runtime.observe()
    }

    fn feed_source(&self) -> crate::observation_feed::FeedSource {
        crate::observation_feed::FeedSource::new(self.runtime.clone(), self.store.clone())
    }

    /// The session's durable head and the cursor bound to its revision:
    /// the snapshot a feed from [`subscribe_and_recover`](Self::subscribe_and_recover)
    /// continues.
    ///
    /// It reads the store's head, so a commit any process made is in it.
    /// The resident runtime adopts the head first unless a run holds it.
    pub async fn snapshot(&self) -> Result<SessionObservation> {
        self.feed_source().snapshot().await
    }

    /// [`snapshot`](Self::snapshot) as remote DTOs.
    pub async fn remote_snapshot(&self) -> Result<RemoteSessionObservation> {
        Ok(RemoteSessionObservation::from_core(self.snapshot().await?))
    }

    /// The live replay after `cursor`, or a gap whose replacement is the
    /// durable head when the cursor is past the head, or behind it without
    /// a replayed `Committed` bridging to it.
    pub async fn resume_from_cursor(&self, cursor: &SessionCursor) -> Result<SessionResume> {
        self.feed_source().resume(cursor).await
    }

    /// A live replay subscription after `cursor`, judged against the durable
    /// head as [`resume_from_cursor`](Self::resume_from_cursor) judges a
    /// replay.
    pub async fn subscribe_from_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<SessionObservationSubscription> {
        self.feed_source().subscribe(cursor).await
    }

    pub async fn subscribe_from_remote_cursor(
        &self,
        cursor: &RemoteSessionCursor,
    ) -> Result<RemoteSessionObservationSubscription> {
        cursor.validate()?;
        let cursor = lash_core::SessionCursor::try_from(cursor.clone())?;
        match self.subscribe_from_cursor(&cursor).await? {
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

    /// The session feed from `cursor`: every durable commit past its
    /// revision, in order and once, whichever process made it, with the
    /// provisional events this process's live replay holds.
    ///
    /// The returned stream yields [`SessionObservationStreamItem::Gap`] when
    /// the cursor cannot be continued: it fell outside the bounded replay
    /// window, another replay store minted it, or it is past the durable
    /// head. Callers should replace their UI/projection from the included
    /// observation, which is the durable head, persist `gap.latest_cursor`,
    /// and keep polling the same stream; it continues from that cursor.
    pub fn subscribe_and_recover(&self, cursor: SessionCursor) -> SessionObservationStream {
        SessionObservationStream::new(self.feed_source(), cursor)
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
        SessionId::from(self.resident().session_id())
    }

    /// Returns a snapshot of the session's recorded policy, as
    /// [`LashSession::policy_snapshot`] does.
    pub fn policy_snapshot(&self) -> SessionPolicy {
        self.resident().read_view.policy().clone()
    }

    pub fn read_view(&self) -> SessionReadView {
        self.resident().read_view.clone()
    }

    pub fn tool_state(&self) -> Option<ToolState> {
        self.resident().tool_state.clone()
    }

    pub fn active_tool_manifests(&self) -> Vec<ToolManifest> {
        self.resident()
            .tool_state
            .as_ref()
            .map(ToolState::tool_manifests)
            .unwrap_or_default()
    }

    /// Lists process handles.
    pub async fn list_process_handles(&self) -> Vec<ProcessHandleView> {
        self.resident().list_process_handles().await
    }

    /// Lists all process handles.
    pub async fn list_all_process_handles(&self) -> Vec<ProcessHandleView> {
        self.resident().list_all_process_handles().await
    }

    pub fn process_scope(&self) -> SessionScope {
        self.resident().process_scope()
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
            store
                .publish(
                    &SessionId::from("session-seq-test"),
                    lash_core::SessionRevision::new(revision),
                    vec![lash_core::LiveReplayEventDraft::new(
                        Some("turn-1"),
                        lash_core::SessionObservationEventPayload::TurnActivity(activity),
                    )],
                )
                .await
                .expect("publish event");
        }

        let subscription = store
            .subscribe_after_cursor(&cursor)
            .await
            .expect("subscribe");
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
