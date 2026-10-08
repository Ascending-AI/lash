//! Session initialisation: the `SessionCreateRequest` pipeline that resolves,
//! materializes, and durably commits a new ordinary session, plus the
//! process-origin port that initializes a recorded child and executes its first
//! turn through the shared session-turn path.
//!
//! This is one of two session-construction APIs, kept deliberately separate
//! (ADR 0089): initialisation builds a *new* session from exactly what its
//! request states — policy, relation, tool access, prompt plan, plugin
//! options, initial nodes and observer intents — and reads no other session
//! (ADR 0134). Catalog `SessionCatalogStore::fork_session` is the one clone:
//! it materializes durable fork lineage and retained-frame content with
//! different failure semantics; neither API covers the other.
//!
//! Run-scoped child residency (FIG-3424): the child runtime a
//! `ProcessInput::SessionTurn` initializes is owned by that process run, held
//! as a local for the run's duration, and dropped when the run completes —
//! success, failure, or cancellation. Nothing caches it. A redelivery in a
//! new run reopens the session's durable row through the ordinary open path;
//! the durable row is the only continuity between attempts.

use super::*;
use crate::TurnId;
use crate::plugin::PluginSessionRequest;
use crate::runtime::host::EmbeddedRuntimeHost;

/// A create request resolved into everything materialization needs. Nothing
/// on the plan reads another session: the initial runtime state is fully
/// built here from the request.
pub(in crate::runtime::session_manager) struct SessionInitPlan {
    session_id: SessionId,
    relation: SessionRelation,
    pending_observer_intents: Vec<crate::SessionObserverIntent>,
    policy: SessionPolicy,
    initial_runtime_state: RuntimeSessionState,
    plugin_config: crate::plugin::SessionAuthorityContext,
    /// The `SessionTurn` process whose start creates this session, recorded
    /// on the session's metadata as its owner (FIG-3607 R1). `None` for a
    /// host create.
    owning_process_id: Option<crate::ProcessId>,
}

/// The resolved request with its runtime assembled but not yet committed.
struct MaterializedSession {
    runtime: LashRuntime,
    store_binding: crate::store::SessionStore,
}

/// The child a `ProcessInput::SessionTurn` run owns: an ordinary session
/// runtime, created or reopened, held only for the run's duration.
pub(in crate::runtime::session_manager) struct InitializedSession {
    pub(in crate::runtime::session_manager) handle: RuntimeHandle,
    pub(in crate::runtime::session_manager) session_id: SessionId,
}

pub(in crate::runtime::session_manager) async fn resolve_session_init(
    current: &CurrentOwnerCapability,
    request: SessionCreateRequest,
) -> Result<SessionInitPlan, crate::PluginError> {
    let (session_id, request) = identified_create_request(request)?;
    let facts = resolve_child_facts(&StarterFacts::of(current), &request, &session_id)?;
    plan_session_init(current, request, session_id, facts)
}

/// The plan that finishes a partial create from the config its admission
/// recorded (FIG-4627). `created` is the session's revision-zero head: the
/// complete config a previous attempt resolved and wrote with the catalog
/// row. It is authoritative, so nothing it records is resolved again: no
/// model key is minted and no plugin owner creates a namespace. Only what
/// the head does not record comes from the recorded request: the initial
/// nodes and the observer intents.
fn recorded_creation_plan(
    current: &CurrentOwnerCapability,
    request: SessionCreateRequest,
    created: &RuntimeSessionState,
) -> Result<SessionInitPlan, crate::PluginError> {
    let (session_id, mut request) = identified_create_request(request)?;
    let policy = created.policy.clone();
    request.tool_access = created.authority.tool_access.clone();
    request.prompt_plan = Some(created.authority.prompt_plan.clone());
    let facts = ChildFacts {
        policy,
        plugin_config: created.authority.plugin_config.clone(),
    };
    plan_session_init(current, request, session_id, facts)
}

/// `request` with its session identified: the id it names, or a minted one.
/// A start point initialisation does not admit is refused.
fn identified_create_request(
    mut request: SessionCreateRequest,
) -> Result<(SessionId, SessionCreateRequest), crate::PluginError> {
    let session_id = request
        .session_id
        .take()
        .unwrap_or_else(|| SessionId::from_uuid(uuid::Uuid::new_v4().as_u128()));
    request.session_id = Some(session_id.clone());
    // `SessionStartPoint` keeps predecessor variants only so durable and
    // remote payloads still decode; `Empty` is the only start point
    // initialisation admits. A recorded request carrying anything else can
    // never run — refuse it here so the refusal is typed rather than a
    // silent reinterpretation.
    if !matches!(request.start, crate::SessionStartPoint::Empty) {
        return Err(crate::PluginError::Session(format!(
            "session `{session_id}` create request carries a start point initialisation does not admit; snapshot starts were removed with the managed-session machinery (FIG-3378)"
        )));
    }
    Ok((session_id, request))
}

/// Build the plan that creates `session_id` with `facts`: the facts
/// `request` resolved to, or the ones its admission already recorded.
fn plan_session_init(
    current: &CurrentOwnerCapability,
    mut request: SessionCreateRequest,
    session_id: SessionId,
    facts: ChildFacts,
) -> Result<SessionInitPlan, crate::PluginError> {
    if let Some(plan) = request.prompt_plan.as_ref() {
        plan.validate().map_err(|error| {
            crate::PluginError::Runtime(crate::RuntimeError::session_config_refused(
                &session_id,
                crate::CoreConfigOwner::creation_refusal(
                    crate::CoreConfigRefusal::PromptPlanRefused { error },
                ),
            ))
        })?;
    }
    let ChildFacts {
        policy,
        plugin_config: recorded_plugin_config,
    } = facts;
    // Every session initializes empty: `SessionStartPoint::Empty` is the only
    // start point initialisation admits. Durable forks and resumed sessions
    // get their state from the store, not from the create request.
    let start_state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(policy.clone())
    };
    // The child records the minted binding in its policy; the request's key
    // has done its work.
    request.model = None;
    request.policy = Some(policy.clone());
    let initial_runtime_state = build_runtime_state(
        session_id.clone(),
        &request,
        start_state,
        &policy,
        recorded_plugin_config,
        current.host.core.clock.as_ref(),
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let plugin_config = crate::plugin::SessionAuthorityContext {
        tool_access: request.tool_access.clone(),
        plugin_config: initial_runtime_state.admitted_plugin_config(),
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
        policy,
        initial_runtime_state,
        plugin_config,
        owning_process_id: None,
    })
}

/// What a session's creation resolves against: the plugin set and models the
/// deployment installs. Nothing of the session that starts the creation
/// stands in for what its request leaves unstated (ADR 0134).
pub(in crate::runtime::session_manager) struct StarterFacts<'a> {
    pub(in crate::runtime::session_manager) plugin_host: &'a crate::PluginHost,
    pub(in crate::runtime::session_manager) protocol_plugin_id: Option<&'a str>,
    /// The plugin admission the starter runs under (FIG-4747): a session it
    /// creates records its namespaces in the same formats. Empty when the
    /// starter adopted none, and each plugin then writes its native format.
    pub(in crate::runtime::session_manager) plugin_admission:
        crate::store::plugin_writers::PluginAdmission,
    /// Mints the binding of a model key the create request names.
    pub(in crate::runtime::session_manager) models: &'a dyn crate::LlmProfiles,
}

impl<'a> StarterFacts<'a> {
    /// The deployment facts of the runtime `current` serves.
    pub(in crate::runtime::session_manager) fn of(current: &'a CurrentOwnerCapability) -> Self {
        Self {
            plugin_host: current.plugins.host(),
            protocol_plugin_id: current.plugins.host().protocol_plugin_id(),
            plugin_admission: current.plugins.plugin_admission().unwrap_or_default(),
            models: current.host.core.providers.models.as_ref(),
        }
    }
}

/// What a created session records: its complete policy and the plugin
/// configuration every installed owner resolved for it.
pub(in crate::runtime::session_manager) struct ChildFacts {
    pub(in crate::runtime::session_manager) policy: SessionPolicy,
    pub(in crate::runtime::session_manager) plugin_config: crate::PluginConfig,
}

/// Resolve the complete facts `request` creates `session_id` with: the
/// policy it states and the plugin configuration every installed owner
/// creates from its stated options. Creation is explicit (ADR 0134): a
/// relation is lineage only, and no other session's policy, model, reasoning
/// or namespace fills what the request leaves unstated.
///
/// A request that states no policy is refused with
/// [`CoreConfigRefusal::PolicyUnstated`](crate::CoreConfigRefusal::PolicyUnstated)
/// inside
/// [`RuntimeErrorCode::SessionConfigRefused`](crate::RuntimeErrorCode::SessionConfigRefused).
/// A request that names a model key of its own mints it, here, through the
/// deployment's models, with the reasoning the request states or the
/// default selection; otherwise the policy's recorded model is kept as
/// recorded. A key they do not register is refused with
/// [`RuntimeErrorCode::LlmProfileUnknown`](crate::RuntimeErrorCode::LlmProfileUnknown),
/// and a reasoning the minted model's capability refuses with
/// [`RuntimeErrorCode::ReasoningRefused`](crate::RuntimeErrorCode::ReasoningRefused).
/// A namespace no installed owner registers, or a value its owner refuses, is
/// [`RuntimeErrorCode::SessionConfigRefused`](crate::RuntimeErrorCode::SessionConfigRefused)
/// with the refusal typed as its cause
/// ([`RuntimeErrorCause::ConfigRefused`](crate::RuntimeErrorCause::ConfigRefused)):
/// this deployment's plugin set cannot run the session, on any attempt.
pub(in crate::runtime::session_manager) fn resolve_child_facts(
    starter: &StarterFacts<'_>,
    request: &SessionCreateRequest,
    session_id: &SessionId,
) -> Result<ChildFacts, crate::PluginError> {
    let Some(mut policy) = request.policy.clone() else {
        return Err(crate::PluginError::Runtime(
            crate::RuntimeError::session_config_refused(
                session_id,
                crate::CoreConfigOwner::creation_refusal(crate::CoreConfigRefusal::PolicyUnstated),
            ),
        ));
    };
    if let Some(key) = request.model.as_ref() {
        let recorded = starter.models.snapshot(key).map_err(|source| {
            crate::PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::LlmProfileUnknown,
                crate::SessionError::LlmProfileUnknown {
                    session_id: session_id.clone(),
                    source,
                }
                .to_string(),
            ))
        })?;
        let model = crate::LlmProfileConfig {
            model: recorded,
            reasoning: request.reasoning.clone().unwrap_or_default(),
        };
        // The reasoning the request states is judged against the capability
        // of the model its key minted (FIG-4531).
        model.validate_reasoning().map_err(|refused| {
            crate::PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ReasoningRefused,
                format!("session `{session_id}` create request is refused: {refused}"),
            ))
        })?;
        policy.model = Some(model);
    }
    // Every installed owner creates its namespace from the request's options
    // over its own defaults (FIG-4379). The creation head records the result.
    let plugin_config = starter
        .plugin_host
        .resolve_creation_plugin_config(
            starter.protocol_plugin_id,
            &request.plugin_options,
            &starter.plugin_admission,
        )
        .map_err(|error| match error {
            crate::CreationConfigError::Format(refusal) => crate::PluginError::Format(refusal),
            crate::CreationConfigError::Refused(refusal) => crate::PluginError::Runtime(
                crate::RuntimeError::session_config_refused(session_id, refusal),
            ),
            crate::CreationConfigError::RecordedCorrupt(corrupt) => {
                crate::PluginError::from(corrupt.into_store_error())
            }
            crate::CreationConfigError::Registration(error) => {
                crate::PluginError::ConfigRegistration(error)
            }
        })?;
    Ok(ChildFacts {
        policy,
        plugin_config,
    })
}

/// Admit a session-turn start's child before its worker handoff
/// (FIG-4396): its complete facts resolve on this deployment's plugin set,
/// the set every worker of its engine binding installs. A request this
/// plugin set cannot create is refused here, before the start is
/// registered.
pub(in crate::runtime::session_manager) fn admit_session_turn_child(
    current: &CurrentOwnerCapability,
    create_request: &SessionCreateRequest,
    start_name: &str,
) -> Result<(), crate::PluginError> {
    // A child whose request names no session takes the id the worker
    // derives from the minted process id (ADR 0107); before registration it
    // only names a refusal.
    let session_id = create_request
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::prefixed("the child of ", start_name));
    resolve_child_facts(&StarterFacts::of(current), create_request, &session_id).map(|_| ())
}

/// Whether `error` is the reasoning refusal [`resolve_child_facts`] mints:
/// the recorded request's reasoning does not fit the model its key mints.
fn reasoning_refused(error: &crate::PluginError) -> bool {
    matches!(
        error,
        crate::PluginError::Runtime(runtime)
            if runtime.code == crate::RuntimeErrorCode::ReasoningRefused
    )
}

/// Whether `error` is the creation refusal [`resolve_child_facts`] mints.
pub(in crate::runtime::session_manager) fn session_config_refused(
    error: &crate::PluginError,
) -> bool {
    matches!(
        error,
        crate::PluginError::Runtime(runtime)
            if runtime.code == crate::RuntimeErrorCode::SessionConfigRefused
    )
}

fn build_runtime_state(
    session_id: SessionId,
    request: &SessionCreateRequest,
    mut base: RuntimeSessionState,
    policy: &SessionPolicy,
    plugin_config: crate::PluginConfig,
    clock: &dyn crate::Clock,
) -> Result<RuntimeSessionState, crate::StoreError> {
    base.session_id = session_id;
    base.head_revision = 0;
    base.checkpoint_components.complete_for_new_session()?;
    // The child captures its own live namespaces at its first boundary.
    base.set_plugin_state(None);
    base.clear_plugin_admission_snapshot();
    base.policy = policy.clone();
    base.authority.tool_access = request.tool_access.clone();
    base.authority.prompt_plan = request.prompt_plan.clone().unwrap_or_default();
    base.session_graph = crate::SessionGraph::default();
    base.agent_frames.clear();
    base.current_frame_node_id = None;
    base.persisted_node_ids.clear();
    base.reset_initial_agent_frame_with_clock(
        crate::AgentFrameAssignment::new(policy.clone(), plugin_config),
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
    current: &CurrentOwnerCapability,
    plan: &SessionInitPlan,
) -> Result<MaterializedSession, crate::PluginError> {
    let plugins = current
        .plugins
        .host()
        .defer_session(PluginSessionRequest::creation(
            &plan.session_id,
            plan.plugin_config.clone(),
        ))?;
    let initial_state = plan.initial_runtime_state.clone();
    let store_binding = bind_session_store(current, plan).await?;
    // Session creation routes through the same assembler as live open and
    // worker-rebuild paths. A freshly created session has a single path, so it
    // materializes under KeepAll (residency trimming is an open-time concern).
    let runtime = LashRuntime::assemble_runtime(
        plan.policy.clone(),
        embedded_host(current),
        plugins,
        crate::runtime::lifecycle::RuntimePersistenceBindings::new(Some(store_binding.clone())),
        current.host.work.clone(),
        crate::runtime::lifecycle::RuntimeSessionAssembly::new(
            initial_state,
            current.runtime_lease_owner.clone(),
        ),
    )
    .await
    .map_err(|err| match err {
        crate::SessionError::Plugin(error) => error,
        error => crate::PluginError::Session(error.to_string()),
    })?;

    Ok(MaterializedSession {
        runtime,
        store_binding,
    })
}

/// Admit the session's catalog row with the config its creation resolved
/// baked in as the created head, in the catalog's own transaction
/// (FIG-4553). A creator that dies before its first commit leaves a session
/// that opens with exactly this config: the complete config that commit
/// writes, taken from the same initial state.
async fn bind_session_store(
    current: &CurrentOwnerCapability,
    plan: &SessionInitPlan,
) -> Result<crate::store::SessionStore, crate::PluginError> {
    let catalog = current.host.core.session_store_factory();
    let request = SessionStoreCreateRequest {
        session_id: plan.session_id.clone(),
        relation: plan.relation.clone(),
        pending_observer_intents: plan.pending_observer_intents.clone(),
        config: crate::store::persisted_session_config_from_state(&plan.initial_runtime_state),
        head: crate::SessionCreationHead::Config,
        owning_process_id: plan.owning_process_id.clone(),
    };
    let creation_error = |error: crate::StoreError| {
        crate::PluginError::Session(session_creation_store_factory_error(
            &plan.session_id,
            error.to_string(),
        ))
    };
    // The created head is written outside any runtime commit, so it is
    // measured against the commit budget here (FIG-4393).
    crate::store::admit_created_session(
        catalog.as_ref(),
        &request,
        current.host.core.durability.commit_budget,
        catalog.fleet_format(),
    )
    .await
    .map_err(creation_error)?;
    let runtime: Arc<dyn crate::store::RuntimeStore> = catalog;
    let store = crate::store::SessionStore::new(runtime, plan.session_id.clone())
        .map_err(creation_error)?;
    validate_created_session_store_binding(&store, &plan.session_id).await?;
    Ok(store)
}

fn embedded_host(current: &CurrentOwnerCapability) -> EmbeddedRuntimeHost {
    EmbeddedRuntimeHost::new(current.host.core.clone())
}

fn session_creation_store_guidance() -> &'static str {
    "A session-creation factory must return a distinct store bound to the requested session id. \
     A backend's session catalog opens one store per session; do not stand a single pre-opened \
     store in for the catalog."
}

fn session_creation_store_factory_error(session_id: &SessionId, message: String) -> String {
    format!(
        "failed to create store for session `{session_id}`: {message}. {}",
        session_creation_store_guidance()
    )
}

async fn validate_created_session_store_binding(
    store: &crate::store::SessionStore,
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
    current: &CurrentOwnerCapability,
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
            operation,
            materialized.runtime.host.core.durability.commit_budget,
            materialized.runtime.fleet_format(),
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
    let result = materialized
        .store_binding
        .commit_runtime_state_verified(commit, materialized.runtime.host.core.tracing.metrics())
        .await
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    persisted_state.apply_persisted_commit_result(result);
    persisted_state.mark_node_ids_persisted(persisted_node_ids);
    materialized
        .runtime
        .install_resident_state(persisted_state)?;
    let observed_processes =
        settle_session_observer_intents(current, &plan.session_id, &materialized.store_binding)
            .await?;
    let handle = SessionHandle {
        session_id: plan.session_id,
        parent_session_id: plan.relation.parent_session_id().map(SessionId::from),
        policy: plan.policy,
        observed_processes,
    };
    Ok((handle, RuntimeHandle::new(materialized.runtime)))
}

/// Settle the observer intents a create request recorded into the new
/// session's durable row. Idempotent and durably recoverable, so a reopen
/// after a crashed create attempt completes it the same way.
async fn settle_session_observer_intents(
    current: &CurrentOwnerCapability,
    session_id: &SessionId,
    store: &crate::store::SessionStore,
) -> Result<Vec<crate::plugin::SessionObservedProcessReceipt>, crate::PluginError> {
    let observer_intent_source =
        crate::runtime::SessionObserverIntentSource::Persisted(store.store().as_ref());
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

/// Whether a session-init failure is the catalog's typed "no by-id lookup"
/// refusal rather than a transient inspect failure. `durable_session_store`
/// mints the code; this is the single consumer-side recognizer.
fn session_catalog_lookup_unsupported(error: &crate::PluginError) -> bool {
    matches!(
        error,
        crate::PluginError::Runtime(runtime)
            if runtime.code == crate::RuntimeErrorCode::SessionCatalogLookupUnsupported
    )
}

/// The session's durable store when its catalog row already exists.
async fn durable_session_store(
    current: &CurrentOwnerCapability,
    session_id: &SessionId,
) -> Result<Option<crate::store::SessionStore>, crate::PluginError> {
    crate::runtime::live_session_view(&current.host.core.session_store_factory(), session_id)
        .await
        .map_err(|error| match error {
            // A catalog that cannot resolve a session by id can never serve
            // the session this call names: the refusal is a deployment fact,
            // not a transient miss. Carrying it as a typed, terminal code —
            // not an ordinary `PluginError::Session` lookup failure — is what
            // lets the `SessionTurn` caller refuse rather than re-admit the
            // process forever (FIG-3487).
            crate::StoreError::UnsupportedStoreOperation { .. } => {
                crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::SessionCatalogLookupUnsupported,
                    format!(
                        "failed to inspect session `{session_id}` before initialisation: {error}"
                    ),
                ))
            }
            error => crate::PluginError::of_store_error(
                format_args!("failed to inspect session `{session_id}` before initialisation"),
                error,
            ),
        })
}

/// `SessionLifecycleService::create_session`: a host's fresh create. The
/// durable row must not already exist — replaying a recorded identity is the
/// process run port's contract, not a host create's.
pub(in crate::runtime::session_manager) async fn create_session(
    current: &CurrentOwnerCapability,
    request: SessionCreateRequest,
) -> Result<SessionHandle, crate::PluginError> {
    let plan = resolve_session_init(current, request).await?;
    if durable_session_store(current, &plan.session_id)
        .await?
        .is_some()
    {
        return Err(crate::PluginError::SessionAlreadyExists {
            session_id: plan.session_id,
        });
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
///
/// A child whose durable row records a head is read from it before the
/// request is resolved against anything this deployment installs (FIG-4531,
/// FIG-4627): the session runs what it recorded, so a redelivery never reads
/// today's catalog or plugin defaults for it. A committed head reopens; a
/// created head at revision zero finishes its create from the config it
/// records. Only a child with no recorded head resolves the recorded
/// request, and a model key this worker does not serve on that path is the
/// typed, retryable `LlmProfileUnavailable`: the start was admitted with the key,
/// so the deployment is at fault, never the request.
async fn initialize_session(
    current: &CurrentOwnerCapability,
    request: SessionCreateRequest,
    owning_process_id: &crate::ProcessId,
) -> Result<InitializedSession, crate::PluginError> {
    if let Some(session_id) = request.session_id.as_ref()
        && let Some(store) = durable_session_store(current, session_id).await?
        && let Some(state) = recorded_session_state(session_id, &request.relation, &store).await?
    {
        if state.head_revision > 0 {
            return reopen_committed_session(current, session_id, store, state).await;
        }
        // A head at revision zero is a partial create: a previous attempt
        // admitted the row, with the creation's complete config as its
        // created head, and crashed before the initial head landed. That
        // config is recorded, so it finishes the create (FIG-4627): the
        // admission is idempotent and the head commit publishes over it.
        let mut plan = recorded_creation_plan(current, request, &state)?;
        plan.owning_process_id = Some(owning_process_id.clone());
        return Box::pin(commit_fresh_session_init(current, plan)).await;
    }
    // No session, or a catalog row that records no head: nothing is recorded
    // for the session yet, so the recorded request creates it.
    let named = request.model.clone();
    let mut plan = resolve_session_init(current, request)
        .await
        .map_err(|error| unserved_profile_key(named.as_ref(), error))?;
    plan.owning_process_id = Some(owning_process_id.clone());
    Box::pin(commit_fresh_session_init(current, plan)).await
}

/// An admitted start's model key this worker's models do not register: the
/// start's admission minted nothing but judged the key, so a worker that
/// cannot mint it is a deployment that does not serve it. Retried, typed
/// `LlmProfileUnavailable` naming the key, as a run's per-run key in the same
/// position is.
fn unserved_profile_key(
    key: Option<&crate::LlmProfileKey>,
    error: crate::PluginError,
) -> crate::PluginError {
    match (key, error) {
        (Some(key), crate::PluginError::Runtime(runtime))
            if runtime.code == crate::RuntimeErrorCode::LlmProfileUnknown =>
        {
            crate::PluginError::Runtime(
                crate::RuntimeEffectControllerError::llm_profile_unavailable(
                    key,
                    format!(
                        "the start's model key is not served by this worker; the process \
                         retries until a deployment serves it: {}",
                        runtime.message
                    ),
                )
                .into_runtime_error(),
            )
        }
        (_, error) => error,
    }
}

/// A session error from assembling a recorded session's runtime, with a
/// store error classified as every read of the reopen is. Nothing on that
/// path binds a model (FIG-4404), so the one model refusal it can meet is a
/// recorded head that selects none: the terminal `LlmProfileUnconfigured`.
fn recorded_session_error(error: crate::SessionError) -> crate::PluginError {
    match error {
        crate::SessionError::Plugin(error) => error,
        crate::SessionError::Store { context, source } => {
            crate::PluginError::of_store_error(context, source)
        }
        error @ crate::SessionError::LlmProfileUnconfigured { .. } => crate::PluginError::Runtime(
            crate::runtime::turn_config::llm_profile_unconfigured(error),
        ),
        error => crate::PluginError::Session(error.to_string()),
    }
}

/// The fresh-create half of `initialize_session`: materialize the plan and
/// commit its initial head, then hand the ordinary runtime to the caller.
async fn commit_fresh_session_init(
    current: &CurrentOwnerCapability,
    plan: SessionInitPlan,
) -> Result<InitializedSession, crate::PluginError> {
    let materialized = materialize_session_init(current, &plan).await?;
    let (handle_view, handle) =
        Box::pin(commit_initialized_session(current, plan, materialized)).await?;
    Ok(InitializedSession {
        handle,
        session_id: handle_view.session_id,
    })
}

/// Inspect the durable session behind an existing catalog row for a
/// redelivery: replay-check the recorded relation, then load the head the
/// row records. A head at revision `0` is the created head an admission
/// wrote before a crash kept the initial head commit from landing. `None`
/// means the row records no head at all.
async fn recorded_session_state(
    session_id: &SessionId,
    relation: &crate::SessionRelation,
    store: &crate::store::SessionStore,
) -> Result<Option<crate::RuntimeSessionState>, crate::PluginError> {
    // The durable row decides lineage. A redelivery replays the same recorded
    // request, so a recorded relation that disagrees is a conflict, never a
    // silent adopt.
    let meta = store.load_session_meta().await.map_err(|error| {
        crate::PluginError::of_store_error(
            format_args!("failed to inspect session `{session_id}` before reopen"),
            error,
        )
    })?;
    if let Some(meta) = &meta
        && meta.relation != *relation
    {
        return Err(crate::PluginError::Session(format!(
            "session `{}` already exists with relation {:?}; a redelivery must replay its recorded relation, not {:?}",
            session_id, meta.relation, relation
        )));
    }
    crate::store::load_session_window_state(store, crate::store::WindowSelector::Current)
        .await
        .map(|loaded| loaded.map(|loaded| loaded.state))
        // A refusal to read the recorded child is the store's answer on
        // every attempt: it stays typed, so the process ends with it instead
        // of retrying a reopen no attempt can make (FIG-4628).
        .map_err(|error| {
            crate::PluginError::of_store_error(
                format_args!("failed to load session `{session_id}` for reopen"),
                error,
            )
        })
}

/// The reopen half of `initialize_session` against an already-loaded durable
/// head: rebuild the plugin session from the recorded state and assemble the
/// runtime under the resumed-session assembly.
async fn reopen_committed_session(
    current: &CurrentOwnerCapability,
    session_id: &SessionId,
    store: crate::store::SessionStore,
    state: crate::RuntimeSessionState,
) -> Result<InitializedSession, crate::PluginError> {
    // The reopened session runs the configuration it recorded, never the
    // redelivered request's (FIG-4379).
    let authority = crate::plugin::SessionAuthorityContext {
        tool_access: state.authority.tool_access.clone(),
        plugin_config: state.admitted_plugin_config(),
    };
    let plugin_host = current.plugins.host();
    let plugins = match state.plugin_state() {
        Some(snapshot) => plugin_host.defer_session(PluginSessionRequest::rematerialization(
            state.session_id.clone(),
            snapshot,
            authority,
        )),
        None => plugin_host.defer_session(PluginSessionRequest::creation(
            state.session_id.clone(),
            authority,
        )),
    }?;
    let policy = state.effective_policy().clone();
    let runtime = LashRuntime::assemble_runtime(
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
    .map_err(recorded_session_error)?;
    // Finish any observer intents a crashed create attempt left pending; the
    // settle is durable and idempotent, so completing it here is the same
    // work the create path performs.
    settle_session_observer_intents(current, session_id, &store).await?;
    Ok(InitializedSession {
        handle: RuntimeHandle::new(runtime),
        session_id: session_id.clone(),
    })
}

impl RuntimeSessionServices {
    /// Initialize a `ProcessInput::SessionTurn`'s child session and mail its
    /// turn's input to it (FIG-5208).
    ///
    /// This is the process-origin initialization port (FIG-3377): the only
    /// caller is the process's `mail_process_session_turn`, which hands over
    /// the durable `SessionCreateRequest` recorded on the process row. No
    /// facade or worker type reaches this layer.
    ///
    /// 1. The child session's create commit lands durably in the child
    ///    store, or a repeat reopens the session an earlier pass created.
    /// 2. The turn's input is accepted into the child as a `NextTurn` pending
    ///    input whose source key is `turn_id`: the session actor's drain
    ///    admits it as run `turn_id`, and the acceptance's transaction wakes
    ///    the actor. A repeat finds the row by its source key.
    ///
    /// The turn runs on the child's session actor with the deployment's turn
    /// services, never here; its end resolves the process's child-session
    /// wait. The child runtime assembled to create the session is dropped on
    /// return: nothing caches it (FIG-3424).
    pub(in crate::runtime::session_manager) async fn initialize_session_and_mail_turn(
        &self,
        create_request: SessionCreateRequest,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
        turn_input: crate::TurnInput,
    ) -> Result<SessionId, SessionTurnInitError> {
        let requested_session_id = create_request.session_id.clone();
        // A recorded request can carry a start point this build keeps only
        // for decode (a predecessor `snapshot` payload on a durable
        // `ProcessInput::SessionTurn` row or its remote copy). Nothing can
        // run it — refuse deterministically so the process terminalizes
        // instead of retrying an unrunnable row on every pass.
        if !matches!(create_request.start, crate::SessionStartPoint::Empty) {
            return Err(SessionTurnInitError::Refused {
                source: Box::new(crate::PluginError::Session(
                    "the recorded session create request carries a start point initialisation does not admit; snapshot starts were removed with the managed-session machinery (FIG-3378)".to_string(),
                )),
            });
        }
        let initialized = Box::pin(initialize_session(
            &self.current,
            create_request,
            process_id,
        ))
        .await
        .map_err(|source| {
            // A catalog that cannot resolve a session by id can never reopen
            // the recorded session on any pass: refuse deterministically so
            // the process terminalizes instead of retrying it forever
            // (FIG-3487). A config this deployment's plugin set refuses is
            // the same kind of fact: every pass resolves the same recorded
            // request against the same owners (FIG-4396). Every other failure
            // stays `Create` — recoverable, because a transient catalog miss
            // may resolve on the next pass.
            if session_catalog_lookup_unsupported(&source)
                || session_config_refused(&source)
                || reasoning_refused(&source)
            {
                return SessionTurnInitError::Refused {
                    source: Box::new(source),
                };
            }
            SessionTurnInitError::Create {
                session_id: requested_session_id.clone(),
                source: Box::new(source),
            }
        })?;
        let InitializedSession { handle, session_id } = initialized;
        let store = handle.runtime.lock().await.services.store.clone();
        let store = store.ok_or_else(|| SessionTurnInitError::Create {
            session_id: Some(session_id.clone()),
            source: Box::new(crate::PluginError::Session(format!(
                "process child session `{session_id}` has no store to mail its turn to"
            ))),
        })?;
        let draft = crate::PendingTurnInputDraft::new(
            session_id.clone(),
            crate::TurnInputIngress::next_turn(),
            turn_input.durable_projection(),
        )
        .with_source_key(turn_id.as_str());
        store
            .store()
            .enqueue_pending_turn_input(draft)
            .await
            .map_err(|error| SessionTurnInitError::Create {
                session_id: Some(session_id.clone()),
                source: Box::new(crate::PluginError::of_store_error(
                    format_args!(
                        "failed to mail the turn of process `{process_id}` to its child session `{session_id}`"
                    ),
                    error,
                )),
            })?;
        Ok(session_id)
    }

    /// Withdraw the cancelled process's child turn input from its child
    /// session(s), without running a turn.
    ///
    /// The child session the request names, plus any session the catalog
    /// attributes to this process (covering an id a crashed pass minted but
    /// never recorded), is opened, and every open row there that is this
    /// turn's is cancelled. Answers the run that already took one, whose
    /// turn is then the one to cancel.
    pub(in crate::runtime::session_manager) async fn withdraw_process_child_inputs(
        &self,
        requested_session_id: Option<&SessionId>,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
    ) -> Result<Option<TurnId>, crate::PluginError> {
        let factory = self.current.host.core.session_store_factory();
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
        let mut admitted = None;
        for session_id in candidates {
            if let Some(run) = self
                .withdraw_open_process_child_input(&factory, &session_id, process_id, turn_id)
                .await?
            {
                admitted = Some(run);
            }
        }
        Ok(admitted)
    }

    async fn withdraw_open_process_child_input(
        &self,
        factory: &Arc<dyn crate::DeploymentStore>,
        session_id: &SessionId,
        process_id: &crate::ProcessId,
        turn_id: &TurnId,
    ) -> Result<Option<TurnId>, crate::PluginError> {
        let store = match crate::runtime::live_session_view(factory, session_id).await {
            Ok(store) => store,
            // A catalog without the by-id seam can hold no admissible input
            // this reconcile could reach — the same toleration
            // `list_sessions` got above (FIG-3487).
            Err(crate::StoreError::UnsupportedStoreOperation { .. }) => return Ok(None),
            Err(error) => {
                return Err(crate::PluginError::Session(format!(
                    "failed to inspect cancelled process `{process_id}` child session `{session_id}`: {error}"
                )));
            }
        };
        let Some(store) = store else {
            return Ok(None);
        };
        // A session the catalog attributes to this process exists solely to
        // run its turn, so every open row under it is the process's work. A
        // session not caused by this process is foreign: only the row mailed
        // under this turn's id may be touched.
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
            .list_pending_turn_inputs()
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
                    && (owned_by_process
                        || read.input.source_key.as_deref() == Some(turn_id.as_str()))
            })
            .map(|read| {
                crate::PendingTurnInputCancelTarget::input_id(read.input.input_id.to_string())
            })
            .collect();
        if targets.is_empty() {
            return Ok(None);
        }
        let receipts = store
            .cancel_pending_turn_inputs(&targets)
            .await
            .map_err(|error| {
                crate::PluginError::Session(format!(
                    "failed to withdraw cancelled process `{process_id}` child session `{session_id}` inputs: {error}"
                ))
            })?;
        Ok(receipts
            .into_iter()
            .find_map(|receipt| match receipt.outcome {
                crate::PendingTurnInputCancelOutcome::AlreadyAdmitted { run, .. } => Some(run),
                _ => None,
            }))
    }
}

/// Where [`RuntimeSessionServices::initialize_session_and_mail_turn`]
/// stopped.
pub(in crate::runtime::session_manager) enum SessionTurnInitError {
    /// Session initialization or the turn's mail failed; another pass may
    /// succeed.
    ///
    /// `session_id` is the child's recorded identity when the durable
    /// request fixed it. Creation is multi-stage, so a `Some` here means a
    /// retained session *may* already exist for that id.
    Create {
        session_id: Option<SessionId>,
        source: Box<crate::PluginError>,
    },
    /// The recorded request can never be initialized by this deployment —
    /// a predecessor payload whose start point `SessionStartPoint` keeps
    /// only for decode, a configured catalog that cannot resolve the
    /// recorded session by id, or a config its plugin set refuses.
    /// Deterministic: no pass can run it, so the process terminalizes with
    /// the refusal rather than retrying an unrunnable row forever.
    Refused { source: Box<crate::PluginError> },
}

#[cfg(test)]
mod tests {
    use super::*;

    const THINKER: &str = "thinker";
    const PLAIN: &str = "plain";

    /// A catalog of two keys: `thinker` advertises the `high` effort, and
    /// `plain` has no reasoning controls.
    fn child_llm_profiles() -> std::sync::Arc<crate::LlmProfileRegistry> {
        let provider = || {
            crate::testing::TestProvider::builder()
                .kind("child-facts")
                .build()
                .into_handle()
        };
        let thinker = crate::LlmProfileMetadata::builder("thinker-wire")
            .context_window_tokens(64_000)
            .capability(crate::LlmProfileCapability {
                reasoning: Some(crate::ReasoningCapability {
                    efforts: vec!["high".to_string()],
                    encoding: crate::ReasoningEncoding::Effort,
                    disable: false,
                    mandatory: false,
                }),
                ..crate::LlmProfileCapability::default()
            })
            .build()
            .expect("thinker metadata");
        std::sync::Arc::new(
            crate::LlmProfileRegistry::new()
                .register(
                    THINKER,
                    crate::RegisteredLlmProfile::new(thinker, provider()),
                )
                .and_then(|registry| {
                    registry.register(
                        PLAIN,
                        crate::RegisteredLlmProfile::new(
                            crate::testing::test_llm_profile_metadata("plain-wire"),
                            provider(),
                        ),
                    )
                })
                .expect("two distinct keys register"),
        )
    }

    /// Resolve `request`'s facts on a deployment of [`child_llm_profiles`].
    fn child_facts(request: &SessionCreateRequest) -> Result<ChildFacts, crate::PluginError> {
        let plugin_host = crate::testing::test_plugin_host(Vec::new());
        let models = child_llm_profiles();
        resolve_child_facts(
            &StarterFacts {
                plugin_host: &plugin_host,
                protocol_plugin_id: Some("test_protocol"),
                plugin_admission: crate::store::plugin_writers::PluginAdmission::default(),
                models: models.as_ref(),
            },
            request,
            &SessionId::from("child-facts"),
        )
    }

    fn runtime_code(error: &crate::PluginError) -> Option<&crate::RuntimeErrorCode> {
        match error {
            crate::PluginError::Runtime(runtime) => Some(&runtime.code),
            _ => None,
        }
    }

    fn root_request() -> SessionCreateRequest {
        SessionCreateRequest::root(
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::default(),
        )
    }

    /// A stated policy with nothing else: the turn budget and tool-call
    /// limit a creator must choose.
    fn stated_policy() -> SessionPolicy {
        SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024))
    }

    /// FIG-4531: a child's model key is judged where the child's facts
    /// resolve. The reasoning the request states must fit the capability its
    /// key mints, and a key the models do not register is typed: terminal at
    /// admission, and the retryable `LlmProfileUnavailable` on the worker of a
    /// start that was admitted with it.
    #[test]
    fn a_child_profile_key_is_judged_typed_when_its_facts_resolve() {
        let high = crate::ReasoningSelection::Effort("high".to_string());
        let request = |key: &str| {
            let mut request = root_request().with_llm_profile(crate::LlmProfileKey::new(key));
            request.policy = Some(stated_policy());
            request.reasoning = Some(high.clone());
            request
        };

        let refused = child_facts(&request(PLAIN))
            .err()
            .expect("a stated effort the key cannot take is refused");
        assert_eq!(
            runtime_code(&refused),
            Some(&crate::RuntimeErrorCode::ReasoningRefused),
            "{refused:?}"
        );
        assert!(reasoning_refused(&refused));

        let facts = child_facts(&request(THINKER)).expect("the effort fits the key");
        assert_eq!(
            facts
                .policy
                .model
                .expect("the child records a model")
                .reasoning,
            high
        );

        let unknown = request("retired");
        let refused = child_facts(&unknown)
            .err()
            .expect("an unregistered key is refused");
        assert_eq!(
            runtime_code(&refused),
            Some(&crate::RuntimeErrorCode::LlmProfileUnknown),
            "admission refuses the key that was named: {refused:?}"
        );
        let on_worker = unserved_profile_key(unknown.model.as_ref(), refused);
        assert_eq!(
            runtime_code(&on_worker),
            Some(&crate::RuntimeErrorCode::LlmProfileUnavailable)
        );
        assert!(
            on_worker.is_retryable() && !on_worker.is_terminal(),
            "an admitted start's unserved key retries until a deployment serves it"
        );
        let crate::PluginError::Runtime(on_worker) = on_worker else {
            unreachable!("the code was read from a runtime error");
        };
        assert_eq!(
            on_worker.profile_key(),
            Some(&crate::LlmProfileKey::new("retired")),
            "the fault names the start's key typed: {on_worker:?}"
        );
        assert_eq!(
            crate::store::ParkReason::engine_retry_exhausted(
                8,
                None,
                on_worker.attempt_failure_text()
            )
            .profile_key(),
            Some(&crate::LlmProfileKey::new("retired")),
            "the park of the exhausted retries names the key"
        );
    }

    /// FIG-4594: a request that states its whole config (a host session-turn
    /// start's spec) records exactly that: its policy's turn budget, and its
    /// key minted with the reasoning it states.
    #[test]
    fn a_request_that_states_its_spec_records_it() {
        let spec = crate::SessionSpec::new(
            THINKER,
            crate::TurnBudget::bounded(4),
            crate::MaxToolCalls::new(1024),
        )
        .reasoning(crate::ReasoningSelection::Effort("high".to_string()));
        let request = root_request().with_spec(&spec).expect("a root spec");
        assert_eq!(request.unstated_root_config(), None);

        let facts = child_facts(&request).expect("the stated spec resolves");
        assert_eq!(facts.policy.turn_budget, crate::TurnBudget::bounded(4));
        let model = facts.policy.model.expect("the child records a model");
        assert_eq!(model.key().as_str(), THINKER);
        assert_eq!(
            model.reasoning,
            crate::ReasoningSelection::Effort("high".to_string())
        );

        assert_eq!(
            root_request().unstated_root_config(),
            Some(crate::UnstatedSessionConfig::Policy)
        );
        assert!(
            root_request()
                .with_spec(&crate::SessionSpec::inherit())
                .is_err(),
            "an overlay states no session of its own"
        );
    }

    /// FIG-5296: creation is explicit (ADR 0134). A child request that
    /// states no policy is refused typed, never filled from the session that
    /// starts it; a key named without a reasoning runs the default
    /// selection, not a reasoning carried from any other model; and the
    /// plugin config is every owner's default, never a parent's namespace.
    #[test]
    fn a_child_records_only_what_its_request_states() {
        let unstated = SessionCreateRequest::child_session(
            "parent",
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::default(),
        );
        let refused = child_facts(&unstated)
            .err()
            .expect("a request with no policy is refused");
        let crate::PluginError::Runtime(runtime) = &refused else {
            panic!("the refusal is a runtime error: {refused:?}");
        };
        assert_eq!(runtime.code, crate::RuntimeErrorCode::SessionConfigRefused);
        assert_eq!(
            runtime
                .config_refusal()
                .and_then(crate::ConfigRefusal::owner_refusal::<crate::CoreConfigRefusal>),
            Some(crate::CoreConfigRefusal::PolicyUnstated),
            "{runtime:?}"
        );

        let keyed = SessionCreateRequest::child(
            "parent",
            crate::SessionStartPoint::Empty,
            stated_policy(),
            crate::PluginOptions::default(),
        )
        .with_llm_profile(crate::LlmProfileKey::new(THINKER));
        let facts = child_facts(&keyed).expect("a stated policy and key resolve");
        assert_eq!(
            facts.policy.model.expect("the key mints").reasoning,
            crate::ReasoningSelection::default(),
            "a key named alone runs the default reasoning"
        );
        assert_eq!(facts.policy.turn_budget, crate::TurnBudget::Unbounded);
        assert_eq!(
            facts.plugin_config,
            child_facts(&SessionCreateRequest::child(
                "another-parent",
                crate::SessionStartPoint::Empty,
                stated_policy(),
                crate::PluginOptions::default(),
            ))
            .expect("an unrelated child resolves")
            .plugin_config,
            "the plugin config depends on the request alone"
        );
    }

    /// FIG-4652: a child whose creation config is refused fails with the
    /// refusal as its typed cause, not as message text, and the cause
    /// survives the plugin boundary's encoding.
    #[test]
    fn a_refused_child_creation_config_carries_its_typed_cause() {
        let mut request = SessionCreateRequest::root(
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::typed("no-such-plugin", serde_json::json!({ "k": 1 }))
                .expect("options"),
        );
        request.policy = Some(stated_policy());
        let refused = child_facts(&request)
            .err()
            .expect("a namespace no installed plugin owns is refused");
        assert!(session_config_refused(&refused), "{refused:?}");
        let expected = crate::ConfigRefusal {
            owner: "no-such-plugin".to_string(),
            at: crate::RefusalSite::Creation,
            reason: crate::ConfigRefusalReason::UnknownOwner,
        };
        let decoded: crate::PluginError =
            serde_json::from_slice(&serde_json::to_vec(&refused).expect("encode the refusal"))
                .expect("decode the refusal");
        for error in [refused, decoded] {
            let crate::PluginError::Runtime(runtime) = error else {
                panic!("the refusal is a runtime error: {error:?}");
            };
            assert_eq!(runtime.code, crate::RuntimeErrorCode::SessionConfigRefused);
            assert!(runtime.is_terminal());
            assert_eq!(
                runtime.cause,
                Some(crate::RuntimeErrorCause::ConfigRefused {
                    refusal: Box::new(expected.clone()),
                })
            );
            assert_eq!(runtime.config_refusal(), Some(&expected));
        }
    }
}
