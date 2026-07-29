use super::*;

fn initial_park_preview(
    state: &crate::RuntimeSessionState,
) -> Result<crate::store::RuntimeCommit, crate::StoreError> {
    let operation =
        super::state::boundary_operation(&state.session_id, "initial-park-preview", "preview");
    let mut graph = state.pending_graph_commit();
    graph.derive_node_ids(&state.session_id, &operation)?;
    crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
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
    policy: &SessionPolicy,
    store: &(dyn crate::store::RuntimePersistence + '_),
    state: &mut RuntimeSessionState,
    relation: crate::SessionRelation,
) -> Result<(), SessionError> {
    let binding = crate::SessionBinding {
        session_id: state.session_id.clone(),
        relation,
        model_id: policy.model.id.clone(),
        cwd: std::env::current_dir()
            .ok()
            .and_then(|path| path.to_str().map(str::to_string)),
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

pub(in crate::runtime) struct RuntimePersistenceBindings {
    runtime_store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    attachment_manifest_store: Option<Arc<dyn crate::store::RuntimePersistence>>,
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
    /// Override the owner identity used for durable session execution leases.
    ///
    /// Normal embedded runtimes use a fresh owner and incarnation so concurrent
    /// opens of the same session exclude each other. Durable orchestrators may
    /// set a stable `(owner_id, incarnation_id)` pair for one serialized logical
    /// workflow.
    pub fn set_runtime_lease_owner(&mut self, owner: crate::LeaseOwnerIdentity) {
        self.runtime_lease_owner = owner;
        self.last_committed_lease_continuity = None;
    }

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
    ) -> Result<Self, SessionError> {
        // Defaulted state (e.g. `RuntimeSessionState::default()` used
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
        state.policy = state.effective_policy().clone();
        state.protocol_turn_options = state.effective_protocol_turn_options().clone();
        let policy = state.effective_policy().clone();
        if policy.model.id.trim().is_empty() {
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
        if let Some(tool_state) = state.tool_state_snapshot.clone() {
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
        if let Some(snapshot) = state.plugin_snapshot.clone() {
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
            .await;
        let protocol_turn_options = state.protocol_turn_options.clone();
        let runtime_scope_id = uuid::Uuid::new_v4().to_string();
        let runtime_lease_owner = crate::LeaseOwnerIdentity::opaque(
            runtime_scope_id.clone(),
            uuid::Uuid::new_v4().to_string(),
        );
        Ok(Self {
            session: Some(session),
            policy,
            host,
            services,
            state,
            runtime_lease_owner,
            managed_sessions: Arc::new(Mutex::new(HashMap::new())),
            managed_turns: Arc::new(Mutex::new(HashMap::new())),
            protocol_turn_options,
            shared_token_ledger: Arc::new(std::sync::Mutex::new(Vec::new())),
            process_sync_needed: Arc::new(AtomicBool::new(false)),
            turn_phase_probe: None,
            last_committed_lease_continuity: None,
            last_committed_observation_turn: None,
            graph_loaded_from_store: false,
        })
    }

    /// Build a runtime for an embedded host with no background worker support.
    pub async fn from_embedded_state(
        policy: SessionPolicy,
        host: EmbeddedRuntimeHost,
        services: RuntimeServices,
        state: RuntimeSessionState,
    ) -> Result<Self, SessionError> {
        Self::from_host_state(policy, host.into(), services, state).await
    }

    /// Build a runtime for a host that supports background plugin work.
    pub async fn from_background_state(
        policy: SessionPolicy,
        host: ProcessRuntimeHost,
        services: RuntimeServices,
        state: RuntimeSessionState,
    ) -> Result<Self, SessionError> {
        Self::from_host_state(policy, host.into(), services, state).await
    }

    /// Build a runtime for an embedded host with persistent store support.
    pub async fn from_persistent_embedded_state(
        policy: SessionPolicy,
        host: EmbeddedRuntimeHost,
        services: PersistentRuntimeServices,
        mut state: RuntimeSessionState,
    ) -> Result<Self, SessionError> {
        bind_state_to_store(
            &policy,
            services.store().as_ref(),
            &mut state,
            crate::SessionRelation::Root,
        )
        .await?;
        Self::from_host_state(policy, host.into(), services.into_runtime_services(), state).await
    }

    /// Build a runtime for a background-capable host with persistent store support.
    pub async fn from_persistent_background_state(
        policy: SessionPolicy,
        host: ProcessRuntimeHost,
        services: PersistentRuntimeServices,
        mut state: RuntimeSessionState,
    ) -> Result<Self, SessionError> {
        bind_state_to_store(
            &policy,
            services.store().as_ref(),
            &mut state,
            crate::SessionRelation::Root,
        )
        .await?;
        Self::from_host_state(policy, host.into(), services.into_runtime_services(), state).await
    }

    /// Assemble a runtime from already-resolved parts: the single place that maps
    /// `(store, process_registry)` to the right host/services constructor.
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
        process_registry: Option<Arc<dyn ProcessRegistry>>,
        mut state: RuntimeSessionState,
        relation: crate::SessionRelation,
    ) -> Result<Self, SessionError> {
        let RuntimePersistenceBindings {
            runtime_store: store,
            attachment_manifest_store,
        } = persistence;
        if let Some(store) = store.as_deref() {
            bind_state_to_store(&policy, store, &mut state, relation).await?;
        }
        let runtime = match (store, process_registry) {
            (Some(store), Some(registry)) => {
                let host = ProcessRuntimeHost::new(embedded_host, registry);
                let mut services = PersistentRuntimeServices::new(plugin_session, store);
                if let Some(manifest_store) = attachment_manifest_store {
                    services = services.with_attachment_manifest_store(manifest_store);
                }
                Self::from_host_state(policy, host.into(), services.into_runtime_services(), state)
                    .await?
            }
            (Some(store), None) => {
                let mut services = PersistentRuntimeServices::new(plugin_session, store);
                if let Some(manifest_store) = attachment_manifest_store {
                    services = services.with_attachment_manifest_store(manifest_store);
                }
                Self::from_host_state(
                    policy,
                    embedded_host.into(),
                    services.into_runtime_services(),
                    state,
                )
                .await?
            }
            (None, Some(registry)) => {
                let host = ProcessRuntimeHost::new(embedded_host, registry);
                let services = RuntimeServices::new(plugin_session);
                Self::from_background_state(policy, host, services, state).await?
            }
            (None, None) => {
                let services = RuntimeServices::new(plugin_session);
                Self::from_embedded_state(policy, embedded_host, services, state).await?
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
    ) -> Result<Self, SessionError> {
        let plugin_host = env.plugin_host.as_ref().ok_or_else(|| {
            SessionError::Protocol(
                "RuntimeEnvironment.plugin_host is required for from_environment".to_string(),
            )
        })?;
        let plugin_session = plugin_host
            .build_session(state.session_id.as_str(), state.plugin_snapshot.as_ref())
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        let mut embedded = EmbeddedRuntimeHost::new(env.core.clone());
        if let Some(factory) = env.session_store_factory.as_ref() {
            embedded = embedded.with_session_store_factory(Arc::clone(factory));
        }
        if let Some(store) = env.trigger_store.as_ref() {
            embedded = embedded.with_trigger_store(Arc::clone(store));
        }
        let mut runtime = Self::assemble_runtime(
            policy,
            embedded,
            plugin_session,
            RuntimePersistenceBindings::new(store),
            env.process_registry.as_ref().cloned(),
            state,
            crate::SessionRelation::Root,
        )
        .await?;
        // Thread the host-owned work drivers onto this session's host so
        // process starts and queued turns can drive ready work directly.
        runtime.host.process_work_driver = env.process_work_driver.clone();
        runtime.host.queued_work_driver = env.queued_work_driver.clone();
        Ok(runtime)
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
        let policy = self.policy.clone();
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
            let proposed = initial_park_preview(&self.state)
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let operation = initial_park_operation(&proposed)
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let (commit, persisted_node_ids) =
                crate::store::RuntimeCommit::persisted_state_with_operation(
                    &mut self.state,
                    &[],
                    operation,
                )
                .map_err(|err| SessionError::Protocol(err.to_string()))?;
            let result = commit_runtime_state_with_fresh_session_execution_lease(
                Arc::clone(&store),
                commit,
                &self.runtime_lease_owner,
                self.host.core.control.lease_timings,
                Arc::clone(&self.host.core.clock),
            )
            .await
            .map_err(|err| {
                SessionError::Protocol(format!("failed to persist runtime state: {err}"))
            })?;
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
        })
    }

    /// Resume a previously parked session against a shared environment.
    pub async fn resume(
        parked: ParkedSession,
        env: &RuntimeEnvironment,
    ) -> Result<Self, SessionError> {
        let loaded = crate::store::load_persisted_session_state(parked.store.as_ref())
            .await
            .map_err(|err| {
                SessionError::Protocol(format!("failed to load runtime state: {err}"))
            })?;
        let state = loaded.unwrap_or_else(|| RuntimeSessionState {
            session_id: parked.session_id.clone(),
            policy: parked.policy.clone(),
            ..RuntimeSessionState::default()
        });
        Self::from_environment(env, parked.policy, state, Some(parked.store)).await
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
            parts: crate::shared_parts(vec![crate::Part {
                id: format!("{id}.p0"),
                kind: crate::PartKind::Text,
                content: content.to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: crate::PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        }
    }

    #[test]
    fn initial_park_identity_is_stable_for_replay_and_distinguishes_content() {
        let mut state = crate::RuntimeSessionState {
            session_id: "park-identity".to_string(),
            ..crate::RuntimeSessionState::default()
        };
        state.ensure_agent_frame_initialized();
        state.append_active_conversation_messages(&[user_message(
            "pending-user-message",
            "persist me before parking",
        )]);
        let first = initial_park_preview(&state).expect("first park preview");

        let mut retry_state = state.clone();
        retry_state.head_revision = 41;
        let retry = initial_park_preview(&retry_state).expect("retry park preview");

        let mut changed_state = retry_state;
        changed_state.turn_index = 1;
        let changed = initial_park_preview(&changed_state).expect("changed park preview");

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
        let policy = crate::SessionPolicy::default();
        let request = crate::SessionStoreCreateRequest {
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
            ..crate::RuntimeSessionState::default()
        };

        let error = bind_state_to_store(
            &policy,
            store.as_ref(),
            &mut state,
            crate::SessionRelation::Root,
        )
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
}
