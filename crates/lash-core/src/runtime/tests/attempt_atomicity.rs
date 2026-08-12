//! The tool-attempt atomicity matrix: every `ToolContext` capability, executed
//! inside a controller-owned recorded `ToolAttempt`, against the
//! [attempt-atomicity sentinel](crate::testing::attempt_sentinel).
//!
//! An ordinal-addressed controller tier records a whole tool attempt as one journal entry
//! and replays that entry on redrive *without re-entering the body*. Any
//! journal command the body emitted while it ran therefore sits in the journal
//! unre-issued, and the handler's next command meets it at the wrong ordinal
//! (Restate `RT0016`). ADR 0042 states the rule; this module is its exhaustive
//! enforcement.
//!
//! Each row of the FIG-1127 route inventory appears exactly once in
//! [`ROUTE_MATRIX`] with three literal facts: the route's classification, the
//! literal outcome text the route produces inside an attempt, and the literal
//! list of controller-boundary crossings the sentinel observes. Nothing is
//! recomputed from the code under test.
//!
//! Crossings, not defects. A crossing means "this capability reached the
//! controller from inside a recorded attempt". Whether that crossing *emits an
//! engine command* is a tier fact: the Restate controller serves `List` and
//! `Transfer` from the registry and derives await-event keys purely (FIG-1126),
//! while `Start`, `Await`, `Cancel`, `Signal` and trigger emission issue real
//! `ctx` commands. Key-addressed tiers can replay those nested commands by
//! stable key and therefore retain them. The ordinal-shift question itself is settled against real
//! captured journal bytes in `lash-restate`'s endpoint-protocol suite; this
//! matrix is what makes the *inventory* structural rather than enumerated:
//! [`sentinel_allows_no_undeclared_crossing_from_inside_an_attempt`] fails the
//! moment any capability reaches the controller from inside an attempt without
//! a declared row, whether or not anyone wrote a law for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::testing::attempt_sentinel::{AttemptAtomicitySentinel, NestedJournalLedger};

const SESSION: &str = "atomic-tool-test-session";
const TURN: &str = "attempt-atomicity-turn";
const ATTEMPT_EFFECT_ID: &str = "attempt-atomicity-attempt";
const CALL_ID: &str = "attempt-atomicity-call";
const LIVE_PROCESS: &str = "attempt-atomicity-live";
const TERMINAL_PROCESS: &str = "attempt-atomicity-terminal";
const EXTERNAL_PROCESS: &str = "attempt-atomicity-external";

/// A controller-owned tier stand-in with an explicit journal-addressing model.
struct ControllerOwnedTier {
    inner: crate::InlineRuntimeEffectController,
    addressing: crate::EffectJournalAddressing,
}

impl ControllerOwnedTier {
    fn ordinal_addressed() -> Self {
        Self {
            inner: crate::InlineRuntimeEffectController::default(),
            addressing: crate::EffectJournalAddressing::OrdinalAddressed,
        }
    }

    fn key_addressed() -> Self {
        Self {
            inner: crate::InlineRuntimeEffectController::default(),
            addressing: crate::EffectJournalAddressing::KeyAddressed,
        }
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for ControllerOwnedTier {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        crate::EffectReplayOwnership::Controller
    }

    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        self.addressing
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        // Opted in so `completion_key()` reaches its await-event derivation
        // instead of stopping at the process-lifetime refusal; the derivation
        // is the route this matrix is classifying.
        true
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for ControllerOwnedTier {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        self.inner.execute_effect(envelope, local_executor).await
    }
}

/// How a route behaves inside a controller-owned recorded attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Classification {
    /// Typed refusal before any side effect.
    Guarded,
    /// Never reaches the controller at all, so it cannot shift an ordinal.
    NoControllerCrossing,
    /// Crosses the controller, and the Restate controller serves the crossing
    /// without issuing an engine command (proven at the endpoint tier).
    JournalNeutralCrossing,
}

#[derive(Clone, Copy)]
enum Route {
    DispatchBatch,
    ProcessStart,
    ProcessAwait,
    ProcessCancel,
    ProcessSignal,
    ProcessList,
    ProcessCompleteExternal,
    ProcessTransfer,
    TriggerEmit,
    SessionStartTurn,
    SessionCreateClose,
    SessionReads,
    DirectCompletion,
    AttachmentPut,
    ProcessEventEmit,
    ProcessEventWait,
    CompletionKey,
    EmitChildProcessStarted,
    NamedPhase,
    Accessors,
}

struct MatrixRow {
    route: Route,
    /// The inventory label, matching ADR 0042's table.
    label: &'static str,
    classification: Classification,
    /// Literal text the route produces inside a recorded attempt.
    outcome: &'static str,
    /// Literal controller-boundary crossings the sentinel observes, in order.
    crossings: &'static [&'static str],
}

/// The 19-row `ToolContext` inventory plus the internal `Transfer` command at
/// the shared process-command choke point. Adding a capability without adding a row trips
/// [`sentinel_allows_no_undeclared_crossing_from_inside_an_attempt`].
const ROUTE_MATRIX: &[MatrixRow] = &[
    MatrixRow {
        route: Route::DispatchBatch,
        label: "dispatch().batch()",
        classification: Classification::Guarded,
        outcome: "nested tool batch dispatch is unavailable inside an atomic tool attempt; decompose the work into process steps",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessStart,
        label: "processes().start()",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::processes().start() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; decompose the process command into a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessAwait,
        label: "processes().await_process()",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::processes().await_process() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; decompose the process command into a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessCancel,
        label: "processes().cancel()",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::processes().cancel() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; decompose the process command into a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessSignal,
        label: "processes().signal()",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::processes().signal() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; decompose the process command into a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessList,
        label: "processes().list_handles_filtered()",
        classification: Classification::JournalNeutralCrossing,
        outcome: "list ok",
        crossings: &["execute_effect:process:process:list:session:atomic-tool-test-session:live"],
    },
    MatrixRow {
        route: Route::ProcessCompleteExternal,
        label: "processes().complete_external()",
        classification: Classification::NoControllerCrossing,
        outcome: "complete_external ok",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessTransfer,
        label: "ProcessCommand::Transfer",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::processes().transfer() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; decompose the process command into a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::TriggerEmit,
        label: "triggers().emit()",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::triggers().emit() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; emit the trigger from a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::SessionStartTurn,
        label: "sessions().start_turn()",
        classification: Classification::Guarded,
        outcome: "plugin session error: ToolContext::sessions().start_turn() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; start the nested turn from a process step; a first-class intent protocol is pending",
        crossings: &[],
    },
    MatrixRow {
        route: Route::SessionCreateClose,
        label: "sessions().create_session() / close_session()",
        classification: Classification::NoControllerCrossing,
        outcome: "create/close ok",
        crossings: &[],
    },
    MatrixRow {
        route: Route::SessionReads,
        label: "sessions().snapshot*/model/tool_catalog/set_tool_membership",
        classification: Classification::NoControllerCrossing,
        // The five read routes succeed; `set_tool_membership` reaches the
        // session's tool registry and is refused there because this fixture's
        // catalog is empty. Either way the route never touches the controller,
        // which is what this row classifies.
        outcome: "plugin session error: unknown tool `attempt-atomicity-tool`",
        crossings: &[],
    },
    MatrixRow {
        route: Route::DirectCompletion,
        label: "direct_completions().complete()",
        classification: Classification::NoControllerCrossing,
        outcome: "direct ok",
        crossings: &[],
    },
    MatrixRow {
        route: Route::AttachmentPut,
        label: "attachments().put()",
        classification: Classification::NoControllerCrossing,
        outcome: "attachment ok",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessEventEmit,
        label: "process_events().emit() / emit_request()",
        classification: Classification::NoControllerCrossing,
        outcome: "process events emitted",
        crossings: &[],
    },
    MatrixRow {
        route: Route::ProcessEventWait,
        label: "process_events().wait_event_after()",
        classification: Classification::NoControllerCrossing,
        outcome: "wait ok",
        crossings: &[],
    },
    MatrixRow {
        route: Route::CompletionKey,
        label: "completion_key()",
        classification: Classification::JournalNeutralCrossing,
        outcome: "completion key ok",
        crossings: &["await_event_key:attempt-atomicity-turn"],
    },
    MatrixRow {
        route: Route::EmitChildProcessStarted,
        label: "emit_child_process_started()",
        classification: Classification::NoControllerCrossing,
        outcome: "child process started hook fired once",
        crossings: &[],
    },
    MatrixRow {
        route: Route::NamedPhase,
        label: "named_phase()",
        classification: Classification::NoControllerCrossing,
        outcome: "named phase ok",
        crossings: &[],
    },
    MatrixRow {
        route: Route::Accessors,
        label: "cancellation_token() / id + payload accessors",
        classification: Classification::NoControllerCrossing,
        outcome: "accessors ok",
        crossings: &[],
    },
];

struct Fixtures {
    host: Arc<crate::testing::MockSessionManager>,
    registry: Arc<dyn crate::ProcessRegistry>,
    trigger_store: Arc<crate::facade_support::InMemoryTriggerStore>,
    lease: crate::ProcessLease,
    child_process_starts: Arc<AtomicUsize>,
}

async fn fixtures() -> Fixtures {
    let host = Arc::new(
        crate::testing::MockSessionManager::default().with_tool_registry(
            crate::ToolRegistry::from_tool_provider(Arc::new(crate::testing::EmptyToolProvider))
                .expect("empty tool registry"),
        ),
    );
    let registry: Arc<dyn crate::ProcessRegistry> = host.process_registry.clone();
    let event_types = [
        "attempt.atomicity.note",
        "attempt.atomicity.awaited",
        "signal.resume",
    ]
    .into_iter()
    .map(|name| crate::ProcessEventType {
        name: name.to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    })
    .collect::<Vec<_>>();
    for (id, disposition) in [
        (LIVE_PROCESS, crate::RecoveryDisposition::Rerunnable),
        (
            TERMINAL_PROCESS,
            crate::RecoveryDisposition::ExternallyOwned,
        ),
        (
            EXTERNAL_PROCESS,
            crate::RecoveryDisposition::ExternallyOwned,
        ),
    ] {
        registry
            .register_process_with_observers(
                crate::ProcessRegistration::new(
                    id,
                    crate::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    disposition,
                    crate::ProcessProvenance::host(),
                )
                .with_extra_event_types(event_types.clone()),
                &[SESSION.to_string()],
            )
            .await
            .expect("register matrix process");
    }
    registry
        .complete_process(
            TERMINAL_PROCESS,
            crate::ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete terminal matrix process");
    let owner = crate::LeaseOwnerIdentity::opaque("attempt-atomicity", "incarnation");
    let lease = registry
        .claim_process_lease(LIVE_PROCESS, &owner, 60_000)
        .await
        .expect("claim live process lease")
        .acquired()
        .expect("live process lease");
    registry
        .record_first_started_with_authority(
            LIVE_PROCESS,
            crate::ProcessStarted {
                owner,
                fencing_token: lease.fencing_token,
                attempt: 1,
                started_at_ms: 1,
            },
            &crate::ProcessExecutionWriteAuthority::lease(lease.clone()),
        )
        .await
        .expect("start live matrix process");
    let trigger_store = Arc::new(crate::facade_support::InMemoryTriggerStore::default());
    Fixtures {
        host,
        registry,
        trigger_store,
        lease,
        child_process_starts: Arc::new(AtomicUsize::new(0)),
    }
}

fn prepared_tool_call() -> crate::PreparedToolCall {
    crate::PreparedToolCall {
        call_id: CALL_ID.to_string(),
        tool_id: crate::ToolId::from("tool:attempt_atomicity".to_string()),
        tool_name: "attempt_atomicity".to_string(),
        args: serde_json::json!({"value": CALL_ID}),
        replay: None,
        prepared_payload: serde_json::json!({"prepared": true}),
    }
}

fn attempt_invocation() -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        ATTEMPT_EFFECT_ID,
        crate::RuntimeEffectKind::ToolAttempt,
        ATTEMPT_EFFECT_ID,
    )
}

fn tool_context<'run>(
    scoped: crate::ScopedEffectController<'run>,
    fixtures: &Fixtures,
) -> crate::ToolContext<'run> {
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let plugins = crate::plugin::PluginHost::new(Vec::new())
        .build_session(SESSION, None)
        .expect("build attempt-atomicity plugin session");
    let processes = crate::testing::effect_backed_process_service(Arc::clone(&fixtures.registry));
    let child_process_starts = Arc::clone(&fixtures.child_process_starts);
    let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
        plugins,
        tools: Arc::new(crate::testing::EmptyToolProvider),
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
        sessions: fixtures.host.clone(),
        session_lifecycle: fixtures.host.clone(),
        session_graph: fixtures.host.clone(),
        processes,
        trigger_router: Some(crate::TriggerRouter::new(
            Arc::clone(&fixtures.trigger_store) as Arc<dyn crate::TriggerStore>,
            Some(Arc::clone(&fixtures.registry)),
            None,
        )),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped),
        // Production runs a tool-position direct completion locally
        // (`DirectExecutionPosition::ToolAttempt` in
        // `runtime/session_manager/direct.rs`), never through the controller.
        // The test source stands in for the provider, not for the routing
        // decision.
        direct_completions: crate::DirectCompletionClient::from_fn(|_request, _usage_source| {
            Ok(crate::plugin::DirectCompletion {
                text: "direct ok".to_string(),
                usage: crate::TokenUsage::default(),
                llm_call: crate::LlmCallRecord {
                    call_id: crate::LlmCallId("attempt-atomicity-direct".to_string()),
                    label: None,
                    attempts: Vec::new(),
                },
            })
        }),
        parent_invocation: Some(attempt_invocation()),
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: SESSION.to_string(),
        agent_frame_id: String::new(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
    });
    crate::ToolContext::from_dispatch(dispatch)
        .tool_call_id(Some(CALL_ID.to_string()))
        .parent_invocation(Some(attempt_invocation()))
        .cancellation_token(Some(tokio_util::sync::CancellationToken::new()))
        .child_execution_trace_hook(Some(crate::ToolChildExecutionTraceHook::new(
            move |_started| {
                child_process_starts.fetch_add(1, Ordering::SeqCst);
            },
        )))
        .process_events(
            LIVE_PROCESS,
            crate::ProcessExecutionWriteAuthority::lease(fixtures.lease.clone()),
            Arc::clone(&fixtures.registry),
            crate::ProcessAwaiter::polling(Arc::clone(&fixtures.registry)),
            None,
            None,
            None,
            Arc::new(crate::SystemClock),
            crate::WakeTurnPolicy::default(),
        )
        .build()
}

async fn invoke(route: Route, context: &crate::ToolContext<'_>, fixtures: &Fixtures) -> String {
    match route {
        Route::DispatchBatch => {
            let replies = context
                .dispatch()
                .batch(vec![crate::ToolInvocation::new(
                    "attempt-atomicity-batch",
                    crate::ToolId::new("noop"),
                    serde_json::Value::Null,
                )])
                .await;
            assert_eq!(
                replies.len(),
                1,
                "batch dispatch returns one reply per call"
            );
            let projected = replies[0].output.value_for_projection();
            projected
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("batch reply carried no failure message")
                .to_string()
        }
        Route::ProcessStart => match context
            .processes()
            .start(crate::ProcessStartRequest::external(
                "attempt-atomicity-nested",
                crate::ProcessOriginator::host_scoped("attempt-atomicity"),
                serde_json::Value::Null,
            ))
            .await
        {
            Ok(_) => "start ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::ProcessAwait => match context.processes().await_process(TERMINAL_PROCESS).await {
            Ok(_) => "await ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::ProcessCancel => match context.processes().cancel(LIVE_PROCESS).await {
            Ok(_) => "cancel ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::ProcessSignal => match context
            .processes()
            .signal(LIVE_PROCESS, "resume", serde_json::json!({"go": true}))
            .await
        {
            Ok(_) => "signal ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::ProcessList => {
            match context
                .processes()
                .list_handles_filtered(&crate::ProcessListFilter::default())
                .await
            {
                Ok(_) => "list ok".to_string(),
                Err(error) => error.to_string(),
            }
        }
        Route::ProcessCompleteExternal => match context
            .processes()
            .complete_external(
                EXTERNAL_PROCESS,
                crate::ProcessAwaitOutput::Success {
                    value: serde_json::json!("external done"),
                    control: None,
                },
            )
            .await
        {
            Ok(_) => "complete_external ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::ProcessTransfer => match context
            .processes
            .transfer(
                SESSION,
                "attempt-atomicity-transfer-target",
                vec![LIVE_PROCESS.to_string()],
                crate::ProcessOpScope::new(context.effect_controller.scoped())
                    .with_parent_invocation(context.parent_invocation.clone()),
            )
            .await
        {
            Ok(()) => "transfer ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::TriggerEmit => match context
            .triggers()
            .emit(crate::TriggerOccurrenceRequest::new(
                "attempt-atomicity.trigger",
                "attempt-atomicity-source",
                serde_json::json!({}),
                "attempt-atomicity-occurrence",
            ))
            .await
        {
            Ok(_) => "trigger ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::SessionStartTurn => match context
            .sessions()
            .start_turn(
                "attempt-atomicity-child",
                "attempt-atomicity-child-turn",
                crate::TurnInput::text("nested turn"),
            )
            .await
        {
            Ok(_) => "start_turn ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::SessionCreateClose => {
            let sessions = context.sessions();
            if let Err(error) = sessions
                .create_session(crate::SessionCreateRequest::root(
                    crate::plugin::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                ))
                .await
            {
                return error.to_string();
            }
            match sessions.close_session("attempt-atomicity-child").await {
                Ok(()) => "create/close ok".to_string(),
                Err(error) => error.to_string(),
            }
        }
        Route::SessionReads => {
            let sessions = context.sessions();
            if let Err(error) = sessions.snapshot_current().await {
                return error.to_string();
            }
            if let Err(error) = sessions.snapshot(SESSION).await {
                return error.to_string();
            }
            if let Err(error) = sessions.model().await {
                return error.to_string();
            }
            if let Err(error) = sessions.tool_catalog().await {
                return error.to_string();
            }
            if let Err(error) = sessions.shared_tool_catalog().await {
                return error.to_string();
            }
            match sessions
                .set_tool_membership(&["attempt-atomicity-tool".to_string()], true)
                .await
            {
                Ok(_) => "session membership mutated".to_string(),
                Err(error) => error.to_string(),
            }
        }
        Route::DirectCompletion => match context
            .direct_completions()
            .complete(
                crate::DirectRequest::text("attempt-atomicity-model", "attempt-atomicity direct"),
                "attempt-atomicity",
            )
            .await
        {
            Ok(completion) => completion.text,
            Err(error) => error.to_string(),
        },
        Route::AttachmentPut => match context
            .attachments()
            .put(
                vec![1, 2, 3, 4],
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("image/png").expect("png media type"),
                    Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    Some("attempt-atomicity.png".to_string()),
                ),
            )
            .await
        {
            Ok(_) => "attachment ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::ProcessEventEmit => {
            let events = context.process_events();
            if let Err(error) = events
                .emit("attempt.atomicity.note", serde_json::json!({"n": 1}))
                .await
            {
                return error.to_string();
            }
            match events
                .emit_request(crate::ProcessEventAppendRequest::new(
                    "attempt.atomicity.note",
                    serde_json::json!({"n": 2}),
                ))
                .await
            {
                Ok(_) => "process events emitted".to_string(),
                Err(error) => error.to_string(),
            }
        }
        Route::ProcessEventWait => {
            let events = context.process_events();
            if let Err(error) = events
                .emit("attempt.atomicity.awaited", serde_json::json!({"n": 1}))
                .await
            {
                return error.to_string();
            }
            match events
                .wait_event_after("attempt.atomicity.awaited", 0)
                .await
            {
                Ok(_) => "wait ok".to_string(),
                Err(error) => error.to_string(),
            }
        }
        Route::CompletionKey => match context.completion_key().await {
            Ok(_) => "completion key ok".to_string(),
            Err(error) => error.to_string(),
        },
        Route::EmitChildProcessStarted => {
            context.emit_child_process_started(LIVE_PROCESS, Some("child".to_string()));
            assert_eq!(
                fixtures.child_process_starts.load(Ordering::SeqCst),
                1,
                "the child-process-started hook fires once per call"
            );
            "child process started hook fired once".to_string()
        }
        Route::NamedPhase => {
            let _phase = context.named_phase("attempt-atomicity");
            "named phase ok".to_string()
        }
        Route::Accessors => {
            assert_eq!(
                context.session_id(),
                "atomic-tool-test-session",
                "accessor row reads the session id"
            );
            assert_eq!(
                context.tool_call_id(),
                Some("attempt-atomicity-call"),
                "accessor row reads the tool call id"
            );
            assert!(
                context.cancellation_token().is_some(),
                "accessor row reads the cancellation token"
            );
            "accessors ok".to_string()
        }
    }
}

struct AttemptRun {
    outcome: String,
    crossings: Vec<String>,
    attempt_bodies_opened: usize,
    body_runs: usize,
}

/// Executes `route` inside a real controller-owned recorded `ToolAttempt`,
/// through the sentinel, and reports what the sentinel saw.
async fn run_route_on_tier(route: Route, tier: &ControllerOwnedTier) -> AttemptRun {
    let fixtures = fixtures().await;
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped attempt-atomicity controller");
    let context = tool_context(scoped, &fixtures);
    let body_runs = Arc::new(AtomicUsize::new(0));
    let outcome_slot: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let envelope = crate::RuntimeEffectEnvelope::new(
        attempt_invocation(),
        crate::RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call(),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    {
        let body_runs = Arc::clone(&body_runs);
        let outcome_slot = Arc::clone(&outcome_slot);
        let context = &context;
        let fixtures = &fixtures;
        crate::RuntimeEffectController::execute_effect(
            &sentinel,
            envelope,
            crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                body_runs.fetch_add(1, Ordering::SeqCst);
                let outcome = invoke(route, context, fixtures).await;
                *outcome_slot.lock().expect("outcome slot") = Some(outcome);
                Ok(crate::RuntimeEffectOutcome::ToolAttempt {
                    launch: Box::new(crate::ToolAttemptLaunch::Done {
                        record: Box::new(crate::ToolCallRecord {
                            call_id: Some(CALL_ID.to_string()),
                            tool: "attempt_atomicity".to_string(),
                            args: serde_json::Value::Null,
                            output: crate::ToolCallOutput::success(serde_json::json!("ok")),
                            duration_ms: 0,
                        }),
                    }),
                    triggers: Vec::new(),
                })
            }),
        )
        .await
        .expect("recorded tool attempt completes");
    }
    let outcome = outcome_slot
        .lock()
        .expect("outcome slot")
        .clone()
        .expect("route produced an outcome");
    AttemptRun {
        outcome,
        crossings: ledger.crossings_inside_attempt(),
        attempt_bodies_opened: ledger.attempt_bodies_opened(),
        body_runs: body_runs.load(Ordering::SeqCst),
    }
}

async fn run_route(route: Route) -> AttemptRun {
    run_route_on_tier(route, &ControllerOwnedTier::ordinal_addressed()).await
}

async fn assert_row(row: &MatrixRow) -> AttemptRun {
    let run = run_route(row.route).await;
    assert_eq!(
        run.attempt_bodies_opened, 1,
        "{}: exactly one recorded attempt must open",
        row.label
    );
    assert_eq!(
        run.body_runs, 1,
        "{}: the attempt body must run exactly once",
        row.label
    );
    assert_eq!(run.outcome, row.outcome, "{}: outcome text", row.label);
    assert_eq!(
        run.crossings,
        row.crossings
            .iter()
            .map(|crossing| (*crossing).to_string())
            .collect::<Vec<_>>(),
        "{}: controller-boundary crossings from inside the attempt",
        row.label
    );
    match row.classification {
        Classification::Guarded | Classification::NoControllerCrossing => assert!(
            row.crossings.is_empty(),
            "{}: a guarded or non-crossing route must declare no crossing",
            row.label
        ),
        Classification::JournalNeutralCrossing => assert!(
            !row.crossings.is_empty(),
            "{}: a crossing classification must declare its crossings",
            row.label
        ),
    }
    run
}

#[tokio::test]
async fn attempt_atomicity_matrix_covers_the_whole_tool_context_inventory() {
    assert_eq!(
        ROUTE_MATRIX.len(),
        20,
        "the matrix must carry the 19 ToolContext rows plus ProcessCommand::Transfer"
    );
    let mut labels = ROUTE_MATRIX.iter().map(|row| row.label).collect::<Vec<_>>();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), 20, "every inventory row must be distinct");
    assert_eq!(
        ROUTE_MATRIX
            .iter()
            .filter(|row| row.classification == Classification::Guarded)
            .count(),
        8,
        "eight routes are guarded: batch dispatch, five process commands, trigger emission, and nested turns"
    );
}

fn key_addressed_expected(route: Route) -> (&'static str, &'static [&'static str]) {
    match route {
        Route::DispatchBatch => (
            "nested tool batch dispatch is unavailable inside an atomic tool attempt; decompose the work into process steps",
            &[],
        ),
        Route::ProcessStart => (
            "start ok",
            &["execute_effect:process:process:start:attempt-atomicity-nested"],
        ),
        Route::ProcessAwait => (
            "await ok",
            &["execute_effect:process:process:await:attempt-atomicity-terminal"],
        ),
        Route::ProcessCancel => (
            "cancel ok",
            &["execute_effect:process:process:cancel:attempt-atomicity-live"],
        ),
        Route::ProcessSignal => (
            "signal ok",
            &[
                "execute_effect:process:process:signal:attempt-atomicity-live:signal.resume:attempt-atomicity-call",
            ],
        ),
        Route::ProcessList => (
            "list ok",
            &["execute_effect:process:process:list:session:atomic-tool-test-session:live"],
        ),
        Route::ProcessCompleteExternal => ("complete_external ok", &[]),
        Route::ProcessTransfer => (
            "transfer ok",
            &[
                "execute_effect:process:process:transfer:session:atomic-tool-test-session:session:attempt-atomicity-transfer-target:process-transfer-set:v1:sha256:fba3e8dcf737aa169c5c5d1b3f322b19effdf791ee16cbe675356c6765ac1187",
            ],
        ),
        Route::TriggerEmit => ("trigger ok", &[]),
        Route::SessionStartTurn => ("start_turn ok", &[]),
        Route::SessionCreateClose => ("create/close ok", &[]),
        Route::SessionReads => (
            "plugin session error: unknown tool `attempt-atomicity-tool`",
            &[],
        ),
        Route::DirectCompletion => ("direct ok", &[]),
        Route::AttachmentPut => ("attachment ok", &[]),
        Route::ProcessEventEmit => ("process events emitted", &[]),
        Route::ProcessEventWait => ("wait ok", &[]),
        Route::CompletionKey => (
            "completion key ok",
            &["await_event_key:attempt-atomicity-turn"],
        ),
        Route::EmitChildProcessStarted => ("child process started hook fired once", &[]),
        Route::NamedPhase => ("named phase ok", &[]),
        Route::Accessors => ("accessors ok", &[]),
    }
}

#[tokio::test]
async fn key_addressed_controller_preserves_the_full_tool_context_matrix() {
    let tier = ControllerOwnedTier::key_addressed();
    let mut process_routes_succeeded = 0;
    for row in ROUTE_MATRIX {
        let run = run_route_on_tier(row.route, &tier).await;
        let (outcome, crossings) = key_addressed_expected(row.route);
        assert_eq!(
            run.attempt_bodies_opened, 1,
            "{}: attempt opened",
            row.label
        );
        assert_eq!(run.body_runs, 1, "{}: body ran once", row.label);
        assert_eq!(run.outcome, outcome, "{}: key-tier outcome", row.label);
        assert_eq!(
            run.crossings,
            crossings
                .iter()
                .map(|crossing| (*crossing).to_string())
                .collect::<Vec<_>>(),
            "{}: key-tier crossings",
            row.label
        );
        if matches!(
            row.route,
            Route::ProcessStart
                | Route::ProcessAwait
                | Route::ProcessCancel
                | Route::ProcessSignal
                | Route::ProcessList
                | Route::ProcessTransfer
        ) {
            assert!(
                run.outcome.ends_with(" ok"),
                "{}: key-addressed process route must succeed",
                row.label
            );
            process_routes_succeeded += 1;
        }
    }
    assert_eq!(
        process_routes_succeeded, 6,
        "all six ToolContext process routes succeed on key-addressed tiers"
    );
}

macro_rules! matrix_row_test {
    ($name:ident, $index:expr) => {
        #[tokio::test]
        async fn $name() {
            assert_row(&ROUTE_MATRIX[$index]).await;
        }
    };
}

matrix_row_test!(attempt_atomicity_row_dispatch_batch, 0);
matrix_row_test!(attempt_atomicity_row_processes_start, 1);
matrix_row_test!(attempt_atomicity_row_processes_await_process, 2);
matrix_row_test!(attempt_atomicity_row_processes_cancel, 3);
matrix_row_test!(attempt_atomicity_row_processes_signal, 4);
matrix_row_test!(attempt_atomicity_row_processes_list_handles_filtered, 5);
matrix_row_test!(attempt_atomicity_row_processes_complete_external, 6);
matrix_row_test!(attempt_atomicity_row_processes_transfer, 7);
matrix_row_test!(attempt_atomicity_row_triggers_emit, 8);
matrix_row_test!(attempt_atomicity_row_sessions_start_turn, 9);
matrix_row_test!(attempt_atomicity_row_sessions_create_close, 10);
matrix_row_test!(attempt_atomicity_row_sessions_reads, 11);
matrix_row_test!(attempt_atomicity_row_direct_completions_complete, 12);
matrix_row_test!(attempt_atomicity_row_attachments_put, 13);
matrix_row_test!(attempt_atomicity_row_process_events_emit, 14);
matrix_row_test!(attempt_atomicity_row_process_events_wait_event_after, 15);
matrix_row_test!(attempt_atomicity_row_completion_key, 16);
matrix_row_test!(attempt_atomicity_row_emit_child_process_started, 17);
matrix_row_test!(attempt_atomicity_row_named_phase, 18);
matrix_row_test!(attempt_atomicity_row_accessors, 19);

/// The catch-all. Every crossing any capability makes from inside a recorded
/// attempt must appear in a declared row's crossing list. A new `ToolContext`
/// capability that journals from inside an attempt trips this without anyone
/// writing a law for it.
#[tokio::test]
async fn sentinel_allows_no_undeclared_crossing_from_inside_an_attempt() {
    let mut observed = Vec::new();
    let mut declared = Vec::new();
    for row in ROUTE_MATRIX {
        let run = run_route(row.route).await;
        observed.extend(run.crossings);
        declared.extend(row.crossings.iter().map(|crossing| (*crossing).to_string()));
    }
    observed.sort();
    declared.sort();
    assert_eq!(
        observed, declared,
        "an undeclared controller crossing escaped a recorded tool attempt; \
         classify the route in ROUTE_MATRIX and prove it at the endpoint tier"
    );
    assert_eq!(
        declared.len(),
        2,
        "two declared ordinal-tier crossings: the registry-served process list and the pure await-event key derivation"
    );
}

/// Anti-vacuity for the sentinel itself: the sentinel must not report a
/// crossing for a command issued outside a recorded attempt, and must report
/// one issued inside.
#[tokio::test]
async fn sentinel_discriminates_inside_and_outside_a_recorded_attempt() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let command = crate::ProcessCommand::Cancel {
        process_id: LIVE_PROCESS.to_string(),
        reason: Some("outside any attempt".to_string()),
    };
    let effect_id = command.effect_id();
    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new(SESSION),
                effect_id.clone(),
                crate::RuntimeEffectKind::Process,
                effect_id,
            ),
            crate::RuntimeEffectCommand::process(command),
        ),
        crate::RuntimeEffectLocalExecutor::processes(Arc::clone(&fixtures.registry), None),
    )
    .await
    .expect("cancel outside an attempt");
    assert!(
        !ledger.tripped(),
        "a command outside any recorded attempt is not a nested emission"
    );
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "no crossings are recorded outside a recorded attempt"
    );

    let inside =
        run_route_on_tier(Route::ProcessCancel, &ControllerOwnedTier::key_addressed()).await;
    assert_eq!(
        inside.crossings,
        vec!["execute_effect:process:process:cancel:attempt-atomicity-live".to_string()],
        "the same command inside a recorded attempt is recorded as nested"
    );
}
