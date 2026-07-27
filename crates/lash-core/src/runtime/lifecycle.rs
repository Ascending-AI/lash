use super::*;

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
        if state.session_id.is_empty() {
            state.session_id = uuid::Uuid::new_v4().to_string();
        }
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
        let state_policy = state.policy.clone();
        if let Some(frame) = state.current_agent_frame_mut()
            && frame.assignment.policy.recorded_provider_id().is_empty()
            && frame.assignment.policy.model.id.trim().is_empty()
        {
            frame.assignment.policy = state_policy;
        }
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
            runtime_scope_id: Arc::<str>::from(runtime_scope_id),
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
            residency: Residency::default(),
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
        state: RuntimeSessionState,
    ) -> Result<Self, SessionError> {
        Self::from_host_state(policy, host.into(), services.into_runtime_services(), state).await
    }

    /// Build a runtime for a background-capable host with persistent store support.
    pub async fn from_persistent_background_state(
        policy: SessionPolicy,
        host: ProcessRuntimeHost,
        services: PersistentRuntimeServices,
        state: RuntimeSessionState,
    ) -> Result<Self, SessionError> {
        Self::from_host_state(policy, host.into(), services.into_runtime_services(), state).await
    }

    /// Assemble a runtime from already-resolved parts: the single place that maps
    /// `(store, process_registry)` to the right host/services constructor, applies
    /// residency, and stamps it onto the runtime.
    ///
    /// Every construction path — the live open (`from_environment`), the worker
    /// rebuild (`EmbeddedRuntimeBuilder::build`), and child-session
    /// materialization — routes through here so the store/registry wiring and
    /// residency cannot drift between them. That drift previously shipped: the
    /// worker rebuild silently kept the full graph and skipped the persisted
    /// tool-catalog restore that the live path applied.
    pub(in crate::runtime) async fn assemble_runtime(
        policy: SessionPolicy,
        embedded_host: EmbeddedRuntimeHost,
        plugin_session: Arc<crate::PluginSession>,
        persistence: RuntimePersistenceBindings,
        process_registry: Option<Arc<dyn ProcessRegistry>>,
        mut state: RuntimeSessionState,
        residency: Residency,
    ) -> Result<Self, SessionError> {
        let RuntimePersistenceBindings {
            runtime_store: store,
            attachment_manifest_store,
        } = persistence;
        // ActivePathOnly without a store is a data-loss footgun: trimming drops
        // orphans from RAM with nowhere to reload them from.
        if matches!(residency, Residency::ActivePathOnly) && store.is_none() {
            return Err(SessionError::Protocol(
                "Residency::ActivePathOnly requires a persistent store — \
                 without one, trimmed orphans are irrecoverable"
                    .to_string(),
            ));
        }
        // Heal FIRST (against the full resident set), then trim to the residency.
        // `from_host_state` normalizes again, which is safe on a trimmed graph.
        apply_residency_on_load(&mut state, residency);
        let mut runtime = match (store, process_registry) {
            (Some(store), Some(registry)) => {
                let host = ProcessRuntimeHost::new(embedded_host, registry);
                let mut services = PersistentRuntimeServices::new(plugin_session, store);
                if let Some(manifest_store) = attachment_manifest_store {
                    services = services.with_attachment_manifest_store(manifest_store);
                }
                Self::from_persistent_background_state(policy, host, services, state).await?
            }
            (Some(store), None) => {
                let mut services = PersistentRuntimeServices::new(plugin_session, store);
                if let Some(manifest_store) = attachment_manifest_store {
                    services = services.with_attachment_manifest_store(manifest_store);
                }
                Self::from_persistent_embedded_state(policy, embedded_host, services, state).await?
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
        runtime.residency = residency;
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
            env.residency,
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
        if self.state.head_revision.is_none() || self.state.graph_flush_required {
            let proposed = crate::store::RuntimeCommit::persisted_state(&self.state, &[]);
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
    /// Loads only the active-path graph when
    /// `env.residency == ActivePathOnly`; under `KeepAll`
    /// loads the full graph (current behavior).
    pub async fn resume(
        parked: ParkedSession,
        env: &RuntimeEnvironment,
    ) -> Result<Self, SessionError> {
        // Under ActivePathOnly, skip the full-graph load: fetch head
        // metadata + the active-path chain only. Durable impls can
        // ActivePathOnly is an exact store capability. Stores that do
        // not support it must return UnsupportedReadScope; resume does
        // not fall back to a full graph load.
        let loaded = match env.residency {
            Residency::KeepAll => {
                crate::store::load_persisted_session_state(parked.store.as_ref()).await
            }
            Residency::ActivePathOnly => {
                crate::store::load_persisted_session_state_active_path(parked.store.as_ref(), None)
                    .await
            }
        }
        .map_err(|err| SessionError::Protocol(format!("failed to load runtime state: {err}")))?;
        let state = loaded.unwrap_or_else(|| RuntimeSessionState {
            session_id: parked.session_id.clone(),
            policy: parked.policy.clone(),
            ..RuntimeSessionState::default()
        });
        Self::from_environment(env, parked.policy, state, Some(parked.store)).await
    }

    /// Opt-in async read for historic (non-active-path) nodes under
    /// `Residency::ActivePathOnly`. Plugins that walk the full graph
    /// call this instead of `session_graph().find_node()` so missing
    /// nodes surface as `Ok(None)` rather than silently missing.
    pub async fn get_historic_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, SessionError> {
        if let Some(node) = self.state.session_graph.find_node(node_id) {
            return Ok(Some(node.clone()));
        }
        let store = self.services.store.clone().ok_or_else(|| {
            SessionError::Protocol("get_historic_node() requires a persistent runtime".to_string())
        })?;
        store
            .load_node(node_id)
            .await
            .map_err(|err| SessionError::Protocol(format!("failed to load historic node: {err}")))
    }

    /// Store-resident node IDs that are NOT reachable from the current
    /// leaf — i.e. orphans eligible for tombstoning. lash owns RAM; the
    /// host owns disk lifecycle, so this is a primitive the host calls
    /// on its own schedule (e.g. every N turns, or off-peak).
    ///
    /// Typical autonomous-agent loop:
    ///
    /// ```ignore
    /// let orphans = runtime.orphaned_node_ids().await?;
    /// if !orphans.is_empty() {
    ///     store.tombstone_nodes(&orphans).await;
    /// }
    /// // And less often:
    /// store.vacuum().await;
    /// ```
    pub async fn orphaned_node_ids(&self) -> Result<Vec<String>, SessionError> {
        let store = self.services.store.clone().ok_or_else(|| {
            SessionError::Protocol("orphaned_node_ids() requires a persistent runtime".to_string())
        })?;
        let Some(read) = store
            .load_session(crate::store::SessionReadScope::FullGraph)
            .await
            .map_err(|err| SessionError::Protocol(format!("failed to load full graph: {err}")))?
        else {
            return Ok(Vec::new());
        };
        let active: std::collections::HashSet<&str> = read
            .graph
            .active_path_nodes()
            .iter()
            .map(|node| node.node_id.as_str())
            .collect();
        Ok(read
            .graph
            .nodes
            .iter()
            .filter(|node| !active.contains(node.node_id.as_str()))
            .map(|node| node.node_id.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::initial_park_operation;

    #[test]
    fn initial_park_identity_is_stable_for_replay_and_distinguishes_content() {
        let mut state = crate::RuntimeSessionState {
            session_id: "park-identity".to_string(),
            graph_flush_required: true,
            ..crate::RuntimeSessionState::default()
        };
        state.ensure_agent_frame_initialized();
        let first = crate::store::RuntimeCommit::persisted_state(&state, &[]);

        let mut retry_state = state.clone();
        retry_state.head_revision = Some(41);
        let retry = crate::store::RuntimeCommit::persisted_state(&retry_state, &[]);

        let mut changed_state = retry_state;
        changed_state.turn_index = 1;
        let changed = crate::store::RuntimeCommit::persisted_state(&changed_state, &[]);

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
}
