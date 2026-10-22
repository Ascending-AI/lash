use super::*;
use crate::core_internal::RuntimeExecutionContextRuntimeOps as _;
use crate::plugin::PluginSessionRequest;
use crate::tool_dispatch::ToolDispatchContext;
use crate::{ToolCall, ToolOutcome, ToolProvider};

struct NoopTools;

#[async_trait::async_trait]
impl ToolProvider for NoopTools {
    fn tool_manifests(
        &self,
        _config: &crate::OwnerConfigView,
    ) -> std::result::Result<
        Vec<crate::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &crate::OwnerConfigView,
    ) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::err_fmt("not used").into()
    }
}

fn test_execution_context() -> RuntimeExecutionContext<'static> {
    test_execution_context_with_env_store(Arc::new(crate::testing::UnavailableProcessEnvStore))
}

fn test_execution_context_with_env_store(
    env_store: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
) -> RuntimeExecutionContext<'static> {
    let plugins = crate::plugin::PluginHost::empty(
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    )
    .build_session(PluginSessionRequest::creation(
        "session",
        crate::plugin::SessionAuthorityContext::fixture(),
    ))
    .expect("plugin session");
    let dispatch = Arc::new(ToolDispatchContext {
        fleet_format: crate::FleetFormat::current(),
        composition: (plugins).composition(),
        config: crate::plugin::OwnerConfigView::session((plugins).admitted_plugin_config()),
        execution_budgets: (plugins).execution_budgets(),
        state: plugins,
        tools: Arc::new((NoopTools).into()),
        routes: crate::SurfaceTools::default(),
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::plugin::ServiceBindings::default()),
        process_engines: crate::ProcessEngineRegistry::default(),
        effect_controller: crate::ActorContext::unavailable()
            .scoped(crate::AdmittedScope::runtime_operation(
                "test-runtime-effect-controller",
            ))
            .expect("valid test runtime scope"),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        observation_call_key: None,
        facts: crate::plugin::ExecutionFacts::of_process(
            &lash_execution_contracts::testing::process_env_with_limit(1024),
        ),
        owner: crate::ExecutionOwner::SessionFrame {
            session_id: SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        },
        observer: std::sync::Arc::new(crate::engine::NullObservationSink),
        attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
        process_lineage: None,
        process_originator: None,
    });
    RuntimeExecutionContext::new(
        dispatch,
        env_store,
        Arc::new(crate::RuntimeAttachmentStore::unavailable()),
        Arc::new(crate::ChronologicalProjection::default()),
        crate::TurnContext::default(),
    )
}

#[test]
fn parentless_effect_envelopes_use_process_originator_not_ambient_session() {
    let envelope = |context: &RuntimeExecutionContext<'_>, effect_id: &str| {
        crate::RuntimeEffectEnvelope::new(
            context.language_runtime_invocation(effect_id),
            crate::RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "sample".to_string(),
            },
        )
    };

    let foreground = test_execution_context();
    assert_eq!(
        envelope(&foreground, "foreground").invocation.attribution,
        crate::RuntimeAttribution::for_session("session")
    );

    let mut host_process = test_execution_context();
    host_process.process_execution = Some(RuntimeProcessExecution {
        process_id: crate::process_id_for_test("host-process"),
        provenance: lash_execution_contracts::process::ProcessProvenance::new(
            lash_execution_contracts::process::ProcessOriginator::host_scoped("automation"),
        ),
        enclosing: None,
    });
    assert_eq!(
        envelope(&host_process, "host").invocation.attribution,
        crate::RuntimeAttribution::none(),
        "ambient current-session capability is descriptive inside a host-owned process"
    );

    let mut session_process = test_execution_context();
    session_process.process_execution = Some(RuntimeProcessExecution {
        process_id: crate::process_id_for_test("session-process"),
        provenance: lash_execution_contracts::process::ProcessProvenance::session(
            lash_execution_contracts::process::SessionScope::new("origin-session"),
        ),
        enclosing: None,
    });
    assert_eq!(
        envelope(&session_process, "session-origin")
            .invocation
            .attribution,
        crate::RuntimeAttribution::for_session("origin-session")
    );
}

// ---------------------------------------------------------------------------
// FIG-3417: the lifecycle parent a child start declares comes from ONE shared
// derivation — the admitted execution scope, which for a process already
// carries the incarnation the admission authority bound. Nothing on this path
// re-resolves the reusable process name against the registry.
// ---------------------------------------------------------------------------

fn scoped_context(
    session_id: &str,
    admitted: crate::AdmittedScope,
) -> RuntimeExecutionContext<'static> {
    let controller = crate::ActorContext::unavailable()
        .scoped(admitted)
        .expect("the test scope validates");
    // Borrowed: the build keeps the scope the controller admits, where a
    // shared controller is re-scoped to the fixture's turn.
    crate::testing::TestExecutionContextBuilder::over_controller(
        crate::testing::TestEffectController::Borrowed(controller),
    )
    .session_id(lash_protocol_types::SessionId::fixture(session_id))
    .plugin_factories(vec![])
    .build()
    .into_runtime()
}

/// A session operation is a durable owner with an end protocol (FIG-3419), so
/// a child it starts is started by the drain itself — the derivation must not
/// silently borrow the session's current turn.
#[tokio::test]
async fn a_child_started_from_a_queued_drain_is_started_by_the_drain() {
    let context = scoped_context(
        "session-1",
        crate::AdmittedScope::session_operation("session-1", "drain-3"),
    );
    assert_eq!(
        context
            .start_cx()
            .expect("a session operation materializes a start context")
            .starter()
            .id(),
        &crate::ScopeId::session_operation("session-1", "drain-3"),
    );
}

/// Records the `(key, ordinal)` every emitted observation lands under: the
/// identity the spec pins (ADR 0105 §1).
#[derive(Default)]
struct ObservationIds(std::sync::Mutex<Vec<String>>);

impl crate::engine::ObservationSink for ObservationIds {
    fn observe(&self, observation: crate::engine::ShiftObservation) {
        self.0
            .lock()
            .expect("observation ids")
            .push(format!("{}#{}", observation.key, observation.ordinal));
    }
}

fn emit_started(context: &RuntimeExecutionContext<'_>, material: &str, sink: &ObservationIds) {
    let call = context.with_call_observation_key(context.call_observation_key(material));
    call.observation_cursor("directives:before").observe(
        sink,
        crate::engine::ObservedEvent::Activity {
            correlation_id: None,
            event: crate::TurnEvent::ToolCallStarted {
                call_id: crate::ToolCallId::fixture(material),
                provider_call_id: None,
                name: "tool".to_string(),
                args: serde_json::json!({}),
                graph_key: None,
            },
        },
    );
}

/// Two tool calls in one turn share the dispatch's observation base — on the
/// turn-dispatched protocol path there is no per-call invocation — so their
/// before-directive lanes only stay distinct because each call carries its
/// own qualified key (ADR 0105 §1).
#[test]
fn two_calls_on_one_base_mint_distinct_before_directive_ids() {
    let context = test_execution_context();
    let sink = ObservationIds::default();
    emit_started(&context, "0:0:lookup", &sink);
    emit_started(&context, "0:1:lookup", &sink);
    let ids = sink.0.lock().expect("observation ids").clone();
    assert_eq!(
        ids,
        [
            format!(
                "{}:call:0:0:lookup:directives:before#0",
                context.dispatch.observation_base_key()
            ),
            format!(
                "{}:call:0:1:lookup:directives:before#0",
                context.dispatch.observation_base_key()
            ),
        ],
        "each call's lane keys under its own material; a shared base would mint the same id twice"
    );
}

/// The model may repeat one `call_id` across protocol iterations: the
/// iteration is part of the lane material, so both calls mint distinct ids
/// (ADR 0105 §1).
#[test]
fn a_call_id_repeated_across_iterations_mints_distinct_ids() {
    let context = test_execution_context();
    let sink = ObservationIds::default();
    emit_started(&context, "0:0:dup", &sink);
    emit_started(&context, "1:0:dup", &sink);
    let ids = sink.0.lock().expect("observation ids").clone();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "iterations qualify the reused call id");
    assert!(ids[0].ends_with(":call:0:0:dup:directives:before#0"));
    assert!(ids[1].ends_with(":call:1:0:dup:directives:before#0"));
}

struct ProjectionRefusalLedger {
    attempts: Arc<std::sync::atomic::AtomicUsize>,
    refuse: Arc<std::sync::atomic::AtomicBool>,
}
impl lash_protocol_types::allocation::AllocationLedger for ProjectionRefusalLedger {
    fn reserve(
        &self,
        bytes: u64,
    ) -> Result<
        lash_protocol_types::allocation::AllocationHold,
        lash_protocol_types::allocation::AllocationError,
    > {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if !self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(Default::default());
        }
        Err(lash_protocol_types::allocation::AllocationError {
            kind: lash_protocol_types::allocation::ReserveError::Pressure,
            actor: "projection-law".into(),
            requested: bytes,
            capacity: 0,
        })
    }
    fn reserve_shared(
        &self,
        _: usize,
        bytes: u64,
    ) -> Result<
        lash_protocol_types::allocation::AllocationHold,
        lash_protocol_types::allocation::AllocationError,
    > {
        self.reserve(bytes)
    }
}

#[test]
fn chronological_cache_attempts_admission_before_materializing_a_turn_projection() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let budget = lash_protocol_types::allocation::AllocationBudget::new(ProjectionRefusalLedger {
        attempts: attempts.clone(),
        refuse: refuse.clone(),
    });
    let messages = crate::MessageSequence::default().clone_in(&budget).unwrap();
    let context = test_execution_context().with_turn_history(Default::default(), messages);
    attempts.store(0, std::sync::atomic::Ordering::SeqCst);
    refuse.store(true, std::sync::atomic::Ordering::SeqCst);
    let _projection = context.chronological_projection();
    assert!(
        attempts.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "chronological cache must ask the serving ledger before materializing"
    );
}
