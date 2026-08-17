//! Post-cutover tool-attempt atomicity laws against the
//! [attempt-atomicity sentinel](crate::testing::attempt_sentinel).
//!
//! An ordinal-addressed controller tier records a whole tool attempt as one journal entry
//! and replays that entry on redrive *without re-entering the body*. Any
//! journal command the body emitted while it ran therefore sits in the journal
//! unre-issued, and the handler's next command meets it at the wrong ordinal
//! (Restate `RT0016`). ADR 0042 states the rule; this module is its exhaustive
//! enforcement.
//!
//! The leaf body has only sealed, controller-free `AttemptContext` reads. The
//! sentinel's post-cutover invariant is therefore literal: while a recorded
//! attempt is open, the controller crossing count is zero. A deliberate nested
//! command below proves the sentinel still detects a regression.
//!
//! Intent laws separately pin exactly one attributed command per admitted
//! declaration and zero commands for refused batches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::sync::MutexExt as _;

use crate::ProcessRegistry as _;
use crate::testing::attempt_sentinel::{AttemptAtomicitySentinel, NestedJournalLedger};

const SESSION: &str = "atomic-tool-test-session";
const TURN: &str = "attempt-atomicity-turn";
const ATTEMPT_EFFECT_ID: &str = "attempt-atomicity-attempt";
const CALL_ID: &str = "attempt-atomicity-call";
const LIVE_PROCESS: &str = "attempt-atomicity-live";
const TERMINAL_PROCESS: &str = "attempt-atomicity-terminal";
const EXTERNAL_PROCESS: &str = "attempt-atomicity-external";
const DIRECT_MODEL: &str = "mock-model";
const DIRECT_TEXT: &str = "unstubbed direct answer";
const FOLLOW_ON_EFFECT_ID: &str = "attempt-atomicity-follow-on";

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

struct Fixtures {
    host: Arc<crate::testing::MockSessionManager>,
    registry: Arc<dyn crate::ProcessRegistry>,
    trigger_store: Arc<crate::facade_support::InMemoryTriggerStore>,
    lease: crate::ProcessLease,
    child_process_starts: Arc<AtomicUsize>,
    /// A real runtime, kept alive so the direct-completion client handed to the
    /// matrix is the production one. A stubbed client answers before the
    /// position classification runs, which would make every direct-completion
    /// row pass by construction instead of exercising the routing decision.
    runtime: crate::runtime::LashRuntime,
}

fn direct_mock_call() -> super::helpers::MockCall {
    super::helpers::MockCall {
        stream_events: Vec::new(),
        response: Ok(crate::LlmResponse {
            full_text: DIRECT_TEXT.to_string(),
            ..crate::LlmResponse::default()
        }),
    }
}

async fn fixtures() -> Fixtures {
    let runtime = super::helpers::runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(crate::testing::EmptyToolProvider),
        super::helpers::mock_provider(vec![direct_mock_call(), direct_mock_call()]),
        crate::runtime::EmbeddedRuntimeHost::new(super::helpers::test_runtime_host_config()),
    )
    .await;
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
        runtime,
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
    tool_context_with_provider(
        scoped,
        fixtures,
        Arc::new(crate::testing::EmptyToolProvider),
    )
}

fn tool_context_with_provider<'run>(
    scoped: crate::ScopedEffectController<'run>,
    fixtures: &Fixtures,
    tools: Arc<dyn crate::ToolProvider>,
) -> crate::ToolContext<'run> {
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let plugins = crate::plugin::PluginHost::new(Vec::new())
        .build_session(SESSION, None)
        .expect("build attempt-atomicity plugin session");
    let processes = crate::testing::effect_backed_process_service(Arc::clone(&fixtures.registry));
    let child_process_starts = Arc::clone(&fixtures.child_process_starts);
    let effect_controller = crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped);
    // The production client, minted against the very controller the sentinel
    // wraps: an `Independent` classification therefore shows up in the ledger
    // as a real crossing instead of being swallowed by a stub.
    let direct_completions = fixtures
        .runtime
        .runtime_session_services()
        .expect("attempt-atomicity session manager")
        .direct_completion_client(effect_controller.clone(), Some(TURN.to_string()));
    let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
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
        effect_controller,
        direct_completions,
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
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
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
            crate::DeliveryPolicy::EarliestSafeBoundary,
            Arc::new(crate::SystemClock),
        )
        .build()
}

struct LegacyRoutingProbeProvider {
    execute_by_id_calls: AtomicUsize,
}

impl LegacyRoutingProbeProvider {
    fn new() -> Self {
        Self {
            execute_by_id_calls: AtomicUsize::new(0),
        }
    }

    fn definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:attempt_atomicity",
            "attempt_atomicity",
            "",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "string"}),
        )
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for LegacyRoutingProbeProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "attempt_atomicity").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
        panic!("provider-routing law must enter the execute_by_id override")
    }

    async fn execute_by_id(
        &self,
        tool_id: &crate::ToolId,
        _args: &serde_json::Value,
        context: &crate::ToolContext<'_>,
    ) -> crate::ToolResult {
        assert_eq!(tool_id, &crate::ToolId::new("tool:attempt_atomicity"));
        self.execute_by_id_calls.fetch_add(1, Ordering::SeqCst);

        let batch = context
            .dispatch()
            .batch(vec![crate::ToolInvocation::new(
                "legacy-routing-batch",
                crate::ToolId::new("noop"),
                serde_json::Value::Null,
            )])
            .await;
        assert_eq!(batch.len(), 1);
        assert!(
            batch[0].output.value_for_projection()["message"]
                .as_str()
                .is_some_and(
                    |message| message.contains("unavailable inside an atomic tool attempt")
                )
        );

        let nested_turn = context
            .sessions()
            .start_turn(
                "legacy-routing-child",
                "legacy-routing-child-turn",
                crate::TurnInput::text("nested turn"),
            )
            .await
            .expect_err("provider-routed nested turn is guarded");
        assert!(
            nested_turn
                .to_string()
                .contains("unavailable inside an atomic tool attempt")
        );

        let trigger = context
            .triggers()
            .emit(crate::TriggerOccurrenceRequest::new(
                "attempt-atomicity.trigger",
                "attempt-atomicity-source",
                serde_json::json!({}),
                "legacy-routing-occurrence",
            ))
            .await
            .expect_err("provider-routed trigger emission is guarded");
        assert!(
            trigger
                .to_string()
                .contains("unavailable inside an atomic tool attempt")
        );

        crate::ToolResult::ok(serde_json::json!("legacy execute_by_id ran"))
    }
}

/// The post-cutover catch-all. A leaf body receives only `AttemptContext`; all
/// of its process and session reads bypass the effect controller, so the exact
/// crossing count while the attempt is open is zero.
#[tokio::test]
async fn sentinel_allows_no_undeclared_crossing_from_inside_an_attempt() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped post-cutover sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            crate::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let attempt = crate::AttemptContext::__for_testing(&tool, TURN);
            let _ = attempt.sessions().snapshot_current().await;
            let _ = attempt
                .processes()
                .list_handles_filtered(&crate::ProcessListFilter::default())
                .await;
            let _ = attempt.session_id();
            let _ = attempt.execution_scope_id();
            // FIG-1486: the sealed leaf environment also owns a direct-completion
            // capability. It carries the recorded attempt's invocation, so the
            // completion runs locally; classified `Independent` it would journal
            // a second entry inside the attempt and wedge the redrive.
            assert_eq!(
                attempt
                    .direct_completions()
                    .complete(
                        crate::DirectRequest::text(DIRECT_MODEL, "attempt direct completion"),
                        "attempt-atomicity",
                    )
                    .await
                    .expect("attempt-context direct completion stays local")
                    .text,
                DIRECT_TEXT
            );
            Ok(crate::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(crate::ToolAttemptLaunch::Done {
                    record: Box::new(crate::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: crate::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: crate::ToolIntents::default(),
                }),
                triggers: Vec::new(),
            })
        }),
    )
    .await
    .expect("sanctioned leaf attempt completes");
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "post-cutover leaf capabilities produce exactly zero controller crossings"
    );
}

/// Providers that have not opted into `AttemptContext` still receive the
/// legacy `ToolContext`. Exercise every surviving journal-capable route and its
/// journal-free capability inventory inside a real recorded attempt so the
/// ordinal guards remain end-to-end, not merely module-local.
#[tokio::test]
async fn legacy_tool_context_guards_and_journal_free_routes_hold_inside_recorded_attempt() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped legacy ToolContext sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    let child_process_starts = Arc::clone(&fixtures.child_process_starts);

    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            crate::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let batch = tool
                .dispatch()
                .batch(vec![crate::ToolInvocation::new(
                    "legacy-batch",
                    crate::ToolId::new("noop"),
                    serde_json::Value::Null,
                )])
                .await;
            assert_eq!(batch.len(), 1);
            assert!(
                batch[0].output.value_for_projection()["message"]
                    .as_str()
                    .is_some_and(
                        |message| message.contains("unavailable inside an atomic tool attempt")
                    )
            );

            let nested_turn = tool
                .sessions()
                .start_turn(
                    "legacy-child",
                    "legacy-child-turn",
                    crate::TurnInput::text("nested turn"),
                )
                .await
                .expect_err("ordinal legacy nested turn is guarded");
            assert!(
                nested_turn
                    .to_string()
                    .contains("unavailable inside an atomic tool attempt")
            );

            let trigger = tool
                .triggers()
                .emit(crate::TriggerOccurrenceRequest::new(
                    "attempt-atomicity.trigger",
                    "attempt-atomicity-source",
                    serde_json::json!({}),
                    "legacy-attempt-occurrence",
                ))
                .await
                .expect_err("ordinal legacy trigger emission is guarded");
            assert!(
                trigger
                    .to_string()
                    .contains("unavailable inside an atomic tool attempt")
            );

            let sessions = tool.sessions();
            sessions
                .snapshot_current()
                .await
                .expect("current snapshot read");
            sessions
                .snapshot(SESSION)
                .await
                .expect("named snapshot read");
            sessions.model().await.expect("model read");
            sessions.tool_catalog().await.expect("catalog read");
            sessions
                .shared_tool_catalog()
                .await
                .expect("shared catalog read");
            assert!(
                sessions
                    .set_tool_membership(&["missing-tool".to_string()], true)
                    .await
                    .is_err(),
                "membership reaches the journal-free registry and refuses the missing tool"
            );

            tool.attachments()
                .put(
                    vec![1, 2, 3, 4],
                    crate::AttachmentCreateMeta::new(
                        crate::MediaType::parse("image/png").expect("png media type"),
                        Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                        Some("legacy-attempt.png".to_string()),
                    ),
                )
                .await
                .expect("attachment write stays inside the attempt body");
            assert_eq!(
                tool.direct_completions()
                    .complete(
                        crate::DirectRequest::text(DIRECT_MODEL, "legacy direct completion"),
                        "attempt-atomicity",
                    )
                    .await
                    .expect("direct completion stays local")
                    .text,
                DIRECT_TEXT
            );

            let events = tool.process_events();
            events
                .emit("attempt.atomicity.note", serde_json::json!({"n": 1}))
                .await
                .expect("registry-authorized event append");
            events
                .emit_request(crate::ProcessEventAppendRequest::new(
                    "attempt.atomicity.awaited",
                    serde_json::json!({"n": 2}),
                ))
                .await
                .expect("registry-authorized request append");
            events
                .wait_event_after("attempt.atomicity.awaited", 0)
                .await
                .expect("registry-authorized event wait");

            tool.emit_child_process_started(LIVE_PROCESS, Some("legacy child".to_string()));
            assert_eq!(child_process_starts.load(Ordering::SeqCst), 1);
            let _phase = tool.named_phase("legacy-tool-context-sentinel");
            assert_eq!(tool.session_id(), SESSION);
            assert_eq!(tool.tool_call_id(), Some(CALL_ID));
            assert!(tool.cancellation_token().is_some());
            assert_eq!(tool.prepared_payload(), &serde_json::Value::Null);

            Ok(crate::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(crate::ToolAttemptLaunch::Done {
                    record: Box::new(crate::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: crate::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: crate::ToolIntents::default(),
                }),
                triggers: Vec::new(),
            })
        }),
    )
    .await
    .expect("legacy ToolContext inventory completes under its guards");
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "legacy guards refuse before the controller and journal-free routes stay local"
    );
}

#[tokio::test]
async fn default_false_provider_routes_through_execute_once_without_controller_crossing() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped provider-routing sentinel controller");
    let provider = Arc::new(LegacyRoutingProbeProvider::new());
    let tool = tool_context_with_provider(
        scoped,
        &fixtures,
        Arc::clone(&provider) as Arc<dyn crate::ToolProvider>,
    );

    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            crate::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let dispatch = Arc::clone(
                tool.runtime_dispatch
                    .as_ref()
                    .expect("tool context carries runtime dispatch"),
            );
            let result =
                crate::tool_dispatch::execute_once(dispatch.as_ref(), &prepared_tool_call(), tool)
                    .await;
            assert!(matches!(result, crate::ToolAttemptResult::Done { .. }));
            Ok(crate::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(crate::ToolAttemptLaunch::Done {
                    record: Box::new(crate::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: crate::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: crate::ToolIntents::default(),
                }),
                triggers: Vec::new(),
            })
        }),
    )
    .await
    .expect("default-false provider completes through execute_once");

    assert_eq!(provider.execute_by_id_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "all three legacy routes refuse before crossing the controller"
    );
}

/// Red proof for the sentinel itself: a deliberately leaked test-only command
/// must be caught while an attempt body is open.
#[tokio::test]
async fn sentinel_test_only_leak_trips_inside_a_recorded_attempt() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let command = crate::ProcessCommand::Cancel {
        process_id: LIVE_PROCESS.to_string(),
        reason: Some("outside any attempt".to_string()),
        replay: None,
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

    let registry = Arc::clone(&fixtures.registry);
    let nested_sentinel = &sentinel;
    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            crate::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let command = crate::ProcessCommand::Cancel {
                process_id: LIVE_PROCESS.to_string(),
                reason: Some("test-only sentinel leak".to_string()),
                replay: None,
            };
            let effect_id = command.effect_id();
            crate::RuntimeEffectController::execute_effect(
                nested_sentinel,
                crate::RuntimeEffectEnvelope::new(
                    crate::RuntimeInvocation::effect(
                        crate::RuntimeScope::new(SESSION),
                        effect_id.clone(),
                        crate::RuntimeEffectKind::Process,
                        effect_id,
                    ),
                    crate::RuntimeEffectCommand::process(command),
                ),
                crate::RuntimeEffectLocalExecutor::processes(registry, None),
            )
            .await?;
            Ok(crate::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(crate::ToolAttemptLaunch::Done {
                    record: Box::new(crate::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: crate::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: crate::ToolIntents::default(),
                }),
                triggers: Vec::new(),
            })
        }),
    )
    .await
    .expect("test-only nested leak executes");
    assert_eq!(
        ledger.crossings_inside_attempt(),
        vec!["execute_effect:process:process:cancel:attempt-atomicity-live".to_string()],
        "the literal test-only leak proves the sentinel fails red when a command escapes"
    );
}

/// Each admitted v1 declaration realizes exactly one controller command, and
/// the sentinel attributes that command to the literal stable intent id.
#[tokio::test]
async fn sentinel_records_exactly_one_crossing_per_tool_intent() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::key_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped intent sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    let mut dispatch = tool
        .runtime_dispatch
        .as_ref()
        .map(|context| context.as_ref().clone())
        .expect("runtime dispatch context");
    dispatch.parent_invocation = Some(crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "intent-drain",
        crate::RuntimeEffectKind::ToolBatch,
        "intent-drain",
    ));

    let intents = crate::ToolIntents::v1(vec![
        crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
            session_id: SESSION.to_string(),
            request: crate::ProcessStartRequest::external(
                "ignored-by-stable-intent-id",
                crate::ProcessOriginator::host_scoped("intent-test"),
                serde_json::json!({"step": "start"}),
            ),
            on_parent_end: crate::ProcessParentEndPolicy::Abandon,
        })),
        crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
            session_id: SESSION.to_string(),
            process_id: LIVE_PROCESS.to_string(),
            signal_name: "resume".to_string(),
            payload: serde_json::json!({"step": "signal"}),
        }),
        crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
            session_id: SESSION.to_string(),
            process_id: LIVE_PROCESS.to_string(),
            event_type: "attempt.atomicity.note".to_string(),
            payload: serde_json::json!({"step": "event"}),
        }),
        crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
            session_id: SESSION.to_string(),
            process_id: LIVE_PROCESS.to_string(),
            reason: Some("intent test complete".to_string()),
        }),
    ]);
    let outcomes =
        crate::tool_dispatch::execute_final_tool_intents(&dispatch, Some(CALL_ID), &intents, None)
            .await;
    assert_eq!(outcomes.len(), 4, "one typed outcome per intent");
    let literal_ids = [
        "tool-intent:v1:sha256:d637b38a6e29a2fdba6263273a49fe3cfc55b37ec8196c90a14293e46911cfde",
        "tool-intent:v1:sha256:7dd01aae6fbd504ff82c32241f5070a31c25ff685469d12ff2c7b179bcc88a50",
        "tool-intent:v1:sha256:96b311b993750b668dc3d87b7b2697d132ce6fe8f2b5f102c7f20f089220d64a",
        "tool-intent:v1:sha256:d4e5390c7ffdc00fde49a69219a623f95c49146c8865ac27c20b10aa713325bc",
    ];
    let actual_ids = outcomes
        .iter()
        .map(|outcome| match outcome {
            crate::ToolIntentExecutionOutcome::Executed { identity, .. } => {
                identity.replay_key.as_str()
            }
            crate::ToolIntentExecutionOutcome::Refused { refusal, .. } => {
                panic!("fixture intent was refused: {refusal:?}")
            }
            crate::ToolIntentExecutionOutcome::ProtocolRefused { refusal } => {
                panic!("fixture batch was refused: {refusal:?}")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(actual_ids, literal_ids);
    for literal_id in literal_ids {
        assert_eq!(
            ledger.crossings_for_intent(literal_id).len(),
            1,
            "intent {literal_id} must issue exactly one command"
        );
    }
}

/// Literal overflow law: admission refuses the complete recorded batch and no
/// process command reaches the controller.
#[tokio::test]
async fn over_budget_intent_batch_refuses_every_intent_and_executes_zero_commands() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::key_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped overflow sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    let dispatch = tool
        .runtime_dispatch
        .as_ref()
        .map(|context| context.as_ref().clone())
        .expect("runtime dispatch context");
    let intents = crate::ToolIntents::v1(
        (0..=crate::TOOL_INTENT_MAX_COUNT)
            .map(|index| {
                crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
                    session_id: SESSION.to_string(),
                    process_id: LIVE_PROCESS.to_string(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"index": index}),
                })
            })
            .collect(),
    );
    let outcomes =
        crate::tool_dispatch::execute_final_tool_intents(&dispatch, Some(CALL_ID), &intents, None)
            .await;
    assert_eq!(outcomes.len(), 33, "every declaration gets a refusal");
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        crate::ToolIntentExecutionOutcome::Refused {
            refusal: crate::ToolIntentRefusalReason::CountBudgetExceeded {
                actual: 33,
                maximum: 32,
            },
            ..
        }
    )));
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "the drain is outside the attempt body"
    );
    for outcome in outcomes {
        let identity = match outcome {
            crate::ToolIntentExecutionOutcome::Refused {
                identity: Some(identity),
                ..
            } => identity,
            other => panic!("expected identity-bearing refusal, got {other:?}"),
        };
        assert_eq!(
            ledger.crossings_for_intent(&identity.replay_key),
            Vec::<String>::new(),
            "over-budget admission issues zero commands"
        );
    }
}

#[tokio::test]
async fn sentinel_uses_structural_intent_attribution_and_missing_metadata_overcounts() {
    let tier = ControllerOwnedTier::key_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let identity = crate::derive_tool_intent_identity(SESSION, TURN, Some(CALL_ID), 9)
        .expect("literal intent identity");
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    registry
        .register_process(
            crate::ProcessRegistration::new(
                "structural-process",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types([crate::ProcessEventType {
                name: "structural.note".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("register structural attribution target");
    let command = crate::ProcessCommand::EmitEvent {
        process_id: "structural-process".to_string(),
        request: crate::ProcessEventAppendRequest::new(
            "structural.note",
            serde_json::json!({"law": "overcount"}),
        )
        .with_replay_key("structural-attribution-event"),
    };
    let mut attributed = crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "structurally-attributed-command",
        crate::RuntimeEffectKind::Process,
        "plain-unprefixed-key",
    );
    attributed.replay = Some(crate::RuntimeReplay {
        key: "plain-unprefixed-key".to_string(),
        attribution: Some(crate::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        )),
    });
    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            attributed,
            crate::RuntimeEffectCommand::process(command.clone()),
        ),
        crate::RuntimeEffectLocalExecutor::processes(registry.clone(), None),
    )
    .await
    .expect("unprefixed command executes");
    assert_eq!(
        ledger.crossings_for_intent(&identity.replay_key),
        vec!["execute_effect:process:structurally-attributed-command".to_string()]
    );

    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        crate::RuntimeEffectEnvelope::new(
            crate::RuntimeInvocation::effect(
                crate::RuntimeScope::for_turn(SESSION, TURN, 0, 0),
                "missing-attribution-command",
                crate::RuntimeEffectKind::Process,
                "another-plain-key",
            ),
            crate::RuntimeEffectCommand::process(command),
        ),
        crate::RuntimeEffectLocalExecutor::processes(registry, None),
    )
    .await
    .expect("unattributed command executes");
    assert_eq!(
        ledger.crossings_for_intent(&identity.replay_key),
        vec![
            "execute_effect:process:structurally-attributed-command".to_string(),
            "execute_effect:process:missing-attribution-command".to_string(),
        ],
        "missing structural metadata fails the one-command law by over-counting"
    );
}

#[tokio::test]
async fn journal_first_redrive_ignores_live_terminal_mutation_and_replays_identical_bytes() {
    let fixtures = fixtures().await;
    let controller = super::effect::RecordingEffectController::default().with_replay_by_key();
    let scoped = crate::ScopedEffectController::borrowed(
        &controller,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped replaying controller");
    let tool = tool_context(scoped, &fixtures);
    let dispatch = tool
        .runtime_dispatch
        .as_ref()
        .map(|context| context.as_ref().clone())
        .expect("runtime dispatch context");
    let intents = crate::ToolIntents::v1(vec![crate::ToolIntent::SignalProcess(
        crate::SignalProcessIntent {
            session_id: SESSION.to_string(),
            process_id: LIVE_PROCESS.to_string(),
            signal_name: "resume".to_string(),
            payload: serde_json::json!({"recorded": "payload"}),
        },
    )]);

    let first =
        crate::tool_dispatch::execute_final_tool_intents(&dispatch, Some(CALL_ID), &intents, None)
            .await;
    let first_bytes = serde_json::to_vec(&first).expect("serialize first intent outcome");
    assert!(
        matches!(
            first.as_slice(),
            [crate::ToolIntentExecutionOutcome::Executed {
                kind: crate::ToolIntentKind::SignalProcess,
                ..
            }]
        ),
        "expected recorded signal execution, got {first:?}"
    );
    let command_frames = controller.envelopes();
    assert_eq!(command_frames.len(), 1, "one command frame on first drain");

    fixtures
        .registry
        .complete_process(
            LIVE_PROCESS,
            crate::ProcessAwaitOutput::Success {
                value: serde_json::json!("terminal after first drain"),
                control: None,
            },
            crate::ProcessCompletionAuthority::workflow_key("live-mutation-law"),
        )
        .await
        .expect("mutate live target to terminal");

    let redriven =
        crate::tool_dispatch::execute_final_tool_intents(&dispatch, Some(CALL_ID), &intents, None)
            .await;
    assert_eq!(
        serde_json::to_vec(&redriven).expect("serialize redriven intent outcome"),
        first_bytes,
        "the recorded command outcome is byte-identical after live mutation"
    );
    let redriven_frames = controller.envelopes();
    assert_eq!(
        redriven_frames, command_frames,
        "redrive reuses the recorded command frame instead of taking a live-state branch"
    );
}

/// One recorded journal entry: the ordinal identity a redrive compares against,
/// plus the outcome the entry replays.
struct JournalEntry {
    identity: String,
    /// `None` until the command settles: a real journal holds the command at
    /// its ordinal from the moment it is issued, not from the moment it
    /// completes.
    outcome: Option<crate::RuntimeEffectOutcome>,
}

/// An ordinal-addressed journal with the two behaviors that make the FIG-1486
/// wedge reachable: a recorded entry replays *without* re-entering its body,
/// and a command that meets a different recorded entry at its ordinal is
/// refused the way an ordinal-addressed engine refuses it (Restate `RT0016`).
struct OrdinalJournaledTier {
    inner: crate::InlineRuntimeEffectController,
    journal: std::sync::Mutex<Vec<JournalEntry>>,
    replaying: std::sync::atomic::AtomicBool,
    cursor: AtomicUsize,
}

impl OrdinalJournaledTier {
    fn recording() -> Self {
        Self {
            inner: crate::InlineRuntimeEffectController::default(),
            journal: std::sync::Mutex::new(Vec::new()),
            replaying: std::sync::atomic::AtomicBool::new(false),
            cursor: AtomicUsize::new(0),
        }
    }

    /// Drops the first incarnation and hands the recorded journal to a fresh
    /// one, which re-issues the same commands from the top.
    fn start_redrive(&self) {
        self.cursor.store(0, Ordering::SeqCst);
        self.replaying.store(true, Ordering::SeqCst);
    }

    fn journal_identities(&self) -> Vec<String> {
        self.journal
            .lock_recover()
            .iter()
            .map(|entry| entry.identity.clone())
            .collect()
    }

    fn identity(envelope: &crate::RuntimeEffectEnvelope) -> String {
        let kind = envelope
            .invocation
            .effect_kind()
            .map(crate::RuntimeEffectKind::as_str)
            .unwrap_or("no_effect_kind");
        let effect_id = envelope.invocation.effect_id().unwrap_or("no_effect_id");
        format!("{kind}:{effect_id}")
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for OrdinalJournaledTier {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        crate::EffectReplayOwnership::Controller
    }

    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        crate::EffectJournalAddressing::OrdinalAddressed
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
impl crate::RuntimeEffectController for OrdinalJournaledTier {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let identity = Self::identity(&envelope);
        if self.replaying.load(Ordering::SeqCst) {
            let ordinal = self.cursor.fetch_add(1, Ordering::SeqCst);
            let journal = self.journal.lock_recover();
            let Some(entry) = journal.get(ordinal) else {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RestateEffectHashMismatch,
                    format!("RT0016: journal ended before ordinal {ordinal} (`{identity}`)"),
                ));
            };
            if entry.identity != identity {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RestateEffectHashMismatch,
                    format!(
                        "RT0016: journal mismatch at ordinal {ordinal}: recorded `{}`, handler issued `{identity}`",
                        entry.identity
                    ),
                ));
            }
            let Some(outcome) = entry.outcome.clone() else {
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RestateEffectHashMismatch,
                    format!("RT0016: recorded entry `{identity}` never settled"),
                ));
            };
            return Ok(outcome);
        }
        // The command occupies its ordinal from the moment it is issued, so a
        // command emitted from inside another command's body lands after it.
        let ordinal = {
            let mut journal = self.journal.lock_recover();
            journal.push(JournalEntry {
                identity,
                outcome: None,
            });
            journal.len() - 1
        };
        let outcome = self.inner.execute_effect(envelope, local_executor).await?;
        self.journal.lock_recover()[ordinal].outcome = Some(outcome.clone());
        Ok(outcome)
    }
}

fn follow_on_invocation() -> crate::RuntimeInvocation {
    crate::RuntimeInvocation::effect(
        crate::RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        FOLLOW_ON_EFFECT_ID,
        crate::RuntimeEffectKind::Sleep,
        FOLLOW_ON_EFFECT_ID,
    )
}

fn attempt_effect_envelope() -> crate::RuntimeEffectEnvelope {
    crate::RuntimeEffectEnvelope::new(
        attempt_invocation(),
        crate::RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call(),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

fn attempt_done_outcome() -> crate::RuntimeEffectOutcome {
    crate::RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(crate::ToolAttemptLaunch::Done {
            record: Box::new(crate::ToolCallRecord {
                call_id: Some(CALL_ID.to_string()),
                tool: "attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                output: crate::ToolCallOutput::success(serde_json::json!("ok")),
                duration_ms: 0,
            }),
            intents: crate::ToolIntents::default(),
        }),
        triggers: Vec::new(),
    }
}

/// FIG-1486's interleaving, end to end on an ordinal-addressed journal: a
/// recorded attempt whose body issues a direct completion is crashed after the
/// attempt settles and redriven by a fresh incarnation. The replayed attempt
/// does not re-enter its body, so a direct entry journaled from inside it would
/// still sit at the next ordinal and wedge the following command with `RT0016`.
#[tokio::test]
async fn direct_completion_inside_a_recorded_attempt_redrives_without_a_journal_mismatch() {
    let fixtures = fixtures().await;
    let tier = OrdinalJournaledTier::recording();
    let bodies_entered = Arc::new(AtomicUsize::new(0));

    let first_incarnation_bodies = Arc::clone(&bodies_entered);
    let scoped =
        crate::ScopedEffectController::borrowed(&tier, crate::ExecutionScope::turn(SESSION, TURN))
            .expect("scoped ordinal-journaled controller");
    let tool = tool_context(scoped, &fixtures);
    crate::RuntimeEffectController::execute_effect(
        &tier,
        attempt_effect_envelope(),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            first_incarnation_bodies.fetch_add(1, Ordering::SeqCst);
            let attempt = crate::AttemptContext::__for_testing(&tool, TURN);
            assert_eq!(
                attempt
                    .direct_completions()
                    .complete(
                        crate::DirectRequest::text(DIRECT_MODEL, "redrive direct completion"),
                        "attempt-atomicity",
                    )
                    .await
                    .expect("attempt-context direct completion")
                    .text,
                DIRECT_TEXT
            );
            Ok(attempt_done_outcome())
        }),
    )
    .await
    .expect("first incarnation records the attempt");
    // The command the handler issues once the attempt settles. On redrive it
    // must meet the attempt's successor ordinal, not an entry the body left
    // behind.
    crate::RuntimeEffectController::execute_effect(
        &tier,
        crate::RuntimeEffectEnvelope::new(
            follow_on_invocation(),
            crate::RuntimeEffectCommand::Sleep { duration_ms: 0 },
        ),
        crate::RuntimeEffectLocalExecutor::testing(|_envelope| async {
            Ok(crate::RuntimeEffectOutcome::Sleep)
        }),
    )
    .await
    .expect("first incarnation records the follow-on command");
    assert_eq!(bodies_entered.load(Ordering::SeqCst), 1);

    tier.start_redrive();
    let redriven_bodies = Arc::clone(&bodies_entered);
    let redriven_scoped =
        crate::ScopedEffectController::borrowed(&tier, crate::ExecutionScope::turn(SESSION, TURN))
            .expect("scoped redrive controller");
    let redriven_tool = tool_context(redriven_scoped, &fixtures);
    let replayed = crate::RuntimeEffectController::execute_effect(
        &tier,
        attempt_effect_envelope(),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            redriven_bodies.fetch_add(1, Ordering::SeqCst);
            let _attempt = crate::AttemptContext::__for_testing(&redriven_tool, TURN);
            Ok(attempt_done_outcome())
        }),
    )
    .await
    .expect("redrive replays the recorded attempt");
    assert!(matches!(
        replayed,
        crate::RuntimeEffectOutcome::ToolAttempt { .. }
    ));
    assert_eq!(
        bodies_entered.load(Ordering::SeqCst),
        1,
        "the recorded attempt replays without re-entering its body"
    );

    let follow_on = crate::RuntimeEffectController::execute_effect(
        &tier,
        crate::RuntimeEffectEnvelope::new(
            follow_on_invocation(),
            crate::RuntimeEffectCommand::Sleep { duration_ms: 0 },
        ),
        crate::RuntimeEffectLocalExecutor::testing(|_envelope| async {
            Ok(crate::RuntimeEffectOutcome::Sleep)
        }),
    )
    .await;
    assert!(
        follow_on.is_ok(),
        "the invocation must complete after redrive instead of wedging: {:?}",
        follow_on.err().map(|error| error.to_string())
    );
    assert_eq!(
        tier.journal_identities(),
        vec![
            format!("tool_attempt:{ATTEMPT_EFFECT_ID}"),
            format!("sleep:{FOLLOW_ON_EFFECT_ID}"),
        ],
        "a recorded attempt owns exactly one entry; a direct completion from its body adds none"
    );
}

fn direct_llm_request(request_id: &str) -> crate::LlmRequest {
    crate::LlmRequest {
        model: DIRECT_MODEL.to_string(),
        messages: vec![crate::llm::types::LlmMessage::new(
            crate::llm::types::LlmRole::User,
            vec![crate::llm::types::LlmContentBlock::Text {
                text: Arc::from("attempt direct llm completion"),
                response_meta: None,
                cache_breakpoint: false,
            }],
        )],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: crate::llm::types::LlmToolChoice::None,
        model_variant: Default::default(),
        model_capability: crate::ModelCapability::default(),
        scope: crate::LlmRequestScope::new(SESSION, format!("{SESSION}:frame"), request_id),
        output_spec: None,
        stream_events: None,
        generation: crate::GenerationOptions::default(),
        provider_trace: None,
    }
}

/// `direct_llm_completion` has no tool-attributed entry point, so it classifies
/// its journal position from the invocation its client was minted inside. A
/// client derived for a recorded attempt — as the attempt-scoped dispatch
/// derives it in production — must keep the full-output direct call local too.
#[tokio::test]
async fn attempt_scoped_client_keeps_direct_llm_completions_out_of_the_journal() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = crate::ScopedEffectController::borrowed(
        &sentinel,
        crate::ExecutionScope::turn(SESSION, TURN),
    )
    .expect("scoped direct-llm sentinel controller");
    let direct_completions = fixtures
        .runtime
        .runtime_session_services()
        .expect("attempt-atomicity session manager")
        .direct_completion_client(
            crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped),
            Some(TURN.to_string()),
        )
        .with_parent_invocation(Some(attempt_invocation()));

    crate::RuntimeEffectController::execute_effect(
        &sentinel,
        attempt_effect_envelope(),
        crate::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let completion = direct_completions
                .direct_llm_completion(
                    direct_llm_request("attempt-atomicity:direct-llm"),
                    "attempt-atomicity",
                )
                .await
                .expect("attempt-scoped direct llm completion");
            assert_eq!(completion.response.full_text, DIRECT_TEXT);
            Ok(attempt_done_outcome())
        }),
    )
    .await
    .expect("attempt completes with a local direct llm completion");

    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "an attempt-scoped client journals no direct entry from inside the attempt"
    );
}
