use super::*;

fn initial_park_preview(
    state: &crate::RuntimeSessionState,
    commit_budget: crate::CommitBudget,
) -> Result<crate::store::RuntimeCommit, crate::StoreError> {
    let operation =
        super::state::boundary_operation(&state.session_id, "initial-park-preview", "preview");
    let mut graph = state.pending_graph_commit();
    graph.derive_node_ids(&state.session_id, &operation)?;
    crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
        state,
        graph,
        &[],
        operation,
        commit_budget,
    )
}

fn initial_park_operation(
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
    store: &(dyn crate::store::RuntimePersistence + '_),
    state: &mut RuntimeSessionState,
    relation: crate::SessionRelation,
) -> Result<(), SessionError> {
    let binding = crate::SessionBinding {
        session_id: state.session_id.clone(),
        relation,
    };
    store
        .admit_and_bind_session(&binding)
        .await
        .map_err(|source| SessionError::Store {
            context: format!("failed to bind session `{}` to its store", state.session_id),
            source,
        })?;
    let meta = store
        .load_session_meta()
        .await
        .map_err(|source| SessionError::Store {
            context: format!(
                "failed to verify session `{}` store binding",
                state.session_id
            ),
            source,
        })?
        .ok_or_else(|| SessionError::Store {
            context: format!(
                "failed to verify session `{}` store binding",
                state.session_id
            ),
            source: crate::StoreError::SessionBindingNotMaterialized {
                session_id: state.session_id.clone(),
            },
        })?;
    if meta.session_id != state.session_id {
        return Err(SessionError::Store {
            context: format!(
                "failed to verify session `{}` store binding",
                state.session_id
            ),
            source: crate::StoreError::SessionBindingMismatch {
                bound_session_id: meta.session_id,
                attempted_session_id: state.session_id.clone(),
            },
        });
    }
    Ok(())
}

async fn bind_state_to_store_with_trace(
    host: &crate::RuntimeHostConfig,
    store: &(dyn crate::store::RuntimePersistence + '_),
    state: &mut RuntimeSessionState,
    relation: crate::SessionRelation,
) -> Result<(), SessionError> {
    let result = bind_state_to_store(store, state, relation).await;
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
    runtime_store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    attachment_manifest_store: Option<Arc<dyn crate::store::RuntimePersistence>>,
}

pub(in crate::runtime) struct RuntimeSessionAssembly {
    state: RuntimeSessionState,
    relation: crate::SessionRelation,
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    runtime_lease_executor_id: String,
}

impl RuntimeSessionAssembly {
    pub(in crate::runtime) fn new(
        state: RuntimeSessionState,
        relation: crate::SessionRelation,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self {
            state,
            relation,
            runtime_lease_owner,
            runtime_lease_executor_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn resumed(
        state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
        runtime_lease_executor_id: String,
    ) -> Self {
        Self {
            state,
            relation: crate::SessionRelation::Root,
            runtime_lease_owner,
            runtime_lease_executor_id,
        }
    }
}

impl RuntimePersistenceBindings {
    pub(in crate::runtime) fn new(
        runtime_store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    ) -> Self {
        Self {
            attachment_manifest_store: runtime_store.clone(),
            runtime_store,
        }
    }

    pub(in crate::runtime) fn with_attachment_manifest_store(
        mut self,
        store: Arc<dyn crate::store::RuntimePersistence>,
    ) -> Self {
        self.attachment_manifest_store = Some(store);
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
        if let Some(store) = services.attachment_manifest_store.clone() {
            let manifest: Arc<dyn crate::AttachmentManifest> =
                Arc::new(crate::attachments::PersistenceManifestAdapter(store));
            // Rebind a fresh facade over the flat backend. Attachment ownership
            // is recorded durably on each intent; no live facade state crosses
            // rebuilds or managed-child materialization.
            let previous_attachment_store = Arc::clone(&host.core.durability.attachment_store);
            let backend = Arc::clone(previous_attachment_store.backend());
            let scoped = Arc::new(crate::SessionAttachmentStore::new_with_clock(
                backend,
                manifest,
                state.session_id.clone(),
                Arc::clone(&host.core.clock),
            ));
            host.core.durability.attachment_store = scoped;
        }
        let services = services
            .with_attachment_store(Arc::clone(&host.core.durability.attachment_store))
            .with_process_env_store(Arc::clone(&host.core.durability.process_env_store))
            .with_clock(Arc::clone(&host.core.clock));
        let mut session = Session::new(services.clone(), &state.session_id).await?;
        if let Some(tool_state) = state.tool_state_snapshot().cloned() {
            // Cold rebuild reconciles the persisted catalog over live tools,
            // adopting its generation when the surface is unchanged.
            // `apply_state` (a delta-apply that
            // requires `snapshot.generation == base` and bumps) would reject a
            // session whose surface reached generation ≥ 2 onto a fresh base-1
            // registry — the worker-rebuild / restart divergence. `restore_state`
            // is not generation-fenced against the fresh registry, so any
            // persisted generation rebuilds. A changed live surface bumps once
            // to make the next commit capture it.
            let report = session
                .plugins()
                .tool_registry()
                .restore_state(tool_state)
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            if !report.orphaned.is_empty() {
                tracing::warn!(
                    session_id = %state.session_id,
                    orphaned = ?report.orphaned,
                    "session restored with orphaned tools: no registered source \
                     resolves them; they remain non-members until their source returns"
                );
            }
        }
        session.refresh_tool_catalog().await?;
        if let Some(snapshot) = state.plugin_snapshot().cloned() {
            session
                .plugins()
                .restore(&snapshot)
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
        }
        let protocol_session = Arc::clone(session.plugins().protocol_session());
        let session_id = state.session_id.clone();
        protocol_session
            .restore_session(
                crate::plugin::ProtocolSessionContext::new(&mut session, &session_id),
                &state,
            )
            .await?;
        state.discard_runtime_snapshots();
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
            managed_sessions: Arc::new(Mutex::new(HashMap::new())),
            managed_turns: Arc::new(StdMutex::new(HashMap::new())),
            shared_token_ledger: Arc::new(std::sync::Mutex::new(Vec::new())),
            process_sync_needed: Arc::new(AtomicBool::new(false)),
            resident_graph_head_stale: Arc::new(AtomicBool::new(false)),
            turn_phase_probe: None,
            last_committed_lease_continuity: None,
            last_committed_observation_turn: None,
            graph_loaded_from_store: false,
            resident_session_state: ResidentSessionState::Valid,
            materialized_protocol_config_dirty: false,
        })
    }

    /// Build a runtime for an embedded host with no background worker support.
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

    /// Build a runtime for a host that supports background plugin work.
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

    /// Build a runtime for an embedded host with persistent store support.
    pub async fn from_persistent_embedded_state(
        policy: SessionPolicy,
        host: EmbeddedRuntimeHost,
        services: PersistentRuntimeServices,
        mut state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        bind_state_to_store_with_trace(
            &host.core,
            services.store().as_ref(),
            &mut state,
            crate::SessionRelation::Root,
        )
        .await?;
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

    /// Build a runtime for a background-capable host with persistent store support.
    pub async fn from_persistent_background_state(
        policy: SessionPolicy,
        host: ProcessRuntimeHost,
        services: PersistentRuntimeServices,
        mut state: RuntimeSessionState,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        bind_state_to_store_with_trace(
            &host.embedded().core,
            services.store().as_ref(),
            &mut state,
            crate::SessionRelation::Root,
        )
        .await?;
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
            mut state,
            relation,
            runtime_lease_owner,
            runtime_lease_executor_id,
        } = session;
        let RuntimePersistenceBindings {
            runtime_store: store,
            attachment_manifest_store,
        } = persistence;
        if let Some(store) = store.as_deref()
            && let Err(error) =
                bind_state_to_store_with_trace(&embedded_host.core, store, &mut state, relation)
                    .await
        {
            return Err(error);
        }
        let host = super::host::RuntimeHost::from_embedded_with_work(embedded_host, work);
        let runtime = match store {
            Some(store) => {
                let mut services = PersistentRuntimeServices::new(plugin_session, store);
                if let Some(manifest_store) = attachment_manifest_store {
                    services = services.with_attachment_manifest_store(manifest_store);
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
                let services = RuntimeServices::new(plugin_session);
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
        store: Option<Arc<dyn crate::store::RuntimePersistence>>,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        Self::from_environment_with_plugin_options(
            env,
            policy,
            state,
            store,
            crate::PluginOptions::default(),
            runtime_lease_owner,
        )
        .await
    }

    /// Build from an environment while applying create-time plugin options to
    /// the session-scoped plugin factories.
    pub async fn from_environment_with_plugin_options(
        env: &RuntimeEnvironment,
        policy: SessionPolicy,
        state: RuntimeSessionState,
        store: Option<Arc<dyn crate::store::RuntimePersistence>>,
        plugin_options: crate::PluginOptions,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Result<Self, SessionError> {
        Self::from_environment_for_executor(
            env,
            policy,
            state,
            store,
            plugin_options,
            runtime_lease_owner,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
    }

    async fn from_environment_for_executor(
        env: &RuntimeEnvironment,
        policy: SessionPolicy,
        state: RuntimeSessionState,
        store: Option<Arc<dyn crate::store::RuntimePersistence>>,
        plugin_options: crate::PluginOptions,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
        runtime_lease_executor_id: String,
    ) -> Result<Self, SessionError> {
        let plugin_host = env.plugin_host.as_ref().ok_or_else(|| {
            SessionError::Protocol(
                "RuntimeEnvironment.plugin_host is required for from_environment".to_string(),
            )
        })?;
        let authority = crate::plugin::SessionAuthorityContext {
            plugin_options,
            ..crate::plugin::SessionAuthorityContext::default()
        };
        let plugin_session = match state.plugin_snapshot() {
            Some(snapshot) => plugin_host.rematerialize_session_with_parent(
                state.session_id.as_str(),
                None,
                snapshot,
                crate::plugin::RecordedSessionConfig {
                    authority,
                    protocol_turn_options: state.protocol_turn_options.clone(),
                },
            ),
            None => plugin_host.build_session_with_parent(
                state.session_id.as_str(),
                None,
                crate::plugin::SessionCreationConfig {
                    authority,
                    protocol_turn_options: state.protocol_turn_options.clone(),
                },
            ),
        }
        .map_err(SessionError::Plugin)?;
        let mut embedded = EmbeddedRuntimeHost::new(env.core.clone());
        if let Some(factory) = env.session_store_factory.as_ref() {
            embedded = embedded.with_session_store_factory(Arc::clone(factory));
        }
        if let Some(store) = env.trigger_store.as_ref() {
            embedded = embedded.with_trigger_store(Arc::clone(store));
        }
        Self::assemble_runtime(
            policy,
            embedded,
            plugin_session,
            RuntimePersistenceBindings::new(store),
            env.work.clone(),
            RuntimeSessionAssembly::resumed(state, runtime_lease_owner, runtime_lease_executor_id),
        )
        .await
    }

    pub(super) async fn persist_materialized_protocol_config(
        &mut self,
    ) -> Result<(), SessionError> {
        if !self.materialized_protocol_config_dirty {
            return Ok(());
        }
        let Some(store) = self.services.store.clone() else {
            return Ok(());
        };
        let operation = super::state::boundary_operation(
            &self.state.session_id,
            "protocol-materialization",
            "record-config",
        );
        let protocol_only_first_commit = self.state.checkpoint_ref.is_none();
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                &[],
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        if protocol_only_first_commit {
            commit.config = crate::PersistedSessionConfig::new(self.state.policy.turn_budget);
        }
        let result = commit_runtime_state_with_fresh_session_execution_lease(
            store,
            commit,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(|source| {
            session_commit_error("failed to record protocol configuration", source)
        })?;
        self.state.apply_persisted_commit_result(result);
        self.state.mark_node_ids_persisted(persisted_node_ids);
        self.materialized_protocol_config_dirty = false;
        Ok(())
    }

    /// Persist any dirty state and drop the runtime, returning a lightweight
    /// handle the embedder can cache and resume later via
    /// [`LashRuntime::resume`]. This is the webserver-embedder parking
    /// primitive: the handle holds only the session id, policy, and store
    /// reference — no graph nodes, no plugin session, no HTTP client.
    pub async fn park(mut self) -> Result<ParkedSession, SessionError> {
        let store = self.services.store.clone().ok_or_else(|| {
            SessionError::Protocol(
                "park() requires a persistent runtime (store is not set)".to_string(),
            )
        })?;
        let session_id = self.state.session_id.clone();
        let policy = self.state.effective_policy().clone();
        // Under the settled-state contract every durable mutation commits at
        // its own boundary (turn final commit, config updates, queued-work
        // drains), so a runtime between boundaries already equals its last
        // commit. Flushing is only needed when the state has never been
        // persisted or has pending graph nodes; an unconditional commit
        // here would bump the head revision on every park/close, disturbing
        // host-side head-CAS expectations for what is durably a no-op.
        if self.state.checkpoint_ref.is_none()
            || !self.state.pending_graph_commit().nodes.is_empty()
        {
            let proposed =
                initial_park_preview(&self.state, self.host.core.durability.commit_budget)
                    .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let operation = initial_park_operation(&proposed)
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let (commit, persisted_node_ids) =
                crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                    &mut self.state,
                    &[],
                    operation,
                    self.host.core.durability.commit_budget,
                )
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            // Lane-less host lifecycle boundary: `park` runs between turns and
            // owns no retained session-execution guard.
            let result = commit_runtime_state_with_fresh_session_execution_lease(
                Arc::clone(&store),
                commit,
                &self.runtime_lease_owner,
                &self.runtime_lease_executor_id,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
            .map_err(|source| session_commit_error("failed to persist runtime state", source))?;
            self.state.apply_persisted_commit_result(result);
            self.state.mark_node_ids_persisted(persisted_node_ids);
        }
        // Drain pending tombstones if any. Under KeepHistory this is a
        // no-op (tombstones never get added). Under DropOrphans, a future
        // orphan-trim path would populate the set for Phase 10's vacuum()
        // design.
        Ok(ParkedSession {
            session_id,
            store,
            policy,
            runtime_lease_owner: self.runtime_lease_owner,
            runtime_lease_executor_id: self.runtime_lease_executor_id,
        })
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
        let loaded = crate::store::load_persisted_session_admitted(
            parked.store.as_ref(),
            &parked.session_id,
            &runtime_lease_owner,
            &parked.runtime_lease_executor_id,
            env.core.control.lease_timings.ttl_ms(),
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
            crate::PluginOptions::default(),
            runtime_lease_owner,
            parked.runtime_lease_executor_id,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{bind_state_to_store, initial_park_operation, initial_park_preview};
    use crate::SessionError;

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
            session_id: "park-identity".to_string(),
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
        let first = initial_park_preview(&state, budget).expect("first park preview");

        let mut retry_state = state.clone();
        retry_state.head_revision = 41;
        let retry = initial_park_preview(&retry_state, budget).expect("retry park preview");

        let mut changed_state = retry_state;
        changed_state.turn_index = 1;
        let changed = initial_park_preview(&changed_state, budget).expect("changed park preview");

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
    async fn session_deletion_refusal_keeps_its_type_through_runtime_binding() {
        use crate::SessionStoreFactory;

        let session_id = "deleted-during-runtime-binding";
        let policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);
        let request = crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: policy.clone(),
        };
        let factory = crate::InMemorySessionStoreFactory::new();
        let store = factory
            .create_store(&request)
            .await
            .expect("create session store before deletion");
        factory
            .delete_session(session_id)
            .await
            .expect("delete session before runtime binding");
        let mut state = crate::RuntimeSessionState {
            session_id: session_id.to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };

        let error = bind_state_to_store(store.as_ref(), &mut state, crate::SessionRelation::Root)
            .await
            .expect_err("runtime binding must refuse a retired session");

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
    async fn park_commit_preserves_a_concurrent_session_deletion_refusal() {
        use crate::SessionStoreFactory;
        use crate::runtime::tests::helpers::{
            EmptyTools, plugin_session_with_tools, standard_test_policy, test_host_config,
        };
        use std::sync::Arc;

        let session_id = "deleted-during-park-commit";
        let policy = standard_test_policy();
        let request = crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: policy.clone(),
        };
        let factory = crate::InMemorySessionStoreFactory::new();
        let store = factory
            .create_store(&request)
            .await
            .expect("create session store before parking");
        let runtime = crate::LashRuntime::from_persistent_embedded_state(
            policy.clone(),
            test_host_config(),
            crate::PersistentRuntimeServices::new(
                plugin_session_with_tools(session_id, Arc::new(EmptyTools)),
                Arc::clone(&store),
            ),
            crate::RuntimeSessionState {
                session_id: session_id.to_string(),
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
            .delete_session(session_id)
            .await
            .expect("delete session before park commit");

        let error = match runtime.park().await {
            Ok(_) => panic!("park commit must refuse the retired session"),
            Err(error) => error,
        };
        let canonical = crate::StoreError::SessionDeleted {
            session_id: session_id.to_string(),
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
        use crate::SessionStoreFactory;
        use crate::runtime::tests::helpers::{
            EmptyTools, plugin_session_with_tools, standard_test_policy, test_host_config,
        };
        use std::sync::Arc;

        let session_id = "transient-park-commit-failure";
        let policy = standard_test_policy();
        let factory = crate::InMemorySessionStoreFactory::new();
        let store = factory
            .create_store(&crate::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: session_id.to_string(),
                relation: crate::SessionRelation::Root,
                policy: policy.clone(),
            })
            .await
            .expect("create session store before parking");
        let runtime = crate::LashRuntime::from_persistent_embedded_state(
            policy.clone(),
            test_host_config(),
            crate::PersistentRuntimeServices::new(
                plugin_session_with_tools(session_id, Arc::new(EmptyTools)),
                store,
            ),
            crate::RuntimeSessionState {
                session_id: session_id.to_string(),
                policy,
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                ))
            },
            crate::testing::runtime_lease_owner(),
        )
        .await
        .expect("build runtime before injected backend failure");
        factory
            .raw_store_for_testing(session_id)
            .expect("raw in-memory store")
            .fail_next_runtime_commit(crate::StoreError::Backend(
                "temporary park backend outage".to_string(),
            ));

        let error = match runtime.park().await {
            Ok(_) => panic!("park commit must surface the injected backend failure"),
            Err(error) => error,
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
