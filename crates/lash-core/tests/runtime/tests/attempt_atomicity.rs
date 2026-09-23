//! Post-cutover tool-attempt atomicity laws against the
//! [attempt-atomicity sentinel](lash_core::testing::attempt_sentinel).
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

use lash_core::ProcessId;
use lash_core::ProcessRegistrar as _;
use lash_core::SessionId;
use lash_core::TurnId;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::sync::MutexExt as _;

use lash_core::testing::attempt_sentinel::{AttemptAtomicitySentinel, NestedJournalLedger};

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

/// A controller-owned tier stand-in.
struct ControllerOwnedTier {
    inner: lash_core::facade_support::NativeRuntimeEffectController,
}

impl ControllerOwnedTier {
    fn ordinal_addressed() -> Self {
        Self {
            inner: lash_core::facade_support::NativeRuntimeEffectController::default(),
        }
    }

    fn key_addressed() -> Self {
        Self {
            inner: lash_core::facade_support::NativeRuntimeEffectController::default(),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for ControllerOwnedTier {
    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        if !may_defer {
            return Ok(lash_core::CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(lash_core::CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for ControllerOwnedTier {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        lash_core::EffectJournaling::Journaled
    }

    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.inner.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: std::sync::Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }
    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

struct Fixtures {
    /// The backend whose registry and process-exec-env store the matrix
    /// runs over; held so its in-memory databases outlive every row.
    backend: lash_sqlite_store::SqliteBackend,
    host: Arc<lash_core::testing::MockSessionManager>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    trigger_store: Arc<lash_core::facade_support::InMemoryTriggerStore>,
    lease: lash_core::ProcessLease,
    child_process_starts: Arc<AtomicUsize>,
    /// A real runtime, kept alive so the direct-completion client handed to the
    /// matrix is the production one. A stubbed client answers before the
    /// position classification runs, which would make every direct-completion
    /// row pass by construction instead of exercising the routing decision.
    runtime: lash_core::runtime::LashRuntime,
}

fn direct_mock_call() -> super::helpers::MockCall {
    super::helpers::MockCall {
        stream_events: Vec::new(),
        response: Ok(lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: DIRECT_TEXT.to_string(),
                response_meta: None,
            }],
            ..lash_core::LlmResponse::default()
        }),
    }
}

async fn fixtures() -> Fixtures {
    let runtime = Box::pin(super::helpers::runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(lash_core::testing::EmptyToolProvider),
        super::helpers::mock_provider(vec![direct_mock_call(), direct_mock_call()]),
        lash_core::runtime::EmbeddedRuntimeHost::new(super::helpers::test_runtime_host_config()),
    ))
    .await;
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("memory backend");
    let registry = lash_core::Backend::process_registry(&backend);
    let host = Arc::new(
        lash_core::testing::MockSessionManager::default()
            .with_process_registry(Arc::clone(&registry))
            .with_tool_registry(
                lash_core::ToolRegistry::from_tool_provider(Arc::new(
                    lash_core::testing::EmptyToolProvider,
                ))
                .expect("empty tool registry"),
            ),
    );
    let event_types = [
        "attempt.atomicity.note",
        "attempt.atomicity.awaited",
        "signal.resume",
    ]
    .into_iter()
    .map(|name| lash_core::ProcessEventType {
        name: name.to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec::default(),
    })
    .collect::<Vec<_>>();
    for (id, disposition) in [
        (LIVE_PROCESS, lash_core::RecoveryContract::Rerunnable),
        (
            TERMINAL_PROCESS,
            lash_core::RecoveryContract::ExternallyOwned,
        ),
        (
            EXTERNAL_PROCESS,
            lash_core::RecoveryContract::ExternallyOwned,
        ),
    ] {
        registry
            .register_process_with_observers(
                lash_core::ProcessRegistration::new(
                    id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    disposition,
                    lash_core::ProcessProvenance::host(),
                    lash_core::ProcessLifecyclePolicy::new(
                        lash_core::ParentScope::Host,
                        lash_core::OnParentEnd::Abandon,
                    ),
                )
                .with_extra_event_types(event_types.clone()),
                &[SessionId::from(SESSION.to_string())],
            )
            .await
            .expect("register matrix process");
    }
    registry
        .complete_process(
            &ProcessId::from(TERMINAL_PROCESS),
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete terminal matrix process");
    let owner = lash_core::LeaseOwnerIdentity::opaque("attempt-atomicity", "incarnation");
    let lease = registry
        .claim_process_lease(&ProcessId::from(LIVE_PROCESS), &owner, 60_000)
        .await
        .expect("claim live process lease")
        .acquired()
        .expect("live process lease");
    registry
        .record_first_started_with_authority(
            &ProcessId::from(LIVE_PROCESS),
            lash_core::ProcessStarted {
                owner,
                fencing_token: lease.fencing_token,
                attempt: 1,
                started_at_ms: 1,
            },
            &lash_core::ProcessExecutionWriteAuthority::lease(lease.clone()),
        )
        .await
        .expect("start live matrix process");
    let trigger_store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    Fixtures {
        backend,
        host,
        registry,
        trigger_store,
        lease,
        child_process_starts: Arc::new(AtomicUsize::new(0)),
        runtime,
    }
}

fn prepared_tool_call() -> lash_core::PreparedToolCall {
    lash_core::PreparedToolCall {
        call_id: CALL_ID.to_string(),
        tool_id: lash_core::ToolId::from("tool:attempt_atomicity".to_string()),
        tool_name: "attempt_atomicity".to_string(),
        args: serde_json::json!({"value": CALL_ID}),
        replay: None,
        prepared_payload: serde_json::json!({"prepared": true}),
    }
}

fn attempt_invocation() -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn(SESSION, TURN),
            ATTEMPT_EFFECT_ID,
        )
        .expect("valid attempt address"),
        lash_core::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        ATTEMPT_EFFECT_ID,
    )
}

fn tool_context<'run>(
    scoped: lash_core::ScopedEffectController<'run>,
    fixtures: &Fixtures,
) -> lash_core::ToolContext<'run> {
    tool_context_with_provider(
        scoped,
        fixtures,
        Arc::new(lash_core::testing::EmptyToolProvider),
        Vec::new(),
        true,
    )
}

fn tool_context_with_provider<'run>(
    scoped: lash_core::ScopedEffectController<'run>,
    fixtures: &Fixtures,
    tools: Arc<dyn lash_core::ToolProvider>,
    catalog: Vec<lash_core::ToolDefinition>,
    bind_direct_client_to_attempt: bool,
) -> lash_core::ToolContext<'run> {
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
    let plugins = lash_core::testing::test_plugin_host(Vec::new())
        .build_session(SESSION)
        .expect("build attempt-atomicity plugin session");
    let processes = lash_core::testing::effect_backed_process_service(
        Arc::clone(&fixtures.registry),
        lash_core::Backend::process_env_store(&fixtures.backend),
    );
    let child_process_starts = Arc::clone(&fixtures.child_process_starts);
    let effect_controller = lash_core::runtime::RuntimeEffectControllerHandle::borrowed(scoped);
    let attempt_parent = attempt_invocation().into_runtime_invocation();
    // The production client, minted against the very controller the sentinel
    // wraps: an `Independent` classification therefore shows up in the ledger
    // as a real crossing instead of being swallowed by a stub.
    let direct_completions = fixtures
        .runtime
        .runtime_session_services()
        .expect("attempt-atomicity session manager")
        .direct_completion_client(
            effect_controller.clone(),
            Some(TurnId::from(TURN.to_string())),
        );
    let direct_completions = if bind_direct_client_to_attempt {
        direct_completions.with_tool_attempt_parent_invocation(attempt_parent.clone())
    } else {
        direct_completions
    };
    let dispatch = Arc::new(lash_core::tool_dispatch::ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog: Arc::new(lash_core::ToolCatalog::from_tool_definitions(catalog)),
        sessions: fixtures.host.clone(),
        session_lifecycle: fixtures.host.clone(),
        session_graph: fixtures.host.clone(),
        processes,
        trigger_router: Some(lash_core::facade_support::TriggerRouter::new(
            Arc::clone(&fixtures.trigger_store) as Arc<dyn lash_core::TriggerStore>,
            lash_core::testing::process_work_wiring_for_registry(Arc::clone(&fixtures.registry)),
        )),
        process_definitions: None,
        process_engines: Default::default(),
        effect_controller,
        direct_completions,
        parent_invocation: Some(attempt_parent.clone()),
        execution_env_spec: lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ),
        session_id: SessionId::from(SESSION.to_string()),
        agent_frame_id: lash_core::FrameNodeId::new("test-frame").unwrap(),
        event_tx,
        turn_activity_tx: None,
        checkpoint_messages: lash_core::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: lash_core::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(
            lash_core::facade_support::SessionAttachmentStore::unavailable(),
        ),
        attachment_source_policy: Arc::new(lash_core::attachments::OpenAttachmentSourcePolicy),
        turn_context: lash_core::TurnContext::default(),
        clock: Arc::new(lash_core::facade_support::SystemClock),
    });
    lash_core::ToolContext::from_dispatch(dispatch)
        .tool_call_id(Some(CALL_ID.to_string()))
        .parent_invocation(Some(attempt_parent))
        .cancellation_token(Some(tokio_util::sync::CancellationToken::new()))
        .child_execution_trace_hook(Some(lash_core::ToolChildExecutionTraceHook::new(
            move |_started| {
                child_process_starts.fetch_add(1, Ordering::SeqCst);
            },
        )))
        .process_events(
            LIVE_PROCESS,
            lash_core::ProcessExecutionWriteAuthority::lease(fixtures.lease.clone()),
            lash_core::testing::process_work_wiring_for_registry(Arc::clone(&fixtures.registry)),
            None,
            None,
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::DeliveryPolicy::EarliestSafeBoundary,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .build()
}

/// A provider that implements only the pure `execute` body — the trait offers
/// no second leaf route to override. The structural law is that its body
/// still runs against `AttemptContext`, so no journal-capable route exists for
/// it to reach in the first place.
struct PureLeafProbeProvider {
    execute_calls: AtomicUsize,
}

impl PureLeafProbeProvider {
    fn new() -> Self {
        Self {
            execute_calls: AtomicUsize::new(0),
        }
    }

    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:attempt_atomicity",
            "attempt_atomicity",
            "",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "string"}),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for PureLeafProbeProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "attempt_atomicity").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(call.name(), "attempt_atomicity");
        // The sealed attempt projection, not the journal-capable `ToolContext`.
        assert_eq!(call.context.session_id(), SESSION);
        assert_eq!(call.context.tool_call_id(), Some(CALL_ID));
        assert_eq!(call.context.execution_scope_id(), TURN);
        // The projection keeps the leaf-safe reads the legacy inventory
        // covered: cancellation observation and the sealed prepared payload
        // both survive the cutover.
        assert!(call.context.cancellation_token().is_some());
        assert_eq!(call.context.prepared_payload(), &serde_json::Value::Null);
        // A tool that never declared `attempt_may_defer` is told exactly that,
        // instead of being pointed at the host's effect controller.
        let refusal = call
            .context
            .completion_key()
            .expect_err("an undeclared deferrer has no reserved completion key");
        assert_eq!(
            refusal.code,
            lash_core::RuntimeErrorCode::ToolDeferralNotDeclared
        );
        assert!(
            refusal.message.contains("attempt_may_defer"),
            "the refusal must name the missing declaration: {}",
            refusal.message
        );
        lash_core::ToolOutcome::ok(serde_json::json!("pure execute ran")).into()
    }
}

/// The post-cutover catch-all. A leaf body receives only `AttemptContext`; all
/// of its process and session reads bypass the effect controller, so the exact
/// crossing count while the attempt is open is zero.
#[tokio::test]
async fn sentinel_allows_no_undeclared_crossing_from_inside_an_attempt() {
    capability_inventory::assert_capability_inventory_complete();
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = lash_core::ScopedEffectController::borrowed(
        &sentinel,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped post-cutover sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        lash_core::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            lash_core::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let attempt = lash_core::AttemptContext::__for_testing(&tool, TURN);
            capability_inventory::exercise_attempt_capabilities(&attempt).await;
            Ok(lash_core::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                    record: Box::new(lash_core::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: lash_core::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: lash_core::ToolIntents::default(),
                }),
                triggers: Vec::new(),
                capture: None,
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

/// A provider still runs its single `execute` body inside the recorded
/// attempt, against `AttemptContext`. There is no
/// per-tool opt-in and no legacy `ToolContext` route left to fall back to, so
/// the attempt opens and closes with zero controller crossings.
#[tokio::test]
async fn pure_execute_provider_routes_through_the_attempt_context_without_controller_crossing() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = lash_core::ScopedEffectController::borrowed(
        &sentinel,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped provider-routing sentinel controller");
    let provider = Arc::new(PureLeafProbeProvider::new());
    let tool = tool_context_with_provider(
        scoped,
        &fixtures,
        Arc::clone(&provider) as Arc<dyn lash_core::ToolProvider>,
        vec![PureLeafProbeProvider::definition()],
        true,
    );

    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        lash_core::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            lash_core::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let dispatch = Arc::clone(
                tool.runtime_dispatch
                    .as_ref()
                    .expect("tool context carries runtime dispatch"),
            );
            let prepared = prepared_tool_call();
            assert!(
                lash_core::tool_dispatch::resolve_callable_manifest_by_id(
                    dispatch.as_ref(),
                    &prepared.tool_id,
                )
                .is_some(),
                "the attempt is admitted through the production catalog authority"
            );
            let result =
                lash_core::tool_dispatch::execute_once(dispatch.as_ref(), &prepared, tool, None)
                    .await;
            // Assert the provider's sentinel value at the test level, not just
            // the Done shape. A recorded attempt body runs under
            // `catch_unwind`, so an assertion that panics *inside* the provider
            // becomes a `Done` failure output: without pinning the exact
            // success payload here, every in-provider law above would be
            // unenforced.
            let lash_core::ToolAttemptOutcome::Done { result, .. } = result else {
                panic!("pure-execute provider must complete, not park");
            };
            assert_eq!(
                result.into_output().outcome,
                lash_core::ToolCallOutcome::Success(lash_core::ToolValue::untrusted_json(
                    serde_json::json!("pure execute ran")
                )),
                "an in-provider assertion panic surfaces here as a failure output"
            );
            Ok(lash_core::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                    record: Box::new(lash_core::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: lash_core::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: lash_core::ToolIntents::default(),
                }),
                triggers: Vec::new(),
                capture: None,
            })
        }),
    )
    .await
    .expect("pure-execute provider completes through the attempt route");

    assert_eq!(provider.execute_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "the sealed attempt context has no controller-crossing route to offer"
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
    let command = lash_core::ProcessCommand::Cancel {
        process_ref: lash_core::ProcessRef::new(
            LIVE_PROCESS,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        origin: lash_core::CancelOrigin::OperatorRequested,
        requester: "test:outside-attempt".to_string(),
        attribution: None,
    };
    let effect_id = command.effect_id();
    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        lash_core::RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(
                    lash_core::ExecutionScope::turn(SESSION, TURN),
                    effect_id.clone(),
                )
                .expect("valid process effect address"),
                lash_core::RuntimeAttribution::for_session(SESSION),
                effect_id.clone(),
            ),
            lash_core::RuntimeEffectCommand::process(command),
        ),
        lash_core::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&fixtures.registry),
            Arc::new(lash_core::NativeProcessWork::for_registry(Arc::clone(
                &fixtures.registry,
            ))),
        ),
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
    let nested_target = registry
        .resolve_process_ref(&ProcessId::from(EXTERNAL_PROCESS))
        .await
        .expect("resolve the nonterminal sentinel target");
    let nested_sentinel = &sentinel;
    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        lash_core::RuntimeEffectEnvelope::new(
            attempt_invocation(),
            lash_core::RuntimeEffectCommand::ToolAttempt {
                call: prepared_tool_call(),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        ),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let command = lash_core::ProcessCommand::Cancel {
                process_ref: nested_target,
                origin: lash_core::CancelOrigin::OperatorRequested,
                requester: "test:sentinel-leak".to_string(),
                attribution: None,
            };
            let effect_id = command.effect_id();
            lash_core::RuntimeEffectController::execute_effect(
                nested_sentinel,
                lash_core::RuntimeEffectEnvelope::new(
                    lash_core::RuntimeEffectInvocation::new(
                        lash_core::EffectAddress::new(
                            lash_core::ExecutionScope::turn(SESSION, TURN),
                            effect_id.clone(),
                        )
                        .expect("valid nested process effect address"),
                        lash_core::RuntimeAttribution::for_session(SESSION),
                        effect_id.clone(),
                    ),
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                lash_core::RuntimeEffectLocalExecutor::processes(
                    Arc::clone(&registry),
                    Arc::new(lash_core::NativeProcessWork::for_registry(registry)),
                ),
            )
            .await?;
            Ok(lash_core::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                    record: Box::new(lash_core::ToolCallRecord {
                        call_id: Some(CALL_ID.to_string()),
                        tool: "attempt_atomicity".to_string(),
                        args: serde_json::Value::Null,
                        output: lash_core::ToolCallOutput::success(serde_json::json!("ok")),
                        duration_ms: 0,
                    }),
                    intents: lash_core::ToolIntents::default(),
                }),
                triggers: Vec::new(),
                capture: None,
            })
        }),
    )
    .await
    .expect("test-only nested leak executes");
    assert_eq!(
        ledger.crossings_inside_attempt(),
        vec!["execute_effect:process:process:cancel:attempt-atomicity-external".to_string()],
        "the literal test-only leak proves the sentinel fails red when a command escapes"
    );
}

/// Each admitted v2 declaration realizes exactly one controller command, and
/// the sentinel attributes that command to the literal stable intent id.
#[tokio::test]
async fn sentinel_records_exactly_one_crossing_per_tool_intent() {
    let fixtures = fixtures().await;
    let tier = ControllerOwnedTier::key_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let scoped = lash_core::ScopedEffectController::borrowed(
        &sentinel,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped intent sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    let mut dispatch = tool
        .runtime_dispatch
        .as_ref()
        .map(|context| context.as_ref().clone())
        .expect("runtime dispatch context");
    dispatch.parent_invocation = Some(lash_core::RuntimeInvocation::effect(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn(SESSION, TURN),
            "intent-drain",
        )
        .expect("valid intent-drain address"),
        lash_core::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        "intent-drain",
    ));

    let intents = lash_core::ToolIntents::v3(vec![
        lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
            session_id: SessionId::from(SESSION.to_string()),
            declaration: lash_core::ProcessStartDeclaration::external(
                lash_core::ProcessOriginator::host_scoped("intent-test"),
                serde_json::json!({"step": "start"}),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ),
        })),
        lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
            session_id: SessionId::from(SESSION.to_string()),
            process_id: ProcessId::from(LIVE_PROCESS.to_string()),
            signal_name: "resume".to_string(),
            payload: serde_json::json!({"step": "signal"}),
        }),
        lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
            session_id: SessionId::from(SESSION.to_string()),
            process_id: ProcessId::from(LIVE_PROCESS.to_string()),
            event_type: "attempt.atomicity.note".to_string(),
            payload: serde_json::json!({"step": "event"}),
        }),
        lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
            session_id: SessionId::from(SESSION.to_string()),
            process_id: ProcessId::from(LIVE_PROCESS.to_string()),
        }),
    ]);
    let outcomes = lash_core::tool_dispatch::execute_final_tool_intents(
        &dispatch,
        Some(CALL_ID),
        &intents,
        None,
    )
    .await
    .expect("execute intent batch");
    assert_eq!(outcomes.len(), 4, "one typed outcome per intent");
    let literal_ids = [
        "tool-intent:v2:blake3:06404a7267c11e95e26c0911398d8e4881ae846fd7e962be54aec5aefa88ab74",
        "tool-intent:v2:blake3:f6a0238ac5cd935c4e90ba7aea76793abbea968361e0d5df0fec1a576615d291",
        "tool-intent:v2:blake3:4d9e09cb0742c6bd58ba1fc01b712b4b74bb762edad0e708fb18a19db960f888",
        "tool-intent:v2:blake3:e93de6b5ba860968e4ab3c339cfe2f77f49aa384c4e6ede22853541e103e1ac4",
    ];
    let actual_ids = outcomes
        .iter()
        .map(|outcome| match outcome {
            lash_core::ToolIntentExecutionOutcome::Executed { identity, .. } => {
                identity.replay_key.as_str()
            }
            lash_core::ToolIntentExecutionOutcome::Refused { refusal, .. } => {
                panic!("fixture intent was refused: {refusal:?}")
            }
            lash_core::ToolIntentExecutionOutcome::ProtocolRefused { refusal } => {
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
    let scoped = lash_core::ScopedEffectController::borrowed(
        &sentinel,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped overflow sentinel controller");
    let tool = tool_context(scoped, &fixtures);
    let dispatch = tool
        .runtime_dispatch
        .as_ref()
        .map(|context| context.as_ref().clone())
        .expect("runtime dispatch context");
    let intents = lash_core::ToolIntents::v3(
        (0..=lash_core::TOOL_INTENT_MAX_COUNT)
            .map(|index| {
                lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
                    session_id: SessionId::from(SESSION.to_string()),
                    process_id: ProcessId::from(LIVE_PROCESS.to_string()),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"index": index}),
                })
            })
            .collect(),
    );
    let outcomes = lash_core::tool_dispatch::execute_final_tool_intents(
        &dispatch,
        Some(CALL_ID),
        &intents,
        None,
    )
    .await
    .expect("refuse over-budget intent batch");
    assert_eq!(outcomes.len(), 33, "every declaration gets a refusal");
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        lash_core::ToolIntentExecutionOutcome::Refused {
            refusal: lash_core::ToolIntentRefusalReason::CountBudgetExceeded {
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
            lash_core::ToolIntentExecutionOutcome::Refused {
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
    let identity =
        lash_core::derive_tool_intent_identity(&SessionId::from(SESSION), TURN, Some(CALL_ID), 9)
            .expect("literal intent identity");
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "structural-process",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "structural.note".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .expect("register structural attribution target");
    let command = lash_core::ProcessCommand::EmitEvent {
        process_id: ProcessId::from("structural-process"),
        request: lash_core::ProcessEventAppendRequest::new(
            "structural.note",
            serde_json::json!({"law": "overcount"}),
        )
        .with_replay_key("structural-attribution-event"),
    };
    let attributed = lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn(SESSION, TURN),
            "plain-unprefixed-key",
        )
        .expect("valid structurally attributed address"),
        lash_core::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        "structurally-attributed-command",
    )
    .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(
        identity.clone(),
    ));
    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        lash_core::RuntimeEffectEnvelope::new(
            attributed,
            lash_core::RuntimeEffectCommand::process(command.clone()),
        ),
        lash_core::RuntimeEffectLocalExecutor::processes(
            registry.clone(),
            Arc::new(lash_core::NativeProcessWork::for_registry(registry.clone())),
        ),
    )
    .await
    .expect("unprefixed command executes");
    assert_eq!(
        ledger.crossings_for_intent(&identity.replay_key),
        vec!["execute_effect:process:structurally-attributed-command".to_string()]
    );

    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        lash_core::RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(
                    lash_core::ExecutionScope::turn(SESSION, TURN),
                    "another-plain-key",
                )
                .expect("valid missing-attribution test address"),
                lash_core::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
                "missing-attribution-command",
            ),
            lash_core::RuntimeEffectCommand::process(command),
        ),
        lash_core::RuntimeEffectLocalExecutor::processes(
            registry.clone(),
            Arc::new(lash_core::NativeProcessWork::for_registry(registry)),
        ),
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
    let scoped = lash_core::ScopedEffectController::borrowed(
        &controller,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped replaying controller");
    let tool = tool_context(scoped, &fixtures);
    let dispatch = tool
        .runtime_dispatch
        .as_ref()
        .map(|context| context.as_ref().clone())
        .expect("runtime dispatch context");
    let intents = lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::SignalProcess(
        lash_core::SignalProcessIntent {
            session_id: SessionId::from(SESSION.to_string()),
            process_id: ProcessId::from(LIVE_PROCESS.to_string()),
            signal_name: "resume".to_string(),
            payload: serde_json::json!({"recorded": "payload"}),
        },
    )]);

    let first = lash_core::tool_dispatch::execute_final_tool_intents(
        &dispatch,
        Some(CALL_ID),
        &intents,
        None,
    )
    .await
    .expect("execute first intent drain");
    let first_bytes = serde_json::to_vec(&first).expect("serialize first intent outcome");
    assert!(
        matches!(
            first.as_slice(),
            [lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::SignalProcess,
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
            &ProcessId::from(LIVE_PROCESS),
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("terminal after first drain"),
            )),
            lash_core::ProcessCompletionAuthority::workflow_key("live-mutation-law"),
        )
        .await
        .expect("mutate live target to terminal");

    let redriven = lash_core::tool_dispatch::execute_final_tool_intents(
        &dispatch,
        Some(CALL_ID),
        &intents,
        None,
    )
    .await
    .expect("redrive intent drain");
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
    outcome: Option<lash_core::RuntimeEffectOutcome>,
}

/// An ordinal-addressed journal with the two behaviors that make the FIG-1486
/// wedge reachable: a recorded entry replays *without* re-entering its body,
/// and a command that meets a different recorded entry at its ordinal is
/// refused the way an ordinal-addressed engine refuses it (Restate `RT0016`).
struct OrdinalJournaledTier {
    inner: lash_core::facade_support::NativeRuntimeEffectController,
    journal: std::sync::Mutex<Vec<JournalEntry>>,
    replaying: std::sync::atomic::AtomicBool,
    cursor: AtomicUsize,
}

impl OrdinalJournaledTier {
    fn recording() -> Self {
        Self {
            inner: lash_core::facade_support::NativeRuntimeEffectController::default(),
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

    fn identity(envelope: &lash_core::RuntimeEffectEnvelope) -> String {
        let kind = envelope.command.kind().as_str();
        let effect_id = envelope.invocation.effect_id();
        format!("{kind}:{effect_id}")
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for OrdinalJournaledTier {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for OrdinalJournaledTier {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        lash_core::EffectJournaling::Journaled
    }

    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let identity = Self::identity(&envelope);
        if self.replaying.load(Ordering::SeqCst) {
            let ordinal = self.cursor.fetch_add(1, Ordering::SeqCst);
            let journal = self.journal.lock_recover();
            let Some(entry) = journal.get(ordinal) else {
                return Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::WorkerReplacementAbort,
                    format!("RT0016: journal ended before ordinal {ordinal} (`{identity}`)"),
                ));
            };
            if entry.identity != identity {
                return Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::WorkerReplacementAbort,
                    format!(
                        "RT0016: journal mismatch at ordinal {ordinal}: recorded `{}`, handler issued `{identity}`",
                        entry.identity
                    ),
                ));
            }
            let Some(outcome) = entry.outcome.clone() else {
                return Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::WorkerReplacementAbort,
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

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: std::sync::Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }
    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

fn follow_on_invocation() -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn(SESSION, TURN),
            FOLLOW_ON_EFFECT_ID,
        )
        .expect("valid follow-on address"),
        lash_core::RuntimeAttribution::for_turn(SESSION, TURN, 0, 0),
        FOLLOW_ON_EFFECT_ID,
    )
}

fn attempt_effect_envelope() -> lash_core::RuntimeEffectEnvelope {
    lash_core::RuntimeEffectEnvelope::new(
        attempt_invocation(),
        lash_core::RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call(),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

fn attempt_done_outcome() -> lash_core::RuntimeEffectOutcome {
    lash_core::RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(lash_core::ToolCallRecord {
                call_id: Some(CALL_ID.to_string()),
                tool: "attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                output: lash_core::ToolCallOutput::success(serde_json::json!("ok")),
                duration_ms: 0,
            }),
            intents: lash_core::ToolIntents::default(),
        }),
        triggers: Vec::new(),
        capture: None,
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
    let scoped = lash_core::ScopedEffectController::borrowed(
        &tier,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped ordinal-journaled controller");
    let tool = tool_context(scoped, &fixtures);
    lash_core::RuntimeEffectController::execute_effect(
        &tier,
        attempt_effect_envelope(),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            first_incarnation_bodies.fetch_add(1, Ordering::SeqCst);
            let attempt = lash_core::AttemptContext::__for_testing(&tool, TURN);
            assert_eq!(
                attempt
                    .direct_completions()
                    .complete(
                        lash_core::facade_support::DirectRequest::text(
                            DIRECT_MODEL,
                            "redrive direct completion"
                        ),
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
    lash_core::RuntimeEffectController::execute_effect(
        &tier,
        lash_core::RuntimeEffectEnvelope::new(
            follow_on_invocation(),
            lash_core::RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For { duration_ms: 0 },
            },
        ),
        lash_core::RuntimeEffectLocalExecutor::testing(|_envelope| async {
            Ok(lash_core::RuntimeEffectOutcome::Sleep)
        }),
    )
    .await
    .expect("first incarnation records the follow-on command");
    assert_eq!(bodies_entered.load(Ordering::SeqCst), 1);

    tier.start_redrive();
    let redriven_bodies = Arc::clone(&bodies_entered);
    let redriven_scoped = lash_core::ScopedEffectController::borrowed(
        &tier,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped redrive controller");
    let redriven_tool = tool_context(redriven_scoped, &fixtures);
    let replayed = lash_core::RuntimeEffectController::execute_effect(
        &tier,
        attempt_effect_envelope(),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            redriven_bodies.fetch_add(1, Ordering::SeqCst);
            let _attempt = lash_core::AttemptContext::__for_testing(&redriven_tool, TURN);
            Ok(attempt_done_outcome())
        }),
    )
    .await
    .expect("redrive replays the recorded attempt");
    assert!(matches!(
        replayed,
        lash_core::RuntimeEffectOutcome::ToolAttempt { .. }
    ));
    assert_eq!(
        bodies_entered.load(Ordering::SeqCst),
        1,
        "the recorded attempt replays without re-entering its body"
    );

    let follow_on = lash_core::RuntimeEffectController::execute_effect(
        &tier,
        lash_core::RuntimeEffectEnvelope::new(
            follow_on_invocation(),
            lash_core::RuntimeEffectCommand::Sleep {
                spec: lash_core::SleepSpec::For { duration_ms: 0 },
            },
        ),
        lash_core::RuntimeEffectLocalExecutor::testing(|_envelope| async {
            Ok(lash_core::RuntimeEffectOutcome::Sleep)
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

fn direct_llm_request(request_id: &str) -> lash_core::LlmRequest {
    lash_core::LlmRequest {
        instructions: None,
        model: DIRECT_MODEL.to_string(),
        messages: vec![lash_core::llm::types::LlmMessage::new(
            lash_core::llm::types::LlmRole::User,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: Arc::from("attempt direct llm completion"),
                response_meta: None,
                cache_breakpoint: false,
            }],
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: lash_core::llm::types::LlmToolChoice::None,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        scope: lash_core::LlmRequestScope::new(SESSION, format!("{SESSION}:frame"), request_id),
        output_spec: None,
        stream_events: None,
        generation: lash_core::GenerationOptions::default(),
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
    let scoped = lash_core::ScopedEffectController::borrowed(
        &sentinel,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped direct-llm sentinel controller");
    let direct_completions = fixtures
        .runtime
        .runtime_session_services()
        .expect("attempt-atomicity session manager")
        .direct_completion_client(
            lash_core::runtime::RuntimeEffectControllerHandle::borrowed(scoped),
            Some(TurnId::from(TURN.to_string())),
        )
        .with_tool_attempt_parent_invocation(attempt_invocation().into_runtime_invocation());

    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        attempt_effect_envelope(),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let completion = direct_completions
                .direct_llm_completion(
                    direct_llm_request("attempt-atomicity:direct-llm"),
                    "attempt-atomicity",
                )
                .await
                .expect("attempt-scoped direct llm completion");
            assert_eq!(completion.response.full_text(), DIRECT_TEXT);
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

/// A provider that reaches for the *raw* direct-completion client the attempt
/// dispatch handed its context and calls the plain entry point. That entry
/// point takes no parent invocation, so only the binding the production attempt
/// dispatch applies to the client itself can keep the call off the journal.
#[derive(Default)]
struct RawClientDirectProvider {
    execute_calls: AtomicUsize,
}

impl RawClientDirectProvider {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:attempt_atomicity",
            "attempt_atomicity",
            "",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "string"}),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RawClientDirectProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "attempt_atomicity").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        let context = call.context;
        let completion = context
            .direct_completions
            .direct_completion(
                lash_core::facade_support::DirectRequest::text(
                    DIRECT_MODEL,
                    "raw client direct completion",
                ),
                "attempt-atomicity",
            )
            .await
            .expect("raw-client direct completion");
        assert_eq!(completion.text, DIRECT_TEXT);
        lash_core::ToolOutcome::ok(serde_json::json!("raw client ran")).into()
    }
}

fn raw_client_probe<'run>(
    sentinel: &'run AttemptAtomicitySentinel<'run>,
    fixtures: &Fixtures,
    provider: &Arc<RawClientDirectProvider>,
) -> lash_core::ToolContext<'run> {
    let scoped = lash_core::ScopedEffectController::borrowed(
        sentinel,
        lash_core::AdmittedScope::turn(SESSION, TURN),
    )
    .expect("scoped raw-client sentinel controller");
    tool_context_with_provider(
        scoped,
        fixtures,
        Arc::clone(provider) as Arc<dyn lash_core::ToolProvider>,
        vec![RawClientDirectProvider::definition()],
        false,
    )
}

async fn assert_raw_client_probe_starts_unbound(fixtures: &Fixtures) {
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let provider = Arc::new(RawClientDirectProvider::default());
    let tool = raw_client_probe(&sentinel, fixtures, &provider);
    let direct_completions = tool
        .runtime_dispatch
        .as_ref()
        .expect("raw-client probe carries runtime dispatch")
        .direct_completions
        .clone();

    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        attempt_effect_envelope(),
        lash_core::RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
            let completion = direct_completions
                .direct_completion(
                    lash_core::facade_support::DirectRequest::text(
                        DIRECT_MODEL,
                        "unbound client precondition",
                    ),
                    "attempt-atomicity",
                )
                .await
                .expect("unbound direct-client precondition completes");
            assert_eq!(completion.text, DIRECT_TEXT);
            Ok(attempt_done_outcome())
        }),
    )
    .await
    .expect("unbound direct-client precondition attempt completes");

    assert_eq!(ledger.attempt_bodies_opened(), 1);
    let crossings = ledger.crossings_inside_attempt();
    assert_eq!(
        crossings.len(),
        1,
        "the raw-client fixture must enter production with exactly one observable unbound crossing"
    );
    assert!(
        crossings[0].starts_with("execute_effect:direct:"),
        "the raw-client fixture must start unbound to a ToolAttempt: {crossings:?}"
    );
}

/// The execution-context attempt path (`RuntimeExecutionContext::
/// execute_prepared_tool_attempt_effect`) must bind the direct client it derives
/// for the attempt, not just the tool context's parent invocation.
#[tokio::test]
async fn execution_context_attempt_dispatch_binds_the_direct_client() {
    let fixtures = fixtures().await;
    assert_raw_client_probe_starts_unbound(&fixtures).await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let provider = Arc::new(RawClientDirectProvider::default());
    let tool = raw_client_probe(&sentinel, &fixtures, &provider);
    let dispatch = Arc::clone(
        tool.runtime_dispatch
            .as_ref()
            .expect("tool context carries runtime dispatch"),
    );
    let execution_context = lash_core::RuntimeExecutionContext::new(
        SessionId::from(SESSION.to_string()),
        dispatch,
        Arc::new(lash_core::InMemoryProcessExecutionEnvStore::new()),
        Arc::new(lash_core::facade_support::SessionAttachmentStore::unavailable()),
        Arc::new(lash_core::facade_support::ChronologicalProjection::default()),
        None,
        lash_core::TurnContext::default(),
    );

    lash_core::RuntimeEffectController::execute_effect(
        &sentinel,
        attempt_effect_envelope(),
        lash_core::RuntimeEffectLocalExecutor::testing(move |envelope| async move {
            let outcome = execution_context
                .execute_prepared_tool_attempt_effect(
                    prepared_tool_call(),
                    None,
                    1,
                    1,
                    envelope.invocation.into_runtime_invocation(),
                    None,
                    None,
                )
                .await?;
            Ok(lash_core::RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(outcome.launch),
                triggers: outcome.triggers,
                capture: (!outcome.capture.is_empty()).then(|| Box::new(outcome.capture)),
            })
        }),
    )
    .await
    .expect("execution-context attempt completes");

    assert_eq!(provider.execute_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "the attempt dispatch must bind its direct client, whatever entry point the leaf uses"
    );
}

/// The prepared-attempt local runner (`RuntimeEffectLocalExecutor::
/// prepared_tool_attempt`) derives its own attempt dispatch, and must bind the
/// direct client the same way. Driven through the production coordinator so the
/// controller opens the recorded attempt itself.
#[tokio::test]
async fn prepared_attempt_runner_dispatch_binds_the_direct_client() {
    let fixtures = fixtures().await;
    assert_raw_client_probe_starts_unbound(&fixtures).await;
    let tier = ControllerOwnedTier::ordinal_addressed();
    let ledger = NestedJournalLedger::new();
    let sentinel = AttemptAtomicitySentinel::new(&tier, Arc::clone(&ledger));
    let provider = Arc::new(RawClientDirectProvider::default());
    let tool = raw_client_probe(&sentinel, &fixtures, &provider);
    let dispatch = Arc::clone(
        tool.runtime_dispatch
            .as_ref()
            .expect("tool context carries runtime dispatch"),
    );

    let launch =
        lash_core::tool_dispatch::coordinate_prepared_tool_call_launch_with_execution_context(
            dispatch.as_ref(),
            prepared_tool_call(),
            None,
            tool,
        )
        .await;

    assert!(matches!(
        launch,
        lash_core::tool_dispatch::ToolCallLaunch::Done(_)
    ));
    assert_eq!(provider.execute_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.crossings_inside_attempt(),
        Vec::<String>::new(),
        "the prepared-attempt runner must bind its direct client before entering the leaf"
    );
}

#[path = "attempt_atomicity/capability_inventory.rs"]
mod capability_inventory;
