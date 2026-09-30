use super::*;
use crate::plugin::PluginSessionRequest;

pub(in crate::runtime) fn initial_park_preview(
    state: &crate::RuntimeSessionState,
    commit_budget: crate::CommitBudget,
    fleet_format: crate::FleetFormat,
) -> Result<crate::store::RuntimeCommit, crate::StoreError> {
    let operation =
        super::state::boundary_operation(&state.session_id, "initial-park-preview", "preview");
    let mut graph = state.pending_graph_commit();
    graph.derive_node_ids(&state.session_id, &operation)?;
    crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
        state,
        graph,
        operation,
        commit_budget,
        fleet_format,
    )
}

pub(in crate::runtime) fn initial_park_operation(
    commit: &crate::store::RuntimeCommit,
) -> Result<crate::OperationId, crate::StoreError> {
    let content_hash = commit.turn_commit_hash()?;
    Ok(super::state::boundary_operation(
        &commit.session_id,
        &format!("content:{content_hash}"),
        "initial-park",
    ))
}

async fn bind_state_to_store(
    store: &crate::store::SessionStore,
    state: &RuntimeSessionState,
) -> Result<(), SessionError> {
    if *store.session_id() != state.session_id {
        return Err(SessionError::Store {
            context: format!("failed to bind session `{}` to its store", state.session_id),
            source: crate::StoreError::ForeignSessionRequest {
                view_session_id: store.session_id().clone(),
                request_session_id: state.session_id.clone(),
            },
        });
    }
    let lookup = store
        .store()
        .lookup_session(&state.session_id)
        .await
        .map_err(|source| SessionError::Store {
            context: format!("failed to bind session `{}` to its store", state.session_id),
            source,
        })?;
    let source = match lookup {
        crate::store::SessionLookup::Live(_) => return Ok(()),
        crate::store::SessionLookup::Absent => crate::StoreError::SessionNotFound {
            session_id: state.session_id.clone(),
        },
        crate::store::SessionLookup::Deleted => crate::StoreError::SessionDeleted {
            session_id: state.session_id.clone(),
        },
    };
    Err(SessionError::Store {
        context: format!("failed to bind session `{}` to its store", state.session_id),
        source,
    })
}

async fn bind_state_to_store_with_trace(
    host: &crate::RuntimeHostConfig,
    store: &crate::store::SessionStore,
    state: &RuntimeSessionState,
) -> Result<(), SessionError> {
    let result = bind_state_to_store(store, state).await;
    if let Err(SessionError::Store { source, .. }) = &result {
        crate::trace::emit_store_error(
            &host.tracing.trace_sink,
            &host.tracing.trace_context,
            lash_trace::TraceContext::default().for_session(state.session_id.clone()),
            "session_store_bind",
            source,
            host.clock.as_ref(),
        );
    }
    result
}

pub(in crate::runtime) struct RuntimePersistenceBindings {
    runtime_store: Option<crate::store::SessionStore>,
    attachment_referrers_store: Option<Arc<dyn crate::store::RuntimeStore>>,
}

pub(in crate::runtime) struct RuntimeSessionAssembly {
    state: RuntimeSessionState,
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    runtime_lease_executor_id: String,
}

impl RuntimeSessionAssembly {
    pub(in crate::runtime) fn new(
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self {
            state,
            runtime_lease_owner,
            runtime_lease_executor_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(in crate::runtime) fn resumed(
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
        runtime_lease_executor_id: String,
    ) -> Self {
        Self {
            state,
            runtime_lease_owner,
            runtime_lease_executor_id,
        }
    }
}

impl RuntimePersistenceBindings {
    pub(in crate::runtime) fn new(runtime_store: Option<crate::store::SessionStore>) -> Self {
        Self {
            attachment_referrers_store: runtime_store
                .as_ref()
                .map(|store| Arc::clone(store.store())),
            runtime_store,
        }
    }

    pub(in crate::runtime) fn with_attachment_referrers_store(
        mut self,
        store: Arc<dyn crate::store::RuntimeStore>,
    ) -> Self {
        self.attachment_referrers_store = Some(store);
        self
    }
}

impl LashRuntime {
    pub fn unregister_plugin_session(&self) -> Result<(), crate::PluginError> {
        if let Some(session) = self.session.as_ref() {
            session
                .plugins()
                .host()
                .unregister_session(&self.state.session_id)?;
        }
        Ok(())
    }

    pub(super) async fn from_host_state(
        policy: SessionPolicy,
        host: RuntimeHost,
        services: RuntimeServices,
        mut state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
        runtime_lease_executor_id: String,
    ) -> Result<Self, SessionError> {
        services
            .plugins
            .require_runtime_owner()
            .map_err(SessionError::Plugin)?;
        // Defaulted state (e.g. `RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))` used
        // by fresh-session constructors) carries an empty policy.
        // Fill it in from the caller's policy so tests and hosts that
        // pass a real policy alongside default state don't trip the explicit
        // model-spec guard below.
        let state_policy_was_unconfigured = state.policy.recorded_provider_id().is_empty()
            && state.policy.model.id.trim().is_empty();
        if state_policy_was_unconfigured {
            state.policy = policy.clone();
        }
        if state.checkpoint_ref.is_none() && state.head_revision == 0 {
            state.authority.tool_access = services.plugins.tool_access();
            state.authority.subagent = services.plugins.subagent_context();
        }
        state.ensure_agent_frame_initialized();
        if state.effective_policy().model.id.trim().is_empty() {
            return Err(SessionError::Protocol(
                "session policy missing model spec; hosts must supply explicit model metadata"
                    .to_string(),
            ));
        }
        let mut host = host;

        // When a persistent backend is wired in, wrap the attachment
        // store so every `put` records a write-ahead intent row first.
        // Crashes between put and the next turn commit then surface as
        // uncommitted manifest rows that GC can reconcile. Ephemeral
        // (no-store) runtimes use the inner store directly — there's
        // nothing to reconcile against.
        if let Some(store) = services.attachment_referrers_store.clone() {
            let manifest: Arc<dyn crate::AttachmentReferrers> =
                Arc::new(crate::attachments::PersistenceReferrersAdapter(store));
            // Rebind a fresh facade over the flat backend. Attachment ownership
            // is recorded durably on each intent; no live facade state crosses
            // rebuilds or child-session initialisation.
            let previous_attachment_store = Arc::clone(&host.core.durability.attachment_store);
            let backend = Arc::clone(previous_attachment_store.backend());
            let scoped = Arc::new(
                crate::RuntimeAttachmentStore::new_with_clock(
                    backend,
                    manifest,
                    crate::RuntimeOwner::Session(state.session_id.clone()),
                    Arc::clone(&host.core.clock),
                )
                .with_max_attachment_bytes(previous_attachment_store.max_attachment_bytes())
                .with_read_policy(previous_attachment_store.read_policy())
                .with_upload_expiry_ms(previous_attachment_store.upload_expiry_ms())
                .with_output_retention(previous_attachment_store.output_retention()),
            );
            host.core.durability.attachment_store = scoped;
        }
        let services = services
            .with_attachment_store(Arc::clone(&host.core.durability.attachment_store))
            .with_process_env_store(Arc::clone(&host.core.durability.process_env_store))
            .with_clock(Arc::clone(&host.core.clock))
            .with_tool_children(host.core.control.tool_children.clone());
        if let Some(snapshot) = state.plugin_state() {
            services
                .plugins
                .require_hydrated_state(snapshot)
                .map_err(SessionError::Plugin)?;
        } else if let Some(reference) = state.plugin_state_ref()
            && !services.plugins.matches_state_ref(reference)
        {
            return Err(SessionError::Protocol(
                "plugin-state reference must be hydrated before runtime construction".into(),
            ));
        }
        let mut session = Session::new(services.clone(), &state.session_id).await?;
        let mut tool_restore_report = None;
        // FIG-3353: an open that will not run a turn declares
        // `PreservePersisted` and leaves the durable tool surface alone — no
        // reconcile, no catalog rebuild, no generation bump, and no report or
        // lost-tools warning for what is an intentional no-source open.
        let preserve_persisted_tools = host.core.control.tool_surface_open_mode
            == crate::ToolSurfaceOpenMode::PreservePersisted;
        // The marker rides the state (never the durable encoding) so every
        // later stamp — park, appends, turn drafts — keeps the loaded snapshot
        // instead of restamping the unreconciled registry.
        state.preserve_tool_state_snapshot = preserve_persisted_tools;
        if !preserve_persisted_tools && let Some(tool_state) = state.tool_state_snapshot().cloned()
        {
            // Cold rebuild reconciles the persisted catalog over live tools,
            // adopting its generation when the surface is unchanged.
            // `apply_state` (a delta-apply that
            // requires `snapshot.generation == base` and bumps) would reject a
            // session whose surface reached generation ≥ 2 onto a fresh base-1
            // registry — the worker-rebuild / restart divergence. `restore_state`
            // is not generation-fenced against the fresh registry, so any
            // persisted generation rebuilds. A changed live surface bumps once
            // to make the next commit capture it.
            //
            // Refusing here is what the Require contract promises: the
            // protocol restore, the `SessionRestored` event and every durable
            // write below have not run, and the admitted load already released
            // its Session Execution Lease.
            let registry = session.plugins().tool_registry();
            tool_restore_report = Some(crate::runtime::tool_restore::install_persisted_tool_state(
                registry.as_ref(),
                tool_state,
                crate::runtime::tool_restore::ToolRestoreContext::for_open(
                    &state.session_id,
                    host.core.control.tool_source_policy,
                    &host.core.tracing,
                    host.core.clock.as_ref(),
                ),
            )?);
        }
        if !preserve_persisted_tools {
            session.refresh_tool_catalog().await?;
        }
        let protocol_session = Arc::clone(session.plugins().protocol_session());
        let session_id = state.session_id.clone();
        protocol_session
            .restore_session(
                crate::plugin::ProtocolSessionContext::new(&mut session, &session_id),
                crate::plugin::ProtocolSessionRestoreView::new(&state),
            )
            .await?;
        if session.history_store().is_some() {
            state.discard_runtime_snapshots();
        } else {
            state.discard_runtime_snapshots_retaining_accepted_execution();
        }
        session
            .plugins()
            .emit_runtime_event(crate::PluginLifecycleEvent::SessionRestored(
                crate::SessionReadView::from_persisted_state(&state),
            ))
            .await
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        Ok(Self {
            session: Some(session),
            host,
            services,
            state,
            runtime_lease_owner,
            runtime_lease_executor_id,
            engine_retries_root: false,
            admitted_turn_index: None,
            drive_root: None,
            process_sync_needed: Arc::new(AtomicBool::new(false)),
            turn_phase_probe: None,
            resident_session: ResidentSessionContinuity::fresh(),
            tool_restore_report,
        })
    }

    pub async fn from_embedded_state(
        policy: SessionPolicy,
        host: EmbeddedRuntimeHost,
        services: RuntimeServices,
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        Self::from_host_state(
            policy,
            host.into(),
            services,
            state,
            runtime_lease_owner,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
    }

    pub async fn from_background_state(
        policy: SessionPolicy,
        host: ProcessRuntimeHost,
        services: RuntimeServices,
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        Self::from_host_state(
            policy,
            host.into(),
            services,
            state,
            runtime_lease_owner,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
    }

    pub async fn from_persistent_embedded_state(
        policy: SessionPolicy,
        host: EmbeddedRuntimeHost,
        services: PersistentRuntimeServices,
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        bind_state_to_store_with_trace(&host.core, &services.store(), &state).await?;
        Self::from_host_state(
            policy,
            host.into(),
            services.into_runtime_services(),
            state,
            runtime_lease_owner,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
    }

    pub async fn from_persistent_background_state(
        policy: SessionPolicy,
        host: ProcessRuntimeHost,
        services: PersistentRuntimeServices,
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        bind_state_to_store_with_trace(&host.embedded().core, &services.store(), &state).await?;
        Self::from_host_state(
            policy,
            host.into(),
            services.into_runtime_services(),
            state,
            runtime_lease_owner,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
    }

    /// Assemble a runtime from already-resolved parts: the single place that maps
    /// `(store, work)` to the right host/services constructor.
    ///
    /// Every construction path — the live open (`from_environment`), the worker
    /// rebuild (`EmbeddedRuntimeBuilder::build`), and child-session
    /// materialization — routes through here so the store/registry wiring cannot
    /// drift between them. Persistent paths bind the supplied state to the
    /// store's durable session id before constructing the runtime.
    pub(in crate::runtime) async fn assemble_runtime(
        policy: SessionPolicy,
        embedded_host: EmbeddedRuntimeHost,
        plugin_session: Arc<crate::PluginSession>,
        persistence: RuntimePersistenceBindings,
        work: super::host::RuntimeWork,
        session: RuntimeSessionAssembly,
    ) -> Result<Self, SessionError> {
        let RuntimeSessionAssembly {
            state,
            runtime_lease_owner,
            runtime_lease_executor_id,
        } = session;
        let RuntimePersistenceBindings {
            runtime_store: store,
            attachment_referrers_store,
        } = persistence;
        if let Some(store) = store.as_ref()
            && let Err(error) =
                bind_state_to_store_with_trace(&embedded_host.core, store, &state).await
        {
            return Err(error);
        }
        let host = super::host::RuntimeHost::from_embedded_with_work(embedded_host, work);
        // Both arms take their attachment and process-exec-env ports from the
        // host, which takes them from its backend (ADR 0102): a runtime
        // with no session store — the worker's reconstruction runtime among
        // them — writes attachments through the backend's attachment port
        // with no manifest, never through an in-memory stand-in.
        let attachment_store = Arc::clone(&host.core.durability.attachment_store);
        let process_env_store = Arc::clone(&host.core.durability.process_env_store);
        let runtime = match store {
            Some(store) => {
                let mut services = PersistentRuntimeServices::new(
                    plugin_session,
                    store,
                    attachment_store,
                    process_env_store,
                );
                if let Some(manifest_store) = attachment_referrers_store {
                    services = services.with_attachment_referrers_store(manifest_store);
                }
                Self::from_host_state(
                    policy,
                    host,
                    services.into_runtime_services(),
                    state,
                    runtime_lease_owner.clone(),
                    runtime_lease_executor_id.clone(),
                )
                .await?
            }
            None => {
                // A storeless runtime persists no session state, but a
                // reconstruction runtime still records its attachment intents
                // on the catalog store its host names, so their owner stays
                // durable.
                let mut services =
                    RuntimeServices::new(plugin_session, attachment_store, process_env_store);
                services.attachment_referrers_store = attachment_referrers_store;
                Self::from_host_state(
                    policy,
                    host,
                    services,
                    state,
                    runtime_lease_owner.clone(),
                    runtime_lease_executor_id.clone(),
                )
                .await?
            }
        };
        Ok(runtime)
    }

    /// Embedder-preferred constructor: build a `LashRuntime` from a
    /// shared `RuntimeEnvironment`.
    ///
    /// Everything expensive (plugin factories, HTTP client pool, prompt
    /// template, path resolver) lives on the environment and is
    /// reused across every runtime the embedder builds. This call is
    /// O(plugin-session-registration + state-hydration), not
    /// O(full-infrastructure-init).
    ///
    /// * `env` — the shared environment. `env.plugin_host` must be set.
    /// * `policy` — per-session policy (model, provider, autonomy, turn limits).
    /// * `state` — persisted session state (empty for a fresh session).
    /// * `store` — per-session store. `None` builds an embedded runtime
    ///   with no persistence; `Some` builds a persistent
    ///   background-capable runtime.
    pub async fn from_environment(
        env: &RuntimeEnvironment,
        policy: SessionPolicy,
        state: RuntimeSessionState,
        store: Option<crate::store::SessionStore>,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        Self::from_environment_for_executor(
            env,
            policy,
            state,
            store,
            runtime_lease_owner,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
    }

    async fn from_environment_for_executor(
        env: &RuntimeEnvironment,
        policy: SessionPolicy,
        state: RuntimeSessionState,
        store: Option<crate::store::SessionStore>,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
        runtime_lease_executor_id: String,
    ) -> Result<Self, SessionError> {
        let plugin_host = env.plugin_host.as_ref().ok_or_else(|| {
            SessionError::Protocol(
                "RuntimeEnvironment.plugin_host is required for from_environment".to_string(),
            )
        })?;
        let parent_session_id = state
            .authority
            .subagent
            .as_ref()
            .map(|subagent| subagent.parent_session_id.clone());
        // The session's recorded plugin configuration, as recorded: every
        // open delivers it unchanged (FIG-4379).
        let authority = crate::plugin::SessionAuthorityContext {
            tool_access: state.authority.tool_access.clone(),
            subagent: state.authority.subagent.clone(),
            plugin_config: state.admitted_plugin_config(),
        };
        let plugin_session = match state.plugin_state() {
            Some(snapshot) => plugin_host.build_session(PluginSessionRequest {
                parent_session_id: parent_session_id.clone(),
                ..PluginSessionRequest::rematerialization(
                    state.session_id.as_str(),
                    snapshot,
                    authority,
                )
            }),
            None => plugin_host.build_session(PluginSessionRequest {
                parent_session_id,
                ..PluginSessionRequest::creation(state.session_id.as_str(), authority)
            }),
        }
        .map_err(SessionError::Plugin)?;
        let embedded = EmbeddedRuntimeHost::new(env.core.clone());
        // Boxed: assembly holds a whole host config and session state across
        // its awaits, which is cold-path weight every caller would carry.
        Box::pin(Self::assemble_runtime(
            policy,
            embedded,
            plugin_session,
            RuntimePersistenceBindings::new(store),
            env.work.clone(),
            RuntimeSessionAssembly::resumed(state, runtime_lease_owner, runtime_lease_executor_id),
        ))
        .await
    }

    /// Persist any dirty state and drop the runtime, returning a lightweight
    /// handle the embedder can cache and resume later via
    /// [`LashRuntime::resume`]. This is the webserver-embedder parking
    /// primitive: the handle holds only the session id, policy, and store
    /// reference — no graph nodes, no plugin session, no HTTP client.
    ///
    /// A park that cannot complete hands the runtime back
    /// ([`ParkRefused`]), with its resident state and its pending usage as
    /// they were: nothing it held is lost. The bound turn owns the session
    /// head (FIG-4202), so a dirty park while a drive owns the head (a bound
    /// root, an owed follow-on or an open session command) is refused busy
    /// in the flush's own transaction, and the host parks again once that
    /// owner's boundary passes. A clean park writes nothing and is never
    /// busy.
    pub async fn park(mut self) -> Result<ParkedSession, ParkRefused> {
        let store = match self.park_store() {
            Ok(store) => store,
            Err(error) => {
                return Err(ParkRefused {
                    runtime: Box::new(self),
                    error: Box::new(error),
                });
            }
        };
        if let Err(error) = Box::pin(self.flush_for_park()).await {
            return Err(ParkRefused {
                runtime: Box::new(self),
                error: Box::new(error),
            });
        }
        Ok(self.into_parked(store))
    }

    /// The handle a park returns, once [`Self::flush_for_park`] landed: the
    /// session id, policy and store reference, and the lease facts a resume
    /// checks.
    pub fn parked_handle(self) -> Result<ParkedSession, SessionError> {
        let store = self.park_store()?;
        Ok(self.into_parked(store))
    }

    fn into_parked(self, store: crate::store::SessionStore) -> ParkedSession {
        ParkedSession {
            session_id: self.state.session_id.clone(),
            store,
            policy: self.state.effective_policy().clone(),
            runtime_lease_owner: self.runtime_lease_owner,
            runtime_lease_executor_id: self.runtime_lease_executor_id,
        }
    }

    fn park_store(&self) -> Result<crate::store::SessionStore, SessionError> {
        self.services.store.clone().ok_or_else(|| {
            SessionError::Protocol(
                "park() requires a persistent runtime (store is not set)".to_string(),
            )
        })
    }

    /// Persist a park's dirty state: the non-consuming half of
    /// [`Self::park`] (FIG-4202).
    ///
    /// A flush that does not land leaves the runtime as it was, its resident
    /// state and its pending usage included: the store refuses it typed
    /// ([`StoreError::SessionHeadOwned`](crate::StoreError::SessionHeadOwned))
    /// while a drive owns the session head, and the same flush lands once
    /// that owner's boundary passes. A head that moved since this runtime
    /// last read it is adopted first, so the flush never commits an old
    /// whole-session snapshot over it; the pending usage, held apart from the
    /// head, rides the flush whatever moved. A clean runtime writes nothing.
    pub async fn flush_for_park(&mut self) -> Result<(), SessionError> {
        self.park_store()?;
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        self.adopt_committed_head()
            .await
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        let store = self.park_store()?;
        self.stamp_live_plugin_state();
        // Under the settled-state contract every durable mutation commits at
        // its own boundary (turn final commit, session commands), so a runtime
        // between boundaries already equals its last commit. Flushing is only
        // needed when the state has never been persisted, has accepted plugin
        // writes, or has pending graph nodes; an unconditional commit here
        // would bump the head revision on every park/close, disturbing
        // host-side head-CAS expectations for what is durably a no-op.
        if self.state.checkpoint_ref.is_some()
            && !self.state.plugin_state_is_dirty()
            && self.state.pending_graph_commit().nodes().is_empty()
        {
            return Ok(());
        }
        let proposed = initial_park_preview(
            &self.state,
            self.host.core.durability.commit_budget,
            self.fleet_format(),
        )
        .map_err(|err| SessionError::Protocol(err.to_string()))?;
        let operation = initial_park_operation(&proposed)
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        // The commit is built over a copy: a flush that does not land leaves
        // the resident state as it was.
        let mut flushed = self.state.clone();
        let fleet_format = self.fleet_format();
        let (commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut flushed,
                operation,
                self.host.core.durability.commit_budget,
                fleet_format,
            )
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        // A write outside every drive: the store refuses it while a drive
        // owns the head (FIG-4202).
        let result = store
            .commit_runtime_state_verified(commit)
            .await
            .map_err(|source| session_commit_error("failed to persist runtime state", source))?;
        flushed.apply_persisted_commit_result(result);
        flushed.mark_node_ids_persisted(persisted_node_ids);
        self.state = flushed;
        Ok(())
    }

    /// Resume a previously parked session against a shared environment.
    pub async fn resume(
        parked: ParkedSession,
        env: &RuntimeEnvironment,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        if !parked
            .runtime_lease_owner
            .same_incarnation(&runtime_lease_owner)
        {
            return Err(SessionError::Protocol(
                "parked runtime owner does not match the resuming host owner".to_string(),
            ));
        }
        let loaded = crate::store::load_session_window_state(
            &parked.store,
            crate::store::WindowSelector::Current,
        )
        .await
        .map_err(|err| session_commit_error("failed to load runtime state", err))?
        .map(|loaded| loaded.state);
        let state = loaded.unwrap_or_else(|| RuntimeSessionState {
            session_id: parked.session_id.clone(),
            policy: parked.policy.clone(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        });
        Self::from_environment_for_executor(
            env,
            parked.policy,
            state,
            Some(parked.store),
            runtime_lease_owner,
            parked.runtime_lease_executor_id,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{initial_park_operation, initial_park_preview};
    use crate::SessionError;
    use crate::SessionId;

    fn user_message(id: &str, content: &str) -> crate::Message {
        crate::Message {
            id: id.to_string(),
            role: crate::MessageRole::User,
            parts: crate::shared_parts(vec![crate::Part::text(
                format!("{id}.p0"),
                content.to_string(),
                None,
            )]),
            origin: None,
        }
    }

    #[test]
    fn initial_park_identity_is_stable_for_replay_and_distinguishes_content() {
        let mut state = crate::RuntimeSessionState {
            session_id: SessionId::from("park-identity"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        state.append_active_conversation_messages(&[user_message(
            "pending-user-message",
            "persist me before parking",
        )]);
        let budget = crate::CommitBudget::bounded(1024 * 1024, 512);
        let first = initial_park_preview(&state, budget, crate::FleetFormat::current())
            .expect("first park preview");

        let mut retry_state = state.clone();
        retry_state.head_revision = 41;
        let retry = initial_park_preview(&retry_state, budget, crate::FleetFormat::current())
            .expect("retry park preview");

        let mut changed_state = retry_state.clone();
        changed_state.turn_index = 1;
        let changed = initial_park_preview(&changed_state, budget, crate::FleetFormat::current())
            .expect("changed park preview");

        let first = initial_park_operation(&first).expect("first park identity");
        let retry = initial_park_operation(&retry).expect("retry park identity");
        let changed = initial_park_operation(&changed).expect("changed park identity");

        assert_eq!(
            first, retry,
            "optimistic head movement alone must not change replay identity"
        );
        assert_ne!(
            first, changed,
            "different persisted content must not reuse one park receipt"
        );
    }

    #[tokio::test]
    async fn park_commit_preserves_a_concurrent_session_deletion_refusal() {
        use crate::runtime::tests::helpers::{
            EmptyTools, plugin_session_with_tools, standard_test_policy, test_host_config,
        };
        use std::sync::Arc;

        let session_id = "deleted-during-park-commit";
        let policy = standard_test_policy();
        let request = crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(session_id.to_string()),
            relation: crate::SessionRelation::Root,
            config: policy.clone().into(),
            head: crate::SessionCreationHead::CommittedByCreator,
        };
        let backend = crate::testing::sqlite_memory_store_backend().await;
        let factory = backend.session_store_factory();
        let store = crate::testing::runtime_helpers::create_session_store(&factory, &request)
            .await
            .expect("create session store before parking");
        let runtime_host = test_host_config(&backend);
        let runtime_services = crate::PersistentRuntimeServices::new(
            plugin_session_with_tools(&SessionId::from(session_id), Arc::new(EmptyTools)),
            store,
            std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
            std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
        );
        let runtime = crate::LashRuntime::from_persistent_embedded_state(
            policy.clone(),
            runtime_host,
            runtime_services,
            crate::RuntimeSessionState {
                session_id: SessionId::from(session_id.to_string()),
                policy,
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                ))
            },
            crate::testing::runtime_lease_owner(),
        )
        .await
        .expect("build runtime before concurrent deletion");
        factory
            .delete_session(&SessionId::from(session_id))
            .await
            .expect("delete session before park commit");

        let error = match Box::pin(runtime.park()).await {
            Ok(_) => panic!("park commit must refuse the retired session"),
            Err(refused) => *refused.error,
        };
        let canonical = crate::StoreError::SessionDeleted {
            session_id: SessionId::from(session_id.to_string()),
        }
        .to_string();

        assert_eq!(
            error.to_string(),
            format!("failed to persist runtime state: {canonical}")
        );
        assert!(matches!(
            error,
            SessionError::Store {
                source: crate::StoreError::SessionDeleted {
                    session_id: deleted_session_id,
                },
                ..
            } if deleted_session_id == session_id
        ));
    }

    #[tokio::test]
    async fn park_commit_keeps_a_transient_backend_failure_as_protocol() {
        use crate::runtime::tests::helpers::{
            EmptyTools, plugin_session_with_tools, standard_test_policy, test_host_config,
        };
        use std::sync::Arc;

        let session_id = "transient-park-commit-failure";
        let policy = standard_test_policy();
        let backend = crate::testing::sqlite_memory_store_backend().await;
        let factory = backend.session_store_factory();
        crate::testing::runtime_helpers::create_session_store(
            &factory,
            &crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(session_id.to_string()),
                relation: crate::SessionRelation::Root,
                config: policy.clone().into(),
                head: crate::SessionCreationHead::CommittedByCreator,
            },
        )
        .await
        .expect("create session store before parking");
        let store = Arc::new(crate::testing::runtime_helpers::RecordingStore::over(
            factory,
        ));
        let runtime_host = test_host_config(&backend);
        let runtime_services = crate::PersistentRuntimeServices::new(
            plugin_session_with_tools(&SessionId::from(session_id), Arc::new(EmptyTools)),
            crate::store::SessionStore::new(
                Arc::clone(&store) as Arc<dyn crate::store::RuntimeStore>,
                SessionId::from(session_id),
            )
            .expect("valid test session id"),
            std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
            std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
        );
        let runtime = crate::LashRuntime::from_persistent_embedded_state(
            policy.clone(),
            runtime_host,
            runtime_services,
            crate::RuntimeSessionState {
                session_id: SessionId::from(session_id.to_string()),
                policy,
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                ))
            },
            crate::testing::runtime_lease_owner(),
        )
        .await
        .expect("build runtime before injected backend failure");
        store.fail_next_runtime_commit(crate::StoreError::Backend(
            "temporary park backend outage".to_string(),
        ));

        let error = match Box::pin(runtime.park()).await {
            Ok(_) => panic!("park commit must surface the injected backend failure"),
            Err(refused) => *refused.error,
        };

        assert!(matches!(
            &error,
            SessionError::Protocol(message)
                if message
                    == "failed to persist runtime state: store backend error: temporary park backend outage"
        ));
        assert_eq!(
            error.to_string(),
            "protocol error: failed to persist runtime state: store backend error: temporary park backend outage"
        );
    }
}
