//! Session initialisation: the `SessionCreateRequest` pipeline that resolves,
//! materializes, and durably commits a new ordinary session, plus the
//! process-origin port that initializes a recorded child and drives its first
//! turn through the shared session-turn path.
//!
//! This is one of two session-construction APIs, kept deliberately separate
//! (ADR 0089): initialisation builds a *new* session from a request that
//! carries policy, relation, tool access, subagent context, initial nodes,
//! observer intents, and the spawn-time `SessionPluginInit` capture. Catalog
//! `SessionStoreFactory::fork_at` materializes durable fork lineage and
//! retained-frame content with different failure semantics and no live
//! parent's plugin-init payload; neither API covers the other.
//!
//! Run-scoped child residency (FIG-3424): the child runtime a
//! `ProcessInput::SessionTurn` initializes is owned by that process run, held
//! as a local for the run's duration, and dropped when the run completes —
//! success, failure, or cancellation. Nothing caches it. A redelivery in a
//! new run reopens the session's durable row through the ordinary open path;
//! the durable row is the only continuity between attempts.

use super::*;
use crate::TurnId;
use crate::facade_support::RuntimeSessionStateFacadeOps;
use crate::runtime::host::EmbeddedRuntimeHost;

/// A create request resolved into everything materialization needs. Nothing
/// on the plan re-reads a live session: `SessionPluginInit` is the spawn-time
/// capture the request carried, and the initial runtime state is fully built
/// here.
pub(in crate::runtime::session_manager) struct SessionInitPlan {
    session_id: SessionId,
    relation: SessionRelation,
    pending_observer_intents: Vec<crate::SessionObserverIntent>,
    parent_session_id: Option<SessionId>,
    policy: SessionPolicy,
    initial_runtime_state: RuntimeSessionState,
    plugin_config: crate::plugin::SessionCreationConfig,
    plugin_source: crate::SessionPluginSource,
    protocol_request: SessionCreateRequest,
}

/// The resolved request with its runtime assembled but not yet committed.
struct MaterializedSession {
    runtime: LashRuntime,
    store_binding: Arc<dyn crate::store::RuntimePersistence>,
}

/// The child a `ProcessInput::SessionTurn` run owns: an ordinary session
/// runtime, created or reopened, held only for the run's duration.
pub(in crate::runtime::session_manager) struct InitializedSession {
    pub(in crate::runtime::session_manager) handle: RuntimeHandle,
    pub(in crate::runtime::session_manager) session_id: SessionId,
}

/// The initialized child session and its committed first turn returned by
/// [`RuntimeSessionServices::initialize_session_and_run_turn`].
pub(in crate::runtime::session_manager) struct InitializedSessionTurn {
    /// The child the turn ran on — a newly created session or a durable one
    /// a redelivery reopened.
    pub session_id: SessionId,
    pub turn: AssembledTurn,
}

pub(in crate::runtime::session_manager) async fn resolve_session_init(
    current: &CurrentSessionCapability,
    mut request: SessionCreateRequest,
) -> Result<SessionInitPlan, crate::PluginError> {
    let session_id = request
        .session_id
        .take()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| SessionId::from(uuid::Uuid::new_v4().to_string()));
    request.session_id = Some(session_id.clone());
    let parent_session_id = request.relation.parent_session_id().map(ToOwned::to_owned);
    // Every session initializes empty: `SessionStartPoint::Empty` is the only
    // start point initialisation admits. Durable forks and resumed sessions
    // get their state from the store, not from the create request.
    let start_state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(current.policy.clone())
    };
    let policy = resolve_session_policy(current, &request, &session_id)
        .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    request.policy = Some(policy.clone());
    let initial_runtime_state = build_runtime_state(
        session_id.clone(),
        &request,
        start_state,
        &policy,
        current.host.core.clock.as_ref(),
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let plugin_config = crate::plugin::SessionCreationConfig {
        authority: crate::plugin::SessionAuthorityContext {
            tool_access: request.tool_access.clone(),
            subagent: request.subagent.clone(),
            plugin_options: request.plugin_options.clone(),
        },
        protocol_turn_options: initial_runtime_state.protocol_turn_options.clone(),
    };

    let mut seen_observed_processes = std::collections::HashSet::new();
    let pending_observer_intents = request
        .observed_processes
        .iter()
        .filter(|process_id| seen_observed_processes.insert(process_id.as_str()))
        .cloned()
        .map(crate::SessionObserverIntent::host_requested)
        .collect();
    Ok(SessionInitPlan {
        session_id,
        relation: request.relation.clone(),
        pending_observer_intents,
        parent_session_id: parent_session_id.map(Into::into),
        policy,
        initial_runtime_state,
        plugin_config,
        plugin_source: request.plugin_source,
        protocol_request: request,
    })
}

/// Resolve the new session's policy, honoring the provider pin the parent
/// session's policy recorded.
///
/// The recorded provider id is a durable fact (ADR 0066), so a create request
/// that carries no policy inherits it and a request whose policy names a
/// *different* provider is refused with
/// [`SessionError::ProviderMismatch`](crate::SessionError::ProviderMismatch)
/// rather than silently overwriting the pin the root open established.
fn resolve_session_policy(
    current: &CurrentSessionCapability,
    request: &SessionCreateRequest,
    session_id: &SessionId,
) -> Result<SessionPolicy, crate::SessionError> {
    let recorded_provider_id = current.policy.recorded_provider_id().to_string();
    let mut policy = request
        .policy
        .clone()
        .unwrap_or_else(|| current.policy.clone());
    policy.provider_id = SessionPolicy::settle_provider_pin(
        session_id,
        &recorded_provider_id,
        policy.recorded_provider_id(),
    )?;
    if request.relation.parent_session_id().is_some() {
        policy.session_id = Some(SessionId::from(session_id.to_string()));
    }
    Ok(policy)
}

fn build_runtime_state(
    session_id: SessionId,
    request: &SessionCreateRequest,
    mut base: RuntimeSessionState,
    policy: &SessionPolicy,
    clock: &dyn crate::Clock,
) -> Result<RuntimeSessionState, crate::StoreError> {
    base.session_id = session_id;
    base.head_revision = 0;
    base.checkpoint_components.complete_for_new_session()?;
    // The child captures its own live namespaces at its first boundary.
    base.set_plugin_state(None);
    base.policy = policy.clone();
    base.authority.tool_access = request.tool_access.clone();
    base.authority.subagent = request.subagent.clone();
    base.session_graph = crate::SessionGraph::default();
    base.agent_frames.clear();
    base.current_frame_node_id = None;
    base.persisted_node_ids.clear();
    base.reset_initial_agent_frame_with_clock(
        crate::AgentFrameAssignment::from_session_request_facts(
            request.plugin_options.clone(),
            policy.clone(),
        ),
        base.protocol_turn_options.clone(),
        clock,
    );
    let draft_namespace = format!("create-session:{}", base.session_id);
    append_session_nodes_to_state_with_clock(
        &mut base,
        &request.initial_nodes,
        &draft_namespace,
        clock,
    );
    Ok(base)
}

async fn materialize_session_init(
    current: &CurrentSessionCapability,
    plan: &SessionInitPlan,
) -> Result<MaterializedSession, crate::PluginError> {
    let (plugins, plugin_init) = build_session_plugins(current, plan)?;
    let mut initial_state = plan.initial_runtime_state.clone();
    if let Some(init) = plugin_init {
        // The captured tool state seeds the child's session state so the
        // shared open path installs it through `install_persisted_tool_state`:
        // the lost-member report, its trace evidence, and the `Require`
        // refusal at creation apply to a forked child exactly as they do to a
        // reopening session (FIG-3367).
        initial_state.set_tool_state_snapshot(Some(init.tool_state.clone()));
    }
    let store_binding = bind_session_store(current, plan).await?;
    // Session creation routes through the same assembler as live open and
    // worker-rebuild paths. A freshly created session has a single path, so it
    // materializes under KeepAll (residency trimming is an open-time concern).
    let mut runtime = LashRuntime::assemble_runtime(
        plan.policy.clone(),
        embedded_host(current),
        plugins,
        crate::runtime::lifecycle::RuntimePersistenceBindings::new(Some(store_binding.clone())),
        current.host.work.clone(),
        crate::runtime::lifecycle::RuntimeSessionAssembly::new(
            initial_state,
            plan.relation.clone(),
            current.runtime_lease_owner.clone(),
        ),
    )
    .await
    .map_err(|err| crate::PluginError::Session(err.to_string()))?;

    runtime.configure_protocol_on_materialize(
        &plan.protocol_request.plugin_options,
        plan.protocol_request.relation.parent_session_id().is_none(),
    )?;

    Ok(MaterializedSession {
        runtime,
        store_binding,
    })
}

fn build_session_plugins<'a>(
    current: &CurrentSessionCapability,
    plan: &'a SessionInitPlan,
) -> Result<
    (
        Arc<crate::PluginSession>,
        Option<&'a crate::SessionPluginInit>,
    ),
    crate::PluginError,
> {
    match plan.plugin_source {
        crate::SessionPluginSource::CurrentHostFresh => Ok((
            current.plugins.host().build_session_with_parent(
                &plan.session_id,
                plan.parent_session_id.clone(),
                plan.plugin_config.clone(),
            )?,
            None,
        )),
        // The fork initializes from the spawn-time capture alone. There is
        // deliberately no read of the running session that created this
        // request — on a process worker that session is a synthetic runtime
        // carrying fresh host plugins, not the real parent.
        crate::SessionPluginSource::ParentFork => {
            let init = plan.protocol_request.plugin_init.as_ref().ok_or(
                crate::PluginError::MissingSessionInit {
                    session_id: plan.session_id.clone(),
                },
            )?;
            let session = current.plugins.host().build_session_from_init(
                &plan.session_id,
                plan.parent_session_id.clone(),
                init,
                plan.plugin_config.clone(),
            )?;
            Ok((session, Some(init)))
        }
    }
}

async fn bind_session_store(
    current: &CurrentSessionCapability,
    plan: &SessionInitPlan,
) -> Result<Arc<dyn crate::store::RuntimePersistence>, crate::PluginError> {
    let Some(factory) = &current.host.session_store_factory else {
        return Err(crate::PluginError::MissingSessionStore {
            session_id: plan.session_id.clone(),
        });
    };
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: plan.session_id.clone(),
            relation: plan.relation.clone(),
            pending_observer_intents: plan.pending_observer_intents.clone(),
            policy: plan.policy.clone(),
        })
        .await
        .map_err(|message| {
            crate::PluginError::Session(session_creation_store_factory_error(
                &plan.session_id,
                message.to_string(),
            ))
        })?;
    validate_created_session_store_binding(store.as_ref(), &plan.session_id).await?;
    Ok(store)
}

fn embedded_host(current: &CurrentSessionCapability) -> EmbeddedRuntimeHost {
    EmbeddedRuntimeHost {
        core: current.host.core.clone(),
        session_store_factory: current.host.session_store_factory.clone(),
        trigger_store: current.host.trigger_store.clone(),
        process_definitions: current.host.process_definitions.clone(),
    }
}

fn session_creation_store_guidance() -> &'static str {
    "A session-creation factory must return a distinct store bound to the requested session id. \
     Do not wrap a single pre-opened store in LashCoreBuilder::store_factory; pass that exact \
     store with SessionBuilder::store(...) and configure \
     LashCoreBuilder::session_creation_store_factory(...) for sessions created from a running session."
}

fn session_creation_store_factory_error(session_id: &SessionId, message: String) -> String {
    format!(
        "failed to create store for session `{session_id}`: {message}. {}",
        session_creation_store_guidance()
    )
}

async fn validate_created_session_store_binding(
    store: &dyn crate::RuntimePersistence,
    session_id: &SessionId,
) -> Result<(), crate::PluginError> {
    let meta = store.load_session_meta().await.map_err(|err| {
        crate::PluginError::Session(format!(
            "failed to inspect store for session `{session_id}`: {err}. {}",
            session_creation_store_guidance()
        ))
    })?;
    if let Some(meta) = meta
        && meta.session_id != session_id
    {
        return Err(crate::PluginError::Session(format!(
            "configured session-creation store is already bound to session `{}` and cannot be used for session `{session_id}`. {}",
            meta.session_id,
            session_creation_store_guidance()
        )));
    }
    Ok(())
}

/// The durable create commit: writes the materialized session's initial head
/// through the canonical admission boundary, then settles the request's
/// process observer intents. The runtime itself stays owned by the caller —
/// nothing registers it.
async fn commit_initialized_session(
    current: &CurrentSessionCapability,
    plan: SessionInitPlan,
    mut materialized: MaterializedSession,
) -> Result<(SessionHandle, RuntimeHandle), crate::PluginError> {
    let mut persisted_state = materialized
        .runtime
        .export_persisted_state()
        .await
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    let operation = super::super::state::boundary_operation(
        &persisted_state.session_id,
        &plan.session_id,
        "create-session",
    );
    let (mut commit, persisted_node_ids) =
        crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
            &mut persisted_state,
            &[],
            operation,
            materialized.runtime.host.core.durability.commit_budget,
        )
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    // Stamp last: the semantic-boundary identity hashes the commit's
    // canonical request content, so every content edit must precede it.
    commit
        .stamp_semantic_boundary()
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    // Lane-less by construction: the session is being created before it
    // owns an execution lane. A guard for another session cannot authorize
    // this commit.
    let result = commit_runtime_state_with_fresh_session_execution_lease(
        Arc::clone(&materialized.store_binding),
        commit,
        &materialized.runtime.runtime_lease_owner,
        &materialized.runtime.runtime_lease_executor_id,
        materialized.runtime.host.core.control.lease_timings,
        Arc::clone(&materialized.runtime.host.core.clock),
    )
    .await
    .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    persisted_state.apply_persisted_commit_result(result);
    persisted_state.mark_node_ids_persisted(persisted_node_ids);
    materialized.runtime.state = persisted_state;
    materialized.runtime.materialized_protocol_config_dirty = false;
    let observed_processes = settle_session_observer_intents(
        current,
        &plan.session_id,
        materialized.store_binding.as_ref(),
    )
    .await?;
    let handle = SessionHandle {
        session_id: plan.session_id,
        parent_session_id: plan.parent_session_id,
        policy: plan.policy,
        observed_processes,
    };
    Ok((handle, RuntimeHandle::new(materialized.runtime)))
}

/// Settle the observer intents a create request recorded into the new
/// session's durable row. Idempotent and durably recoverable, so a reopen
/// after a crashed create attempt completes it the same way.
async fn settle_session_observer_intents(
    current: &CurrentSessionCapability,
    session_id: &SessionId,
    store: &dyn crate::store::RuntimePersistence,
) -> Result<Vec<crate::plugin::SessionObservedProcessReceipt>, crate::PluginError> {
    let observer_intent_source = crate::runtime::SessionObserverIntentSource::Persisted(store);
    crate::runtime::reconcile_session_process_observer_intents(
        current.host.process_registry().map(Arc::as_ref),
        session_id,
        observer_intent_source,
    )
    .await
    .map_err(|error| {
        crate::PluginError::Session(format!(
            "failed to settle session-create observer intents: {error}"
        ))
    })
}

/// The session's durable store when its catalog row already exists.
async fn durable_session_store(
    current: &CurrentSessionCapability,
    session_id: &SessionId,
) -> Result<Option<Arc<dyn crate::store::RuntimePersistence>>, crate::PluginError> {
    let Some(factory) = &current.host.session_store_factory else {
        return Ok(None);
    };
    factory
        .open_existing_store_by_id(session_id)
        .await
        .map_err(|error| {
            crate::PluginError::Session(format!(
                "failed to inspect session `{session_id}` before initialisation: {error}"
            ))
        })
}

/// `SessionLifecycleService::create_session`: a host's fresh create. The
/// durable row must not already exist — replaying a recorded identity is the
/// process run port's contract, not a host create's.
pub(in crate::runtime::session_manager) async fn create_session(
    current: &CurrentSessionCapability,
    request: SessionCreateRequest,
) -> Result<SessionHandle, crate::PluginError> {
    let plan = resolve_session_init(current, request).await?;
    if plan.session_id == current.session_id
        || durable_session_store(current, &plan.session_id)
            .await?
            .is_some()
    {
        return Err(crate::PluginError::Session(format!(
            "session `{}` already exists",
            plan.session_id
        )));
    }
    let materialized = materialize_session_init(current, &plan).await?;
    // A host create returns only the durable handle: the runtime it assembled
    // to commit the initial head is dropped here, and the session is run by
    // opening it through the ordinary open path like every other session.
    let (handle, _runtime) =
        Box::pin(commit_initialized_session(current, plan, materialized)).await?;
    Ok(handle)
}

/// `ProcessInput::SessionTurn` initialisation: create the recorded session,
/// or reopen it when a previous attempt already committed the durable row.
/// Either way the result is an ordinary session runtime owned by the caller.
async fn initialize_session(
    current: &CurrentSessionCapability,
    request: SessionCreateRequest,
) -> Result<InitializedSession, crate::PluginError> {
    let plan = resolve_session_init(current, request).await?;
    match durable_session_store(current, &plan.session_id).await? {
        Some(store) => reopen_initialized_session(current, &plan, store).await,
        None => {
            let materialized = materialize_session_init(current, &plan).await?;
            let (handle_view, handle) =
                Box::pin(commit_initialized_session(current, plan, materialized)).await?;
            Ok(InitializedSession {
                handle,
                session_id: handle_view.session_id,
            })
        }
    }
}

/// Reopen an already-committed session through the ordinary open path: load
/// its durable head, rebuild its plugin session from the recorded state, and
/// assemble the runtime under the resumed-session assembly. This is the
/// redelivery contract for `ProcessInput::SessionTurn` — a new run whose
/// previous attempt crashed after the create commit finds the row here and
/// never re-runs the create.
pub(in crate::runtime::session_manager) async fn reopen_initialized_session(
    current: &CurrentSessionCapability,
    plan: &SessionInitPlan,
    store: Arc<dyn crate::store::RuntimePersistence>,
) -> Result<InitializedSession, crate::PluginError> {
    // The durable row decides lineage. A redelivery replays the same recorded
    // request, so a recorded relation that disagrees is a conflict, never a
    // silent adopt.
    let meta = store.load_session_meta().await.map_err(|error| {
        crate::PluginError::Session(format!(
            "failed to inspect session `{}` before reopen: {error}",
            plan.session_id
        ))
    })?;
    if let Some(meta) = &meta
        && meta.relation != plan.relation
    {
        return Err(crate::PluginError::Session(format!(
            "session `{}` already exists with relation {:?}; a redelivery must replay its recorded relation, not {:?}",
            plan.session_id, meta.relation, plan.relation
        )));
    }
    let state = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .map_err(|error| {
            crate::PluginError::Session(format!(
                "failed to load session `{}` for reopen: {error}",
                plan.session_id
            ))
        })?
        .ok_or_else(|| {
            crate::PluginError::Session(format!(
                "session `{}` committed its catalog row without a session head",
                plan.session_id
            ))
        })?;
    let authority = crate::plugin::SessionAuthorityContext {
        tool_access: state.authority.tool_access.clone(),
        subagent: state.authority.subagent.clone(),
        plugin_options: plan.protocol_request.plugin_options.clone(),
    };
    let plugin_host = current.plugins.host();
    let plugins = match state.plugin_state() {
        Some(snapshot) => plugin_host.rematerialize_session_with_parent(
            state.session_id.as_str(),
            plan.parent_session_id.clone(),
            snapshot,
            crate::plugin::RecordedSessionConfig {
                authority,
                protocol_turn_options: state.protocol_turn_options.clone(),
            },
        ),
        None => plugin_host.build_session_with_parent(
            state.session_id.as_str(),
            plan.parent_session_id.clone(),
            crate::plugin::SessionCreationConfig {
                authority,
                protocol_turn_options: state.protocol_turn_options.clone(),
            },
        ),
    }?;
    let policy = state.effective_policy().clone();
    let mut runtime = LashRuntime::assemble_runtime(
        policy,
        embedded_host(current),
        plugins,
        crate::runtime::lifecycle::RuntimePersistenceBindings::new(Some(store.clone())),
        current.host.work.clone(),
        crate::runtime::lifecycle::RuntimeSessionAssembly::resumed(
            state,
            current.runtime_lease_owner.clone(),
            current.runtime_lease_executor_id.clone(),
        ),
    )
    .await
    .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    runtime
        .configure_protocol_on_materialize(
            &plan.protocol_request.plugin_options,
            plan.parent_session_id.is_none(),
        )
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    // Finish any observer intents a crashed create attempt left pending; the
    // settle is durable and idempotent, so completing it here is the same
    // work the create path performs.
    settle_session_observer_intents(current, &plan.session_id, store.as_ref()).await?;
    Ok(InitializedSession {
        handle: RuntimeHandle::new(runtime),
        session_id: plan.session_id.clone(),
    })
}

impl RuntimeSessionServices {
    /// Initialize a brand-new session and run its first turn as one operation.
    ///
    /// This is the process-origin initialization port (FIG-3377): the only
    /// caller is `run_process_session_turn`, which hands over the durable
    /// `SessionCreateRequest` recorded on the process row, the process's own
    /// execution authority, and its cancellation token. No facade or worker
    /// type reaches this layer.
    ///
    /// Ordering contract (the crash/replay boundary on both substrates):
    ///
    /// 1. The child session's create commit lands durably in the child store.
    /// 2. The first turn is accepted and committed inside the child session
    ///    under the ordinary session execution lease — never the process
    ///    registry.
    /// 3. Only the runner's caller records the process terminal. A committed
    ///    child turn is not itself a recorded process result.
    ///
    /// Residency (FIG-3424): the child runtime is owned by this run as an
    /// ordinary local — no registry, map, or cache retains it. A redelivery
    /// in a *new* worker run finds the durable row and reopens it through the
    /// ordinary open path; a redelivery *within* this run finds the owned
    /// value and skips initialisation entirely. The runtime is dropped when
    /// the run returns, on success, failure, or cancellation.
    ///
    /// Cancellation is the standard turn cancellation, not a teardown path:
    ///
    /// * Observed before the create commit, nothing is created. A cancelled
    ///   redelivery still reconciles first: a previous attempt may have
    ///   committed the child and accepted this turn's input before crashing,
    ///   so any open input scoped to this turn is durably settled before the
    ///   cancellation is reported.
    /// * Observed after the create commit but before turn admission, the
    ///   session is retained idle — an empty durable row that is never
    ///   reclaimed by lash.
    /// * Observed while the turn runs, the supplied token is the turn's own
    ///   cancellation token, so the turn settles `Cancelled` through the same
    ///   path as every other cancelled turn: the cancelled turn commits, the
    ///   accepted turn-input row is settled, and nothing remains claimable.
    ///   The child session stays durable and reusable; lash never deletes a
    ///   session because a process was cancelled.
    ///
    /// Settlement is the fence around process terminalization: a cancelled
    /// outcome is only ever returned once this turn's accepted child input is
    /// terminal and unclaimed. A reconciliation or commit failure surfaces as
    /// a retryable init error so the substrate keeps the process recoverable
    /// instead of writing a `Cancelled` terminal over an unsettled child.
    pub(in crate::runtime::session_manager) async fn initialize_session_and_run_turn(
        &self,
        create_request: crate::SessionCreateRequest,
        process_id: &crate::ProcessId,
        turn_id: TurnId,
        turn_input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
    ) -> Result<InitializedSessionTurn, SessionTurnInitError> {
        let requested_session_id = create_request.session_id.clone();
        if cancellation.is_cancelled() {
            self.settle_cancelled_process_child_inputs(
                requested_session_id.as_ref(),
                process_id,
                &turn_id,
            )
            .await
            .map_err(|source| SessionTurnInitError::Reconcile {
                session_id: requested_session_id.clone(),
                source: Box::new(source),
            })?;
            return Err(SessionTurnInitError::CancelledBeforeCreate);
        }
        // The child runtime is owned by this run. A redelivery within the run
        // finds it in this local — never in a registry — and skips
        // initialisation; a redelivery in a new run reopens the durable
        // session inside `initialize_session`.
        let mut child: Option<InitializedSession> = None;
        let session_id = match child.as_ref() {
            Some(initialized) => initialized.session_id.clone(),
            None => {
                let initialized = Box::pin(initialize_session(&self.current, create_request))
                    .await
                    .map_err(|source| SessionTurnInitError::Create {
                        session_id: requested_session_id.clone(),
                        source: Box::new(source),
                    })?;
                #[cfg(any(test, feature = "testing"))]
                spawned_children::record(&initialized.session_id, &initialized.handle);
                let session_id = initialized.session_id.clone();
                child = Some(initialized);
                session_id
            }
        };
        let child = child
            .as_ref()
            .map(|initialized| &initialized.handle)
            .ok_or_else(|| SessionTurnInitError::Create {
                session_id: Some(session_id.clone()),
                source: Box::new(crate::PluginError::Session(format!(
                    "process child session `{session_id}` was not initialized for this run"
                ))),
            })?;
        if cancellation.is_cancelled() {
            self.settle_cancelled_process_child_inputs(Some(&session_id), process_id, &turn_id)
                .await
                .map_err(|source| SessionTurnInitError::Reconcile {
                    session_id: Some(session_id.clone()),
                    source: Box::new(source),
                })?;
            return Err(SessionTurnInitError::CancelledAfterCreate { session_id });
        }
        let (turn_input, scoped_effect_controller) = validated_process_turn_input(
            &turn_id,
            turn_input,
            process_id,
            scoped_effect_controller,
        )
        .map_err(|source| SessionTurnInitError::Request {
            session_id: session_id.clone(),
            source: Box::new(source),
        })?;
        let turn = self
            .run_child_session_turn(
                child,
                &turn_id,
                turn_input,
                scoped_effect_controller,
                cancellation,
            )
            .await
            .map_err(|source| SessionTurnInitError::Turn {
                session_id: session_id.clone(),
                source: Box::new(source),
            })?;
        Ok(InitializedSessionTurn { session_id, turn })
    }

    /// Durably settle this turn's still-open input on the cancelled process's
    /// retained child session(s) without running a turn.
    ///
    /// This is the redelivery reconcile: a previous attempt may have
    /// committed the child session and accepted the input before crashing or
    /// observing the durable cancellation. Opening the recorded child (plus
    /// any session the catalog attributes to this process, covering an id a
    /// crashed attempt minted but never recorded) and cancelling every open
    /// row scoped to `turn_id` leaves terminal receipts behind and nothing
    /// claimable — the precondition for the caller to write a `Cancelled`
    /// process terminal.
    ///
    /// A row still held under a live session-execution-lease claim refuses
    /// cancellation and surfaces as an error: a live holder can still settle
    /// it, so the process stays recoverable rather than terminalizing over an
    /// input a survivor might complete.
    async fn settle_cancelled_process_child_inputs(
        &self,
        requested_session_id: Option<&SessionId>,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
    ) -> Result<(), crate::PluginError> {
        let Some(factory) = self.current.host.session_store_factory.as_ref() else {
            return Ok(());
        };
        let mut candidates: Vec<SessionId> = requested_session_id.cloned().into_iter().collect();
        match factory
            .list_sessions(&crate::SessionListFilter {
                caused_by: Some(crate::CausalRef::Process {
                    process_id: process_id.clone(),
                }),
                ..Default::default()
            })
            .await
        {
            Ok(summaries) => {
                for session_id in summaries.into_iter().map(|summary| summary.session_id) {
                    if !candidates.contains(&session_id) {
                        candidates.push(session_id);
                    }
                }
            }
            Err(crate::StoreError::UnsupportedStoreOperation { .. }) => {}
            Err(error) => {
                return Err(crate::PluginError::Session(format!(
                    "failed to enumerate sessions caused by cancelled process `{process_id}`: {error}"
                )));
            }
        }
        for session_id in candidates {
            self.settle_open_process_child_turn_input(
                factory.as_ref(),
                &session_id,
                process_id,
                turn_id,
            )
            .await?;
        }
        Ok(())
    }

    async fn settle_open_process_child_turn_input(
        &self,
        factory: &dyn crate::SessionStoreFactory,
        session_id: &SessionId,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
    ) -> Result<(), crate::PluginError> {
        let Some(store) = factory
            .open_existing_store_by_id(session_id)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to inspect cancelled process `{process_id}` child session `{session_id}`: {error}"
                ))
            })?
        else {
            return Ok(());
        };
        // A session the catalog attributes to this process exists solely to
        // run its turn, so every open row under it is the dead attempt's work.
        // A session not caused by this process is foreign — only rows scoped
        // to this exact turn may be touched.
        let owned_by_process = store
            .load_session_meta()
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to read cancelled process `{process_id}` child session `{session_id}` metadata: {error}"
                ))
            })?
            .is_some_and(|meta| {
                matches!(
                    &meta.relation,
                    crate::SessionRelation::Child {
                        caused_by: Some(crate::CausalRef::Process {
                            process_id: owner_process_id,
                        }),
                        ..
                    } if owner_process_id == process_id
                )
            });
        let pending = store
            .list_pending_turn_inputs(session_id)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to list cancelled process `{process_id}` child session `{session_id}` inputs: {error}"
                ))
            })?;
        let targets: Vec<crate::PendingTurnInputCancelTarget> = pending
            .iter()
            .filter(|read| {
                !read.input.state.is_terminal()
                    && (owned_by_process || read.input.state.active_turn_id() == Some(turn_id))
            })
            .map(|read| {
                crate::PendingTurnInputCancelTarget::input_id(read.input.input_id.to_string())
            })
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let receipts = store
            .cancel_pending_turn_inputs(session_id, &targets)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to settle cancelled process `{process_id}` child session `{session_id}` inputs: {error}"
                ))
            })?;
        for receipt in &receipts {
            if let crate::PendingTurnInputCancelOutcome::AlreadyClaimed { claim, .. } =
                &receipt.outcome
            {
                return Err(crate::PluginError::Session(format!(
                    "cancelled process `{process_id}` child session `{session_id}` still holds this turn's input under a live claim: {claim:?}"
                )));
            }
        }
        Ok(())
    }

    /// The shared child-turn drive: the owned runtime's single-writer lock,
    /// a fresh task stack for shareable controllers, the event drain, and the
    /// post-turn usage persistence. `cancel` is the process's token, so a
    /// cancelled process settles an ordinary cancelled turn inside the child.
    async fn run_child_session_turn(
        &self,
        child: &RuntimeHandle,
        turn_id: &TurnId,
        input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancel: CancellationToken,
    ) -> Result<AssembledTurn, crate::PluginError> {
        let (event_tx, mut event_rx) = mpsc::channel::<SessionStreamEvent>(100);
        let sink = ChannelEventSink { tx: event_tx };
        let event_drain =
            crate::task::spawn(async move { while event_rx.recv().await.is_some() {} });
        let turn = match scoped_effect_controller.into_static() {
            Ok(scoped_effect_controller) => {
                // Canonical recursion-growth seam: every shareable child turn
                // gets a fresh Tokio task stack here. Concurrency is bounded
                // upstream by the process worker's execution slots — one
                // process run owns at most one child turn — so no admission
                // registry or limit lives at this layer.
                let task = crate::task::spawn(
                    crate::runtime::process_permit::inherit_process_execution_permit(
                        run_initialized_session_turn(
                            child.clone(),
                            input,
                            cancel,
                            scoped_effect_controller,
                            sink.clone(),
                        ),
                    ),
                );
                let mut abort_on_drop = AbortTaskOnDrop::new(task.abort_handle());
                let joined = task.await;
                abort_on_drop.disarm();
                match joined {
                    Ok(turn) => turn,
                    Err(err) if err.is_panic() => child_turn_panicked(err.into_panic()),
                    Err(err) => Err(crate::PluginError::Session(format!(
                        "child session turn task was cancelled: {err}"
                    ))),
                }
            }
            Err(scoped_effect_controller) => {
                // Handler-scoped durable controllers cannot outlive their host
                // invocation and therefore cannot cross Tokio's `'static`
                // spawn contract. Preserve their exact journal semantics by
                // retaining the scoped controller on the calling task.
                run_initialized_session_turn(
                    child.clone(),
                    input,
                    cancel,
                    scoped_effect_controller,
                    sink.clone(),
                )
                .await
            }
        };
        drop(sink);
        let _ = event_drain.await;
        Box::pin(
            self.usage
                .persist_current_usage_ledger(&self.current, turn_id),
        )
        .await?;
        turn
    }
}

/// Where [`RuntimeSessionServices::initialize_session_and_run_turn`] stopped.
///
/// The variants partition the operation so the process runner can report the
/// stage faithfully without inspecting error strings.
pub(in crate::runtime::session_manager) enum SessionTurnInitError {
    /// Cancellation was observed before the create commit; this attempt
    /// created nothing. Any input a previous attempt left open under this
    /// turn was durably settled before this error was returned.
    CancelledBeforeCreate,
    /// Cancellation was observed in the window between the create commit and
    /// turn admission (or after the turn settled). The session is committed
    /// and retained; nothing this turn accepted remains claimable.
    CancelledAfterCreate { session_id: SessionId },
    /// Session initialization failed.
    ///
    /// `session_id` is the child's recorded identity when the durable
    /// request fixed it. Creation is multi-stage — the durable catalog row
    /// and the runtime commit can land before a later stage fails — so a
    /// `Some` here means a retained session *may* already exist for that id
    /// even though no runtime was initialized. `None` means only that the
    /// request named no id, not that nothing was committed.
    Create {
        session_id: Option<SessionId>,
        source: Box<crate::PluginError>,
    },
    /// The process's execution authority did not validate for the child turn.
    Request {
        session_id: SessionId,
        source: Box<crate::PluginError>,
    },
    /// The first turn itself failed to run to a committed outcome — including
    /// a failed final commit, which can leave the accepted input open for a
    /// later attempt to recover.
    Turn {
        session_id: SessionId,
        source: Box<crate::PluginError>,
    },
    /// Cancellation was observed but reconciling the retained child's durable
    /// input failed, so this turn's accepted input may still be open. The
    /// process must stay recoverable: terminalizing it now would strand a
    /// claimable input inside the retained session.
    Reconcile {
        session_id: Option<SessionId>,
        source: Box<crate::PluginError>,
    },
}

impl SessionTurnInitError {
    /// The child session id the operation could have left durable state for —
    /// either a session it provably retained or, for `Create`/`Reconcile`,
    /// the recorded identity whose catalog row may exist even though the
    /// failure carried no runtime handle. `None` means the request named no
    /// session, not that nothing was committed.
    pub(in crate::runtime::session_manager) fn retained_session_id(&self) -> Option<&SessionId> {
        match self {
            Self::CancelledAfterCreate { session_id }
            | Self::Request { session_id, .. }
            | Self::Turn { session_id, .. } => Some(session_id),
            Self::Create { session_id, .. } | Self::Reconcile { session_id, .. } => {
                session_id.as_ref()
            }
            Self::CancelledBeforeCreate => None,
        }
    }
}

/// The process-backed turn-input validation: the child's turn keeps the
/// process scope it was admitted under, a non-empty durable turn id, and a
/// `trace_turn_id` stamped to that id.
fn validated_process_turn_input<'run>(
    turn_id: &TurnId,
    mut input: crate::TurnInput,
    process_id: &crate::ProcessId,
    scoped_effect_controller: crate::ScopedEffectController<'run>,
) -> Result<(crate::TurnInput, crate::ScopedEffectController<'run>), crate::PluginError> {
    let required_scope = crate::ExecutionScope::process(process_id);
    if scoped_effect_controller.execution_scope() != &required_scope {
        return Err(crate::PluginError::Session(format!(
            "process-backed session turn `{turn_id}` requires execution scope {required_scope:?}"
        )));
    }
    if turn_id.trim().is_empty() {
        return Err(crate::PluginError::Session(
            "session turns require a non-empty stable turn id".to_string(),
        ));
    }
    if let Some(input_turn_id) = input.trace_turn_id.as_deref()
        && input_turn_id != turn_id
    {
        return Err(crate::PluginError::Session(format!(
            "input trace_turn_id `{input_turn_id}` does not match turn id `{turn_id}`"
        )));
    }
    input.trace_turn_id = Some(turn_id.clone());
    Ok((input, scoped_effect_controller))
}

fn child_turn_panicked(
    payload: Box<dyn std::any::Any + Send>,
) -> Result<AssembledTurn, crate::PluginError> {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let failure = Err(crate::PluginError::Session(format!(
        "child_turn_panicked: {message}"
    )));
    crate::panic_containment::enforce_loudness(payload);
    failure
}

async fn run_initialized_session_turn(
    runtime: RuntimeHandle,
    input: crate::TurnInput,
    cancel: CancellationToken,
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    sink: ChannelEventSink,
) -> Result<AssembledTurn, crate::PluginError> {
    // This mutex is the child runtime's single-writer boundary. Hold it for
    // the complete turn and publish from the guarded post-turn state before
    // releasing it.
    let mut runtime_guard = runtime.runtime.lock().await;
    let scoped_effect_controller = match scoped_effect_controller.execution_scope() {
        crate::ExecutionScope::Turn { turn_id, .. } => scoped_effect_controller
            .rescope(
                crate::AdmittedScope::unpinned(runtime_guard.state.turn_scope(turn_id.clone()))
                    .map_err(|err| crate::PluginError::Session(err.to_string()))?,
            )
            .map_err(crate::PluginError::Runtime)?,
        crate::ExecutionScope::Process { .. } => scoped_effect_controller,
        scope => {
            return Err(crate::PluginError::Session(format!(
                "child session turns require a turn or process execution scope, got {scope:?}"
            )));
        }
    };
    let options =
        crate::runtime::TurnOptions::new(cancel, scoped_effect_controller).with_events(&sink);
    let result = runtime_guard
        .stream_turn_with_agent_frames(input, options)
        .await
        .map_err(crate::PluginError::Runtime)
        .and_then(|run| {
            run.into_final_turn().ok_or_else(|| {
                crate::PluginError::Session("agent frame run completed without a turn".to_string())
            })
        });
    runtime.publish_from(&runtime_guard);
    result
}

struct AbortTaskOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortTaskOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

/// Test seam (FIG-3424): a `Weak` to every child runtime session
/// initialisation mints, keyed by the child's session id, so tests can prove
/// the run dropped its owner — after the run completes, the entry for that
/// session no longer upgrades. Production carries nothing; the process child
/// has no registry to leak into.
#[cfg(any(test, feature = "testing"))]
mod spawned_children {
    use super::*;
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex;

    static SPAWNED: StdMutex<Vec<(SessionId, std::sync::Weak<Mutex<LashRuntime>>)>> =
        StdMutex::new(Vec::new());

    pub fn record(session_id: &SessionId, handle: &RuntimeHandle) {
        SPAWNED
            .lock_recover()
            .push((session_id.clone(), Arc::downgrade(&handle.runtime)));
    }

    pub fn take() -> Vec<(SessionId, std::sync::Weak<Mutex<LashRuntime>>)> {
        std::mem::take(&mut *SPAWNED.lock_recover())
    }
}

/// `(session id, Weak)` pairs for every process-spawned child `LashRuntime`
/// minted since the last read; drains the seam so each run's children are
/// attributed to the run that produced them. Empty in production builds.
#[cfg(any(test, feature = "testing"))]
pub fn take_spawned_child_runtimes()
-> Vec<(SessionId, std::sync::Weak<tokio::sync::Mutex<LashRuntime>>)> {
    spawned_children::take()
}

#[cfg(test)]
mod tests {
    #[test]
    fn contained_child_turn_panic_is_loud_in_test_builds() {
        let previous = crate::panic_containment::set_loud(true);
        let panic = std::panic::catch_unwind(|| {
            let _ = super::child_turn_panicked(Box::new("child turn remains loud"));
        });
        crate::panic_containment::set_loud(previous);
        assert!(panic.is_err());
    }
}
