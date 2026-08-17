use super::*;
use proptest::prelude::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeScenarioCoverage {
    pub(crate) test_name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) owned_invariant: &'static str,
}

macro_rules! runtime_scenario_coverage {
    ($test_fn:ident, $display_name:literal, $owned_invariant:literal) => {
        RuntimeScenarioCoverage {
            test_name: stringify!($test_fn),
            display_name: $display_name,
            owned_invariant: $owned_invariant,
        }
    };
}

const COMMAND_BEFORE_TURN_WORK: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_drains_command_before_turn_work_and_commits_checkpoint,
    "command before turn work",
    "Session-command gate, checkpoint persistence, stale queue completion rejection, final queue drain."
);
const COMMAND_ONLY_QUEUE_DRAIN: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_command_only_queue_drain_completes_without_turn_work,
    "command-only queue drain",
    "Command-only queued work claims no turn work and explicitly commits."
);
const QUEUED_WORK_KEEPS_NEXT_INPUT: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_queued_work_claim_keeps_pending_next_turn_input,
    "queued work claim keeps pending next-turn input",
    "Queued turn work does not consume pending next-turn input."
);
const ACTIVE_CHECKPOINT_WAKE_CLAIM: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_claims_process_wake_at_active_checkpoint_boundary,
    "active checkpoint process wake claim",
    "Process-wake turn work is eligible at the active-checkpoint claim boundary."
);
const QUEUED_TURN_INPUT_COMPLETION: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_claims_queued_turn_input_and_completes_it,
    "queued turn input completion",
    "Next-turn pending inputs are claimed, hidden while live, and completed by commit."
);
const OBSERVATION_REPLAY: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_observation_replay_keeps_original_turn_input,
    "observation replay preserves live turn input",
    "Source-key observation replay preserves the original live input payload and id."
);
const CHECKPOINT_REDRIVE_CANCEL: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_defers_checkpoint_turn_input_and_respects_cancel,
    "checkpoint redrive cancel",
    "Active-turn input deferral, cancellation after deferral, and no later idle claim."
);
const SESSION_LEASE_RELEASE_FAULT: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_commits_after_advisory_session_lease_release,
    "advisory session lease release",
    "A released advisory lease permits a current-head commit while the head CAS rejects stale state."
);
const STALE_LEASE_EXPIRY: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_waits_for_stale_session_lease_ttl,
    "stale session lease expiry",
    "An unexpired stale holder stays busy; TTL expiry advances the fence and the successor stays protected."
);
const TOOL_INTENT_DRAIN: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_opted_in_provider_drains_every_v1_tool_intent,
    "opted-in provider tool-intent drain",
    "A real runtime turn commits an opted-in provider attempt, then realizes all four v1 intent kinds through the production coordinator."
);

pub(crate) const RUNTIME_SCENARIO_COVERAGE: &[RuntimeScenarioCoverage] = &[
    COMMAND_BEFORE_TURN_WORK,
    COMMAND_ONLY_QUEUE_DRAIN,
    QUEUED_WORK_KEEPS_NEXT_INPUT,
    ACTIVE_CHECKPOINT_WAKE_CLAIM,
    QUEUED_TURN_INPUT_COMPLETION,
    OBSERVATION_REPLAY,
    CHECKPOINT_REDRIVE_CANCEL,
    SESSION_LEASE_RELEASE_FAULT,
    STALE_LEASE_EXPIRY,
    TOOL_INTENT_DRAIN,
];

#[test]
fn runtime_scenario_coverage_metadata_is_unique_and_complete() {
    assert_eq!(RUNTIME_SCENARIO_COVERAGE.len(), 10);
    let mut names = BTreeSet::new();
    for coverage in RUNTIME_SCENARIO_COVERAGE {
        assert!(
            coverage.test_name.starts_with("runtime_scenario_"),
            "unexpected Runtime Scenario test name {}",
            coverage.test_name
        );
        assert!(
            !coverage.display_name.trim().is_empty(),
            "{} must have a scenario display name",
            coverage.test_name
        );
        assert!(
            !coverage.owned_invariant.trim().is_empty(),
            "{} must document its owned invariant",
            coverage.test_name
        );
        assert!(
            names.insert(coverage.test_name),
            "duplicate Runtime Scenario coverage metadata for {}",
            coverage.test_name
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum RuntimeStateMachinePhaseSymbol {
    Ingress,
    Checkpoint,
    LeadingCommandClaim,
    TurnWorkClaim,
    NextTurnInputClaim,
    MisalignedNextTurnInputClaim,
    StaleLeaseExpiry,
    StaleQueueCompletionFault,
    ReleasedLeaseCommitFault,
    Commit,
}

impl RuntimeStateMachinePhaseSymbol {
    fn phase(self) -> RuntimeScenarioPhase {
        match self {
            Self::Ingress => RuntimeIngressPhase::new().into(),
            Self::Checkpoint => RuntimeCheckpointPhase::new().into(),
            Self::LeadingCommandClaim => RuntimeLeadingCommandClaimPhase::new().into(),
            Self::TurnWorkClaim => {
                RuntimeTurnWorkClaimPhase::at(QueuedWorkClaimBoundary::Idle).into()
            }
            Self::NextTurnInputClaim => RuntimeNextTurnInputClaimPhase::new().into(),
            Self::MisalignedNextTurnInputClaim => RuntimeNextTurnInputClaimPhase::new()
                .expect_inputs(vec!["one"], Vec::new())
                .into(),
            Self::StaleLeaseExpiry => RuntimeLeasePhase::expire_stale_holder().into(),
            Self::StaleQueueCompletionFault => RuntimeFaultPhase::StaleQueueCompletion.into(),
            Self::ReleasedLeaseCommitFault => {
                RuntimeFaultPhase::CommitAfterAdvisoryLeaseRelease.into()
            }
            Self::Commit => RuntimeCommitPhase::new().into(),
        }
    }

    fn releases_session_lease(self) -> bool {
        matches!(self, Self::Commit | Self::ReleasedLeaseCommitFault)
    }

    fn requires_live_session_lease(self) -> bool {
        matches!(
            self,
            Self::Checkpoint
                | Self::LeadingCommandClaim
                | Self::TurnWorkClaim
                | Self::NextTurnInputClaim
                | Self::MisalignedNextTurnInputClaim
                | Self::StaleQueueCompletionFault
                | Self::Commit
        )
    }
}

fn runtime_state_machine_phase_symbol_strategy()
-> impl Strategy<Value = RuntimeStateMachinePhaseSymbol> {
    prop_oneof![
        Just(RuntimeStateMachinePhaseSymbol::Ingress),
        Just(RuntimeStateMachinePhaseSymbol::Checkpoint),
        Just(RuntimeStateMachinePhaseSymbol::LeadingCommandClaim),
        Just(RuntimeStateMachinePhaseSymbol::TurnWorkClaim),
        Just(RuntimeStateMachinePhaseSymbol::NextTurnInputClaim),
        Just(RuntimeStateMachinePhaseSymbol::MisalignedNextTurnInputClaim),
        Just(RuntimeStateMachinePhaseSymbol::StaleLeaseExpiry),
        Just(RuntimeStateMachinePhaseSymbol::StaleQueueCompletionFault),
        Just(RuntimeStateMachinePhaseSymbol::ReleasedLeaseCommitFault),
        Just(RuntimeStateMachinePhaseSymbol::Commit),
    ]
}

fn runtime_state_machine_phase_order_oracle(symbols: &[RuntimeStateMachinePhaseSymbol]) -> bool {
    let mut saw_live_lease_claim = false;
    let mut saw_turn_work_claim = false;
    for (index, symbol) in symbols.iter().copied().enumerate() {
        if symbol.releases_session_lease() && index + 1 != symbols.len() {
            return false;
        }
        if symbol.requires_live_session_lease() {
            saw_live_lease_claim = true;
        }
        match symbol {
            RuntimeStateMachinePhaseSymbol::StaleLeaseExpiry if saw_live_lease_claim => {
                return false;
            }
            RuntimeStateMachinePhaseSymbol::StaleLeaseExpiry => {
                saw_live_lease_claim = true;
            }
            RuntimeStateMachinePhaseSymbol::TurnWorkClaim => {
                saw_live_lease_claim = true;
                saw_turn_work_claim = true;
            }
            RuntimeStateMachinePhaseSymbol::StaleQueueCompletionFault if !saw_turn_work_claim => {
                return false;
            }
            RuntimeStateMachinePhaseSymbol::MisalignedNextTurnInputClaim => {
                return false;
            }
            _ => {}
        }
    }
    true
}

proptest! {
    #[test]
    fn runtime_state_machine_property_phase_order_matches_scenario_dsl(
        symbols in prop::collection::vec(runtime_state_machine_phase_symbol_strategy(), 1..9),
    ) {
        let mut scenario = RuntimeScenario::new("runtime state-machine property");
        for symbol in &symbols {
            scenario = scenario.phase(symbol.phase());
        }

        prop_assert_eq!(
            scenario.phase_order_is_valid_for_test(),
            runtime_state_machine_phase_order_oracle(&symbols)
        );
    }
}

#[tokio::test]
async fn runtime_scenario_drains_command_before_turn_work_and_commits_checkpoint() {
    RuntimeScenario::new(COMMAND_BEFORE_TURN_WORK.display_name)
        .session_id("runtime-scenario-command-before-turn")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-worker",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue(RuntimeQueueIngress::RefreshToolCatalog {
                    reason: "refresh before turn",
                })
                .enqueue(RuntimeQueueIngress::ProcessWake {
                    text: "wake after command",
                })
                .expect_enqueued_classes(vec![
                    QueuedWorkClass::SessionCommand,
                    QueuedWorkClass::TurnWork,
                ]),
        )
        .phase(
            RuntimeLeadingCommandClaimPhase::new()
                .expect_turn_work_blocked_before_command(true)
                .expect_count(1),
        )
        .phase(RuntimeCheckpointPhase::new().turn_index(7))
        .phase(RuntimeTurnWorkClaimPhase::at(QueuedWorkClaimBoundary::Idle).expect_count(1))
        .phase(RuntimeFaultPhase::StaleQueueCompletion)
        .phase(RuntimeCommitPhase::new().expect_checkpoint_turn_index(7))
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_command_only_queue_drain_completes_without_turn_work() {
    RuntimeScenario::new(COMMAND_ONLY_QUEUE_DRAIN.display_name)
        .session_id("runtime-scenario-command-only")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-command-only-worker",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue(RuntimeQueueIngress::RefreshToolCatalog {
                    reason: "command-only refresh",
                })
                .expect_enqueued_classes(vec![QueuedWorkClass::SessionCommand]),
        )
        .phase(RuntimeLeadingCommandClaimPhase::new().expect_count(1))
        .phase(RuntimeTurnWorkClaimPhase::at(QueuedWorkClaimBoundary::Idle).expect_count(0))
        .phase(RuntimeCommitPhase::new())
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_queued_work_claim_keeps_pending_next_turn_input() {
    RuntimeScenario::new(QUEUED_WORK_KEEPS_NEXT_INPUT.display_name)
        .session_id("runtime-scenario-queue-keeps-turn-input")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-queue-turn-input-owner",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue(RuntimeQueueIngress::ProcessWake {
                    text: "wake selected before user input",
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::NextTurn {
                    alias: "pending-user-input",
                    text: "still pending user input",
                    source_key: None,
                })
                .expect_enqueued_classes(vec![QueuedWorkClass::TurnWork]),
        )
        .phase(
            RuntimeTurnWorkClaimPhase::at(QueuedWorkClaimBoundary::Idle)
                .expect_count(1)
                .expect_pending_turn_inputs_after_claim(vec![RuntimePendingTurnInputExpectation {
                    alias: "pending-user-input",
                    state: TurnInputState::DeferredNextTurn,
                    ingress: RuntimePendingTurnInputIngressExpectation::NextTurn,
                }]),
        )
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_claims_process_wake_at_active_checkpoint_boundary() {
    RuntimeScenario::new(ACTIVE_CHECKPOINT_WAKE_CLAIM.display_name)
        .session_id("runtime-scenario-active-checkpoint-wake")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-active-checkpoint-owner",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue(RuntimeQueueIngress::ProcessWake {
                    text: "wake at checkpoint",
                })
                .expect_enqueued_classes(vec![QueuedWorkClass::TurnWork]),
        )
        .phase(
            RuntimeTurnWorkClaimPhase::at(QueuedWorkClaimBoundary::ActiveTurnCheckpoint)
                .expect_count(1),
        )
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_claims_queued_turn_input_and_completes_it() {
    RuntimeScenario::new(QUEUED_TURN_INPUT_COMPLETION.display_name)
        .session_id("runtime-scenario-queued-turn-input")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-turn-input-owner",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue_turn_input(RuntimeTurnInputIngress::NextTurn {
                    alias: "first",
                    text: "first queued input",
                    source_key: None,
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::NextTurn {
                    alias: "second",
                    text: "second queued input",
                    source_key: None,
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::NextTurnForSession {
                    session_id: "runtime-scenario-other-session",
                    text: "other session input",
                }),
        )
        .phase(
            RuntimeNextTurnInputClaimPhase::new()
                .expect_inputs(
                    vec!["first", "second"],
                    vec!["first queued input", "second queued input"],
                )
                .expect_pending_hidden_after_claim(),
        )
        .phase(RuntimeCommitPhase::new().expect_pending_turn_inputs_empty())
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_observation_replay_keeps_original_turn_input() {
    RuntimeScenario::new(OBSERVATION_REPLAY.display_name)
        .session_id("runtime-scenario-observation-replay")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-observation-replay-owner",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue_turn_input(RuntimeTurnInputIngress::NextTurn {
                    alias: "observed-live-input",
                    text: "observed live input",
                    source_key: Some("runtime-scenario:observation"),
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::ReplayNextTurn {
                    alias: "observed-live-input-replay",
                    text: "observed live input",
                    source_key: "runtime-scenario:observation",
                    expected_alias: "observed-live-input",
                    expected_text: "observed live input",
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::ConflictNextTurnReplay {
                    text: "changed payload must conflict",
                    source_key: "runtime-scenario:observation",
                    expected_alias: "observed-live-input",
                }),
        )
        .phase(
            RuntimeNextTurnInputClaimPhase::new()
                .expect_inputs(vec!["observed-live-input"], vec!["observed live input"])
                .expect_pending_hidden_after_claim(),
        )
        .phase(RuntimeCommitPhase::new().expect_pending_turn_inputs_empty())
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_defers_checkpoint_turn_input_and_respects_cancel() {
    let turn_id = "runtime-scenario-redrive-turn";
    RuntimeScenario::new(CHECKPOINT_REDRIVE_CANCEL.display_name)
        .session_id("runtime-scenario-checkpoint-redrive-cancel")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-redrive-input-owner",
        })
        .phase(
            RuntimeIngressPhase::new()
                .enqueue_turn_input(RuntimeTurnInputIngress::ActiveTurn {
                    alias: "active-keep",
                    turn_id,
                    min_boundary: TurnInputCheckpointBoundary::AfterWork,
                    text: "active input to redrive",
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::ActiveTurn {
                    alias: "active-cancel",
                    turn_id,
                    min_boundary: TurnInputCheckpointBoundary::AfterWork,
                    text: "active input cancelled before redrive",
                })
                .enqueue_turn_input(RuntimeTurnInputIngress::NextTurn {
                    alias: "next-cancel",
                    text: "next input cancelled before redrive",
                    source_key: None,
                })
                .cancel_turn_input_before_commit("active-cancel")
                .cancel_turn_input_before_commit("next-cancel"),
        )
        .phase(
            RuntimeCheckpointPhase::new()
                .defer_interrupted_turn_inputs(turn_id)
                .cancel_turn_input_after_deferral("active-keep")
                .expect_pending_after_deferral(vec![RuntimePendingTurnInputExpectation {
                    alias: "active-keep",
                    state: TurnInputState::DeferredNextTurn,
                    ingress: RuntimePendingTurnInputIngressExpectation::NextTurn,
                }])
                .expect_no_next_turn_input_claim_after_cancellations(),
        )
        .phase(RuntimeCommitPhase::new().expect_pending_turn_inputs_empty())
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_commits_after_advisory_session_lease_release() {
    RuntimeScenario::new(SESSION_LEASE_RELEASE_FAULT.display_name)
        .session_id("runtime-scenario-lease-failure")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-lease-owner",
        })
        .phase(RuntimeFaultPhase::CommitAfterAdvisoryLeaseRelease)
        .run()
        .await;
}

#[tokio::test]
async fn runtime_scenario_waits_for_stale_session_lease_ttl() {
    RuntimeScenario::new(STALE_LEASE_EXPIRY.display_name)
        .session_id("runtime-scenario-stale-lease-expiry")
        .host_behavior(RuntimeHostBehavior {
            lease_owner_id: "runtime-scenario-reclaim-owner",
        })
        .phase(RuntimeLeasePhase::expire_stale_holder())
        .run()
        .await;
}

struct RuntimeScenarioIntentProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

fn runtime_scenario_intent_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:runtime_scenario_intents",
        "runtime_scenario_intents",
        "Emit every v1 tool intent kind.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for RuntimeScenarioIntentProvider {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![runtime_scenario_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "runtime_scenario_intents")
            .then(|| Arc::new(runtime_scenario_intent_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
        panic!("the runtime scenario provider must use AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptResult {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let session_id = call.context.session_id().to_string();
        crate::ToolAttemptResult::done(
            crate::ToolResultDone::ok(serde_json::json!({"provider": "done"})),
            crate::ToolIntents::v1(vec![
                crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
                    session_id: session_id.clone(),
                    request: crate::ProcessStartRequest::external(
                        "runtime-scenario-intent-child",
                        crate::ProcessOriginator::host_scoped("runtime-scenario"),
                        serde_json::json!({"kind": "start"}),
                    ),
                    on_parent_end: crate::ProcessParentEndPolicy::Abandon,
                })),
                crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
                    session_id: session_id.clone(),
                    process_id: "runtime-scenario-intent-target".to_string(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"kind": "signal"}),
                }),
                crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                    session_id: session_id.clone(),
                    process_id: "runtime-scenario-intent-target".to_string(),
                    event_type: "runtime.intent.note".to_string(),
                    payload: serde_json::json!({"kind": "emit"}),
                }),
                crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
                    session_id,
                    process_id: "runtime-scenario-intent-target".to_string(),
                    reason: Some("runtime scenario complete".to_string()),
                }),
            ]),
        )
    }
}

#[tokio::test]
async fn runtime_scenario_opted_in_provider_drains_every_v1_tool_intent() {
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool_provider: Arc<dyn crate::ToolProvider> = Arc::new(RuntimeScenarioIntentProvider {
        calls: Arc::clone(&provider_calls),
    });
    let model_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = crate::testing::TestProvider::builder()
        .kind("mock")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            move |_| {
                let model_calls = Arc::clone(&model_calls);
                async move {
                    Ok(
                        match model_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                            0 => crate::LlmResponse {
                                parts: vec![crate::LlmOutputPart::ToolCall {
                                    call_id: "runtime-scenario-intent-call".to_string(),
                                    tool_name: "runtime_scenario_intents".to_string(),
                                    input_json: "{}".to_string(),
                                    replay: None,
                                }],
                                response_metadata: Default::default(),
                                ..crate::LlmResponse::default()
                            },
                            1 => crate::LlmResponse {
                                full_text: "intent drain complete".to_string(),
                                parts: vec![crate::LlmOutputPart::Text {
                                    text: "intent drain complete".to_string(),
                                    response_meta: None,
                                }],
                                response_metadata: Default::default(),
                                ..crate::LlmResponse::default()
                            },
                            index => panic!("unexpected model call {index}"),
                        },
                    )
                }
            }
        })
        .build();
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tool_provider, transport).await;
    let registry = runtime
        .host
        .process_registry
        .as_ref()
        .expect("runtime scenario process registry")
        .clone();
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                "runtime-scenario-intent-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryDisposition::ExternallyOwned,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types([
                crate::ProcessEventType {
                    name: "signal.resume".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                },
                crate::ProcessEventType {
                    name: "runtime.intent.note".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                },
            ]),
            &["root".to_string()],
        )
        .await
        .expect("register runtime scenario intent target");

    let turn_scope = named_turn_scope("root", "runtime-scenario-intent-turn");
    let wake_controller = turn_scope
        .owned_controller()
        .expect("runtime scenario turn owns its controller");
    let wake_key = wake_controller
        .await_event_key(
            &crate::ExecutionScope::process("runtime-scenario-intent-target"),
            crate::AwaitEventWaitIdentity::process_signal(
                "runtime-scenario-intent-target",
                "resume",
                1,
            ),
        )
        .await
        .expect("mint runtime-tier process-signal wait");
    let wake_wait = {
        let wake_controller = Arc::clone(&wake_controller);
        let wake_key = wake_key.clone();
        crate::task::spawn(async move {
            wake_controller
                .await_await_event(&wake_key, tokio_util::sync::CancellationToken::new(), None)
                .await
        })
    };
    tokio::task::yield_now().await;
    let turn = runtime
        .run_turn_assembled(
            crate::TurnInput::text("run intent scenario"),
            tokio_util::sync::CancellationToken::new(),
            turn_scope,
        )
        .await
        .expect("run opted-in provider intent turn");
    assert_eq!(turn.assistant_output.safe_text, "intent drain complete");
    assert_eq!(provider_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), wake_wait)
            .await
            .expect("SignalProcess intent must wake the parked runtime-tier wait")
            .expect("runtime-tier wait task")
            .expect("runtime-tier wait resolution"),
        crate::Resolution::Ok(serde_json::json!({"kind": "signal"}))
    );
    let events = registry
        .events_after("runtime-scenario-intent-target", 0)
        .await
        .expect("read literal intent target events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "process.observer_added",
            "signal.resume",
            "runtime.intent.note",
            "process.cancel_requested",
        ]
    );
    assert!(
        registry
            .list_processes(&crate::ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..crate::ProcessListFilter::default()
            })
            .await
            .expect("list intent-created processes")
            .iter()
            .any(|record| matches!(
                record.input.as_ref(),
                crate::ProcessInput::External { metadata }
                    if metadata == &serde_json::json!({"kind": "start"})
            )),
        "the StartProcess declaration must realize through the runtime"
    );
}
