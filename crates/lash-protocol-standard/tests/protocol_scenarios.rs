use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::ProcessRegistry as _;
use lash_core::sansio::{self, ChatContextProjector, ProtocolDriverHandle, Response};
use lash_core::testing::behavior_transcript::Transcript;
use lash_core::testing::sansio_transcript::record_effects;
use lash_core::{
    CheckpointKind, Effect, LlmCallError, LlmOutputPart, LlmRequest, LlmResponse,
    LlmTerminalReason, Message, MessageRole, Part, ToolCallOutput, ToolFailure, ToolFailureClass,
    TurnMachine, TurnMachineConfig, facade_support::SessionStreamEvent,
    facade_support::TurnOutcome, facade_support::TurnStop,
};
use lash_protocol_standard::StandardDriver;

/// Actor name pinned on every Standard Protocol Scenario transcript. The harness
/// drives one sans-io machine for one session, so the whole transcript belongs to
/// this actor.
const STANDARD_TRANSCRIPT_ACTOR: &str = "standard";

#[derive(Clone, Copy, Debug)]
struct StandardProtocolScenarioCoverage {
    test_name: &'static str,
    declared_test: fn(),
    display_name: &'static str,
    owned_invariant: &'static str,
}

macro_rules! standard_protocol_coverage {
    ($test_fn:ident, $display_name:literal, $owned_invariant:literal) => {
        StandardProtocolScenarioCoverage {
            test_name: stringify!($test_fn),
            declared_test: $test_fn,
            display_name: $display_name,
            owned_invariant: $owned_invariant,
        }
    };
}

const PROJECTION: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_projects_initial_request,
    "projection",
    "Standard protocol projects user/system input into the first model request."
);
const EMPTY_MODEL_RESPONSE: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_empty_model_response_stops_provider_error,
    "empty response",
    "Empty provider response terminates through the protocol error boundary."
);
const PROVIDER_ERROR: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_provider_error_stops_without_checkpoint,
    "provider error",
    "Provider errors stop without committing a protocol checkpoint."
);
const NATIVE_TOOL_LOOP: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint,
    "native tool loop",
    "Native tool calls checkpoint and re-enter the model loop."
);
const PARALLEL_TOOL_CHECKPOINT: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_parallel_tool_results_checkpoint_once,
    "parallel tool checkpoint",
    "Parallel native tool results checkpoint once before protocol re-entry."
);
const TOOL_FAILURE_FEEDBACK: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_tool_failure_feedback_reenters_model_after_checkpoint,
    "tool failure feedback",
    "Tool failure is converted into model feedback after checkpoint."
);
const TOOL_INTENT_FEEDBACK: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_projects_every_v1_intent_outcome_into_model_feedback,
    "tool intent feedback",
    "Typed outcomes for every v1 intent kind survive the Standard tool boundary and enter the next model request."
);
const STREAMED_TEXT_TERMINATION: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_streamed_text_finishes_without_duplicate_delta,
    "streamed text termination",
    "Streaming text projection emits a clean final response without duplicate deltas."
);
const MAX_TURN_TERMINATION: StandardProtocolScenarioCoverage = standard_protocol_coverage!(
    standard_protocol_scenario_max_turns_terminates_after_tool_result,
    "max turn termination",
    "Tool-result continuation terminates at max-turns with the expected final message."
);

const STANDARD_PROTOCOL_SCENARIO_COVERAGE: &[StandardProtocolScenarioCoverage] = &[
    PROJECTION,
    EMPTY_MODEL_RESPONSE,
    PROVIDER_ERROR,
    NATIVE_TOOL_LOOP,
    PARALLEL_TOOL_CHECKPOINT,
    TOOL_FAILURE_FEEDBACK,
    TOOL_INTENT_FEEDBACK,
    STREAMED_TEXT_TERMINATION,
    MAX_TURN_TERMINATION,
];

#[test]
fn standard_protocol_scenario_coverage_metadata_is_unique_and_complete() {
    assert_eq!(STANDARD_PROTOCOL_SCENARIO_COVERAGE.len(), 9);
    let mut names = BTreeSet::new();
    for coverage in STANDARD_PROTOCOL_SCENARIO_COVERAGE {
        let _declared_test = coverage.declared_test;
        assert!(
            coverage
                .test_name
                .starts_with("standard_protocol_scenario_"),
            "unexpected Standard Protocol Scenario test name {}",
            coverage.test_name
        );
        assert!(!coverage.owned_invariant.trim().is_empty());
        assert!(
            names.insert(coverage.test_name),
            "duplicate Standard Protocol Scenario coverage metadata for {}",
            coverage.test_name
        );
    }
}

#[derive(Clone, Debug)]
struct StandardProtocolScenario {
    name: &'static str,
    user_message: &'static str,
    max_turns: Option<usize>,
    steps: Vec<StandardProtocolStep>,
    expectations: StandardProtocolExpectations,
}

impl StandardProtocolScenario {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            user_message: "",
            max_turns: None,
            steps: Vec::new(),
            expectations: StandardProtocolExpectations::default(),
        }
    }

    fn user_message(mut self, user_message: &'static str) -> Self {
        self.user_message = user_message;
        self
    }

    fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    fn llm_response(mut self, text_streamed: bool, parts: Vec<LlmOutputPart>) -> Self {
        self.steps.push(StandardProtocolStep::LlmResponse {
            text_streamed,
            parts,
        });
        self
    }

    fn llm_error(mut self, message: &'static str) -> Self {
        self.steps.push(StandardProtocolStep::LlmError(message));
        self
    }

    fn tool_results(mut self, results: Vec<StandardToolResult>) -> Self {
        self.steps.push(StandardProtocolStep::ToolResults(results));
        self
    }

    fn checkpoint(mut self) -> Self {
        self.steps.push(StandardProtocolStep::Checkpoint);
        self
    }

    fn expect(mut self, expectations: StandardProtocolExpectations) -> Self {
        self.expectations = expectations;
        self
    }

    fn run(self) -> StandardProtocolRun {
        let mut config = standard_config();
        config.turn_budget = self
            .max_turns
            .map(lash_core::TurnBudget::bounded)
            .unwrap_or(lash_core::TurnBudget::Unbounded);
        let mut machine = TurnMachine::new(
            config,
            vec![user_message(self.user_message)],
            Arc::new(Vec::new()),
            0,
        );
        let mut observed = StandardProtocolRun::default();
        // One sans-io machine, one session: name the actor instead of letting it
        // fall back to a positional alias.
        observed
            .transcript
            .pin(STANDARD_TRANSCRIPT_ACTOR, STANDARD_TRANSCRIPT_ACTOR);
        let mut effects = drain_effects(&mut machine);
        observed.record(&effects);
        record_effects(
            &mut observed.transcript,
            STANDARD_TRANSCRIPT_ACTOR,
            &effects,
        );
        observed.initial_request = find_llm_request(&effects).cloned();

        for step in &self.steps {
            match step {
                StandardProtocolStep::LlmResponse {
                    text_streamed,
                    parts,
                } => {
                    let llm_id = *find_llm_call(&effects).unwrap_or_else(|| {
                        panic!("{} expected pending LLM call before response", self.name)
                    });
                    machine.handle_response(Response::LlmComplete {
                        id: llm_id,
                        text_streamed: *text_streamed,
                        result: Ok(llm_response(parts.clone())),
                    });
                }
                StandardProtocolStep::LlmError(message) => {
                    let llm_id = *find_llm_call(&effects).unwrap_or_else(|| {
                        panic!("{} expected pending LLM call before error", self.name)
                    });
                    machine.handle_response(Response::LlmComplete {
                        id: llm_id,
                        text_streamed: false,
                        result: Err(llm_error(message)),
                    });
                }
                StandardProtocolStep::ToolResults(results) => {
                    let (tool_id, calls) = effects
                        .iter()
                        .find_map(|effect| match effect {
                            Effect::ToolCalls { id, calls } => Some((*id, calls.clone())),
                            _ => None,
                        })
                        .unwrap_or_else(|| {
                            panic!("{} expected pending native tool calls", self.name)
                        });
                    assert_eq!(
                        calls
                            .iter()
                            .map(|call| (call.call_id.as_str(), call.tool_name.as_str()))
                            .collect::<Vec<_>>(),
                        results
                            .iter()
                            .map(|result| (result.call_id, result.tool_name))
                            .collect::<Vec<_>>(),
                        "{} native tool calls changed",
                        self.name
                    );
                    observed.intent_outcomes.extend(
                        results
                            .iter()
                            .flat_map(|result| result.intent_outcomes.iter().cloned()),
                    );
                    machine.handle_response(Response::ToolResults {
                        id: tool_id,
                        results: calls
                            .iter()
                            .zip(results)
                            .map(|(call, result)| result.completed_call(call.args.clone()))
                            .collect(),
                    });
                }
                StandardProtocolStep::Checkpoint => {
                    let (checkpoint_id, _) = find_checkpoint(&effects)
                        .unwrap_or_else(|| panic!("{} expected checkpoint", self.name));
                    machine.handle_response(Response::Checkpoint {
                        id: checkpoint_id,
                        delivery: sansio::CheckpointDelivery::default(),
                    });
                }
            }

            effects = drain_effects(&mut machine);
            observed.record(&effects);
            record_effects(
                &mut observed.transcript,
                STANDARD_TRANSCRIPT_ACTOR,
                &effects,
            );
        }

        self.expectations.assert(self.name, &observed, &machine);
        observed
    }
}

#[derive(Clone, Debug)]
enum StandardProtocolStep {
    LlmResponse {
        text_streamed: bool,
        parts: Vec<LlmOutputPart>,
    },
    LlmError(&'static str),
    ToolResults(Vec<StandardToolResult>),
    Checkpoint,
}

#[derive(Clone, Debug)]
struct StandardToolResult {
    call_id: &'static str,
    tool_name: &'static str,
    output: ToolCallOutput,
    model_return_text: &'static str,
    intent_outcomes: Vec<lash_core::ToolIntentExecutionOutcome>,
}

impl StandardToolResult {
    fn ok(
        call_id: &'static str,
        tool_name: &'static str,
        output: serde_json::Value,
        model_return_text: &'static str,
    ) -> Self {
        Self {
            call_id,
            tool_name,
            output: ToolCallOutput::success(output),
            model_return_text,
            intent_outcomes: Vec::new(),
        }
    }

    fn failure(
        call_id: &'static str,
        tool_name: &'static str,
        code: &'static str,
        message: &'static str,
        model_return_text: &'static str,
    ) -> Self {
        Self {
            call_id,
            tool_name,
            output: ToolCallOutput::failure(ToolFailure::tool(
                ToolFailureClass::Execution,
                code,
                message,
            )),
            model_return_text,
            intent_outcomes: Vec::new(),
        }
    }

    fn completed_call(&self, args: serde_json::Value) -> sansio::CompletedToolCall {
        sansio::CompletedToolCall {
            call_id: self.call_id.to_string(),
            tool_name: self.tool_name.to_string(),
            args,
            output: self.output.clone(),
            model_return: lash_core::facade_support::ModelToolReturn {
                call_id: self.call_id.to_string(),
                tool_name: self.tool_name.to_string(),
                parts: std::iter::once(lash_core::facade_support::ModelToolReturnPart::text(
                    self.model_return_text,
                ))
                .chain(self.intent_outcomes.iter().map(|outcome| {
                    lash_core::facade_support::ModelToolReturnPart::text(outcome.model_addendum())
                }))
                .collect(),
            },
            duration_ms: 1,
            intent_outcomes: self.intent_outcomes.clone(),
            replay: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct StandardProtocolExpectations {
    initial_request_contains: Vec<&'static str>,
    tool_calls: Vec<ExpectedToolCall>,
    checkpoints: Vec<CheckpointKind>,
    llm_call_count: Option<usize>,
    done: Option<bool>,
    no_text_deltas: bool,
    error_contains: Vec<&'static str>,
    turn_outcome: Option<TurnOutcome>,
    model_requests_contain: Vec<&'static str>,
    intent_kinds: Vec<lash_core::ToolIntentKind>,
}

impl StandardProtocolExpectations {
    fn assert(&self, scenario_name: &str, run: &StandardProtocolRun, machine: &TurnMachine) {
        let initial_request = run
            .initial_request
            .as_ref()
            .unwrap_or_else(|| panic!("{scenario_name} did not project an initial LLM request"));
        let initial_request_text = format!("{:?}", initial_request.messages);
        for expected in &self.initial_request_contains {
            assert!(
                initial_request_text.contains(expected),
                "{scenario_name} initial projection omitted `{expected}`: {initial_request_text}"
            );
        }
        assert_eq!(
            run.tool_calls, self.tool_calls,
            "{scenario_name} native tool-call sequence changed"
        );
        assert_eq!(
            run.checkpoints, self.checkpoints,
            "{scenario_name} checkpoint sequence changed"
        );
        if let Some(llm_call_count) = self.llm_call_count {
            assert_eq!(
                run.llm_call_count, llm_call_count,
                "{scenario_name} LLM call count changed"
            );
        }
        if self.no_text_deltas {
            assert!(
                run.text_deltas.is_empty(),
                "{scenario_name} emitted duplicate text deltas for streamed text: {:?}",
                run.text_deltas
            );
        }
        for expected in &self.error_contains {
            assert!(
                run.errors.iter().any(|error| error.contains(expected)),
                "{scenario_name} missing error containing `{expected}`: {:?}",
                run.errors
            );
        }
        if let Some(done) = self.done {
            assert_eq!(
                machine.is_done(),
                done,
                "{scenario_name} done state changed"
            );
        }
        if let Some(expected) = &self.turn_outcome {
            assert!(
                run.turn_outcomes.iter().any(|outcome| outcome == expected),
                "{scenario_name} missing turn outcome {expected:?}: {:?}",
                run.turn_outcomes
            );
        }
        for expected in &self.model_requests_contain {
            assert!(
                run.model_request_texts
                    .iter()
                    .any(|request| request.contains(expected)),
                "{scenario_name} omitted `{expected}` from its model requests: {:?}",
                run.model_request_texts
            );
        }
        assert_eq!(
            run.intent_outcomes
                .iter()
                .filter_map(lash_core::ToolIntentExecutionOutcome::kind)
                .collect::<Vec<_>>(),
            self.intent_kinds,
            "{scenario_name} typed intent evidence changed"
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedToolCall {
    call_id: String,
    tool_name: String,
    args: serde_json::Value,
}

#[derive(Default)]
struct StandardProtocolRun {
    /// Behavior transcript built from the machine's own effect stream, in drain
    /// order. See `lash_core::testing::sansio_transcript`.
    transcript: Transcript,
    initial_request: Option<LlmRequest>,
    tool_calls: Vec<ExpectedToolCall>,
    checkpoints: Vec<CheckpointKind>,
    llm_call_count: usize,
    text_deltas: Vec<String>,
    errors: Vec<String>,
    turn_outcomes: Vec<TurnOutcome>,
    model_request_texts: Vec<String>,
    intent_outcomes: Vec<lash_core::ToolIntentExecutionOutcome>,
}

impl StandardProtocolRun {
    fn record(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::LlmCall { request, .. } => {
                    self.llm_call_count += 1;
                    self.model_request_texts
                        .push(format!("{:?}", request.messages));
                }
                Effect::ToolCalls { calls, .. } => {
                    self.tool_calls
                        .extend(calls.iter().map(|call| ExpectedToolCall {
                            call_id: call.call_id.clone(),
                            tool_name: call.tool_name.clone(),
                            args: call.args.clone(),
                        }));
                }
                Effect::Checkpoint { checkpoint, .. } => self.checkpoints.push(*checkpoint),
                Effect::Emit(SessionStreamEvent::TextDelta { content }) => {
                    self.text_deltas.push(content.clone());
                }
                Effect::Emit(SessionStreamEvent::Error { message, .. }) => {
                    self.errors.push(message.clone());
                }
                Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) => {
                    self.turn_outcomes.push(outcome.clone());
                }
                _ => {}
            }
        }
    }
}

fn standard_config() -> TurnMachineConfig {
    let protocol_driver: Arc<dyn ProtocolDriverHandle<lash_core::HostTurnProtocol>> =
        Arc::new(StandardDriver);
    TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        sync_execution_environment: false,
        model: "test-model".to_string(),
        max_context_tokens: None,
        turn_budget: lash_core::TurnBudget::Unbounded,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Vec::new().into(),
        system_prompt: std::sync::Arc::from(""),
        session_id: "standard-protocol-scenario".to_string(),
        turn_id: "standard-protocol-turn".to_string(),
        emit_llm_trace: false,
        termination: lash_core::ProtocolTurnOptions::empty(),
        turn_limit_final_message: Arc::new(test_turn_limit_final_message),
    }
}

fn test_turn_limit_final_message(message_id: String, max_turns: usize) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: lash_core::facade_support::shared_parts(vec![Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before a final test response."),
        )]),
        origin: None,
    }
}

fn user_message(content: &str) -> Message {
    Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text("m0.p0".to_string(), content.to_string(), None)].into(),
        origin: None,
    }
}

fn drain_effects(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        effects.push(effect);
    }
    effects
}

fn find_llm_call(effects: &[Effect]) -> Option<&sansio::EffectId> {
    effects.iter().find_map(|effect| match effect {
        Effect::LlmCall { id, .. } => Some(id),
        _ => None,
    })
}

fn find_llm_request(effects: &[Effect]) -> Option<&LlmRequest> {
    effects.iter().find_map(|effect| match effect {
        Effect::LlmCall { request, .. } => Some(request.as_ref()),
        _ => None,
    })
}

fn find_checkpoint(effects: &[Effect]) -> Option<(sansio::EffectId, CheckpointKind)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Checkpoint { id, checkpoint } => Some((*id, *checkpoint)),
        _ => None,
    })
}

fn text_part(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}

fn tool_call_part(call_id: &str, tool_name: &str, input_json: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        input_json: input_json.to_string(),
        replay: None,
    }
}

fn llm_response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    let full_text = parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    LlmResponse {
        full_text,
        parts,
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn llm_error(message: &str) -> LlmCallError {
    LlmCallError {
        message: message.to_string(),
        retryable: false,
        kind: lash_core::ProviderFailureKind::Unknown,
        raw: None,
        code: Some("test_provider_error".to_string()),
        terminal_reason: LlmTerminalReason::ProviderError,
        request_body: None,
        partial_response: None,
    }
}

#[test]
fn standard_protocol_scenario_projects_initial_request() {
    StandardProtocolScenario::new(PROJECTION.display_name)
        .user_message("hello standard protocol")
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["hello standard protocol"],
            llm_call_count: Some(1),
            done: Some(false),
            ..StandardProtocolExpectations::default()
        })
        .run();
}

#[test]
fn standard_protocol_scenario_empty_model_response_stops_provider_error() {
    StandardProtocolScenario::new(EMPTY_MODEL_RESPONSE.display_name)
        .user_message("answer with something")
        .llm_response(false, vec![])
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["answer with something"],
            llm_call_count: Some(1),
            done: Some(true),
            error_contains: vec!["Model returned no assistant text or tool calls."],
            turn_outcome: Some(TurnOutcome::Stopped(TurnStop::ProviderError)),
            ..StandardProtocolExpectations::default()
        })
        .run();
}

#[test]
fn standard_protocol_scenario_provider_error_stops_without_checkpoint() {
    StandardProtocolScenario::new(PROVIDER_ERROR.display_name)
        .user_message("trigger provider failure")
        .llm_error("upstream provider unavailable")
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["trigger provider failure"],
            checkpoints: vec![],
            llm_call_count: Some(1),
            done: Some(true),
            error_contains: vec!["LLM error: upstream provider unavailable"],
            turn_outcome: Some(TurnOutcome::Stopped(TurnStop::ProviderError)),
            ..StandardProtocolExpectations::default()
        })
        .run();
}

#[test]
fn standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint() {
    let run = StandardProtocolScenario::new(NATIVE_TOOL_LOOP.display_name)
        .user_message("read file")
        .llm_response(
            false,
            vec![
                text_part("Let me read that."),
                tool_call_part("tc1", "read_file", r#"{"path":"foo.txt"}"#),
            ],
        )
        .tool_results(vec![StandardToolResult::ok(
            "tc1",
            "read_file",
            serde_json::json!("file contents"),
            "file contents",
        )])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["read file"],
            tool_calls: vec![ExpectedToolCall {
                call_id: "tc1".to_string(),
                tool_name: "read_file".to_string(),
                args: serde_json::json!({"path":"foo.txt"}),
            }],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            done: Some(false),
            ..StandardProtocolExpectations::default()
        })
        .run();
    insta::assert_snapshot!(run.transcript.render(), @r#"
    standard     provider  model.request           messages=1 tools=0
    standard     tool      tool.call               name="read_file" call=call-001
    standard     tool      tool.result             name="read_file" outcome=success call=call-001
    standard     commit    checkpoint.request      checkpoint=after_work
    standard                 usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
    standard     provider  model.request           messages=3 tools=0
    "#);
}

struct StandardIntentProvider;

fn standard_intent_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:intent_leaf",
        "intent_leaf",
        "Return all four v1 tool intents.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for StandardIntentProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![standard_intent_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "intent_leaf").then(|| Arc::new(standard_intent_tool().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        panic!("Standard intent scenario must use AttemptContext")
    }

    fn supports_attempt_context(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == standard_intent_tool().id()
    }

    async fn execute_attempt(
        &self,
        call: lash_core::AttemptToolCall<'_>,
    ) -> lash_core::ToolAttemptResult {
        let session_id = call.context.session_id().to_string();
        lash_core::ToolAttemptResult::done(
            lash_core::ToolResultDone::ok(serde_json::json!({"provider": "done"})),
            lash_core::ToolIntents::v1(vec![
                lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
                    session_id: session_id.clone(),
                    request: lash_core::ProcessStartRequest::external(
                        "standard-intent-child",
                        lash_core::ProcessOriginator::host_scoped("standard-scenario"),
                        serde_json::json!({"kind": "start"}),
                    ),
                    on_parent_end: lash_core::ProcessParentEndPolicy::Abandon,
                })),
                lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
                    session_id: session_id.clone(),
                    process_id: "standard-intent-target".to_string(),
                    signal_name: "resume".to_string(),
                    payload: serde_json::json!({"kind": "signal"}),
                }),
                lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
                    session_id: session_id.clone(),
                    process_id: "standard-intent-target".to_string(),
                    event_type: "standard.intent.note".to_string(),
                    payload: serde_json::json!({"kind": "emit"}),
                }),
                lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
                    session_id,
                    process_id: "standard-intent-target".to_string(),
                    reason: Some("standard scenario complete".to_string()),
                }),
            ]),
        )
    }
}

#[tokio::test]
async fn standard_protocol_scenario_projects_every_v1_intent_outcome_into_model_feedback() {
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "standard-intent-target",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([
                lash_core::ProcessEventType {
                    name: "signal.resume".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
                lash_core::ProcessEventType {
                    name: "standard.intent.note".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
            &["standard-protocol-scenario".to_string()],
        )
        .await
        .expect("register Standard intent target");
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("standard-scenario")
        .complete({
            let requests = Arc::clone(&requests);
            let model_calls = Arc::clone(&model_calls);
            move |request| {
                let requests = Arc::clone(&requests);
                let model_calls = Arc::clone(&model_calls);
                async move {
                    requests
                        .lock()
                        .expect("record Standard model request")
                        .push(
                            serde_json::to_string(&request.messages).expect("serialize messages"),
                        );
                    Ok(match model_calls.fetch_add(1, Ordering::SeqCst) {
                        0 => lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::ToolCall {
                                call_id: "tc-intents".to_string(),
                                tool_name: "intent_leaf".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        1 => lash_core::LlmResponse {
                            full_text: "intent feedback observed".to_string(),
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text: "intent feedback observed".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        },
                        index => panic!("unexpected Standard model call {index}"),
                    })
                }
            }
        })
        .build();
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(StandardIntentProvider);
    let mut factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory,
    )];
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "standard-intent-tools",
        lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
    )));
    let policy = lash_core::SessionPolicy {
        provider_id: "standard-scenario".into(),
        model: lash_core::ModelSpec::builder("standard-scenario-model")
            .context_window_tokens(100_000)
            .build()
            .expect("Standard scenario model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let mut runtime = lash_core::facade_support::LashRuntime::builder(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
    )
    .with_session_id("standard-protocol-scenario")
    .with_policy(policy)
    .with_plugin_factories(factories)
    .with_provider_resolver(Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider.into_handle()),
    ))
    .with_process_registry(registry)
    .build()
    .await
    .expect("build Standard intent runtime");
    let turn = runtime
        .run_turn_assembled(
            lash_core::TurnInput::text("run durable follow-on work"),
            tokio_util::sync::CancellationToken::new(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
                lash_core::ExecutionScope::turn(
                    "standard-protocol-scenario",
                    "standard-protocol-turn",
                ),
            )
            .expect("Standard scenario turn scope"),
        )
        .await
        .expect("run Standard intent turn");
    assert_eq!(turn.assistant_output.safe_text, "intent feedback observed");
    let requests = requests.lock().expect("read Standard requests");
    assert_eq!(requests.len(), 2);
    for literal in [
        "[tool intent start_process #0 executed:",
        "[tool intent signal_process #1 executed:",
        "[tool intent emit_process_event #2 executed:",
        "[tool intent cancel_process #3 executed:",
    ] {
        assert!(
            requests[1].contains(literal),
            "missing `{literal}` in {}",
            requests[1]
        );
    }
}

#[test]
fn standard_protocol_scenario_parallel_tool_results_checkpoint_once() {
    let run = StandardProtocolScenario::new(PARALLEL_TOOL_CHECKPOINT.display_name)
        .user_message("read two files")
        .llm_response(
            false,
            vec![
                tool_call_part("tc1", "read_file", r#"{"path":"left.txt"}"#),
                tool_call_part("tc2", "read_file", r#"{"path":"right.txt"}"#),
            ],
        )
        .tool_results(vec![
            StandardToolResult::ok(
                "tc1",
                "read_file",
                serde_json::json!("left contents"),
                "left contents",
            ),
            StandardToolResult::ok(
                "tc2",
                "read_file",
                serde_json::json!("right contents"),
                "right contents",
            ),
        ])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["read two files"],
            tool_calls: vec![
                ExpectedToolCall {
                    call_id: "tc1".to_string(),
                    tool_name: "read_file".to_string(),
                    args: serde_json::json!({"path":"left.txt"}),
                },
                ExpectedToolCall {
                    call_id: "tc2".to_string(),
                    tool_name: "read_file".to_string(),
                    args: serde_json::json!({"path":"right.txt"}),
                },
            ],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            done: Some(false),
            ..StandardProtocolExpectations::default()
        })
        .run();
    // Expect test: the reviewable artifact is the batch order and the single
    // checkpoint between the tool results and model re-entry — the defect class
    // ADR 0044 names (batch ordering reversed, serial tools run in parallel).
    // The expectations above still own the invariants.
    insta::assert_snapshot!(run.transcript.render(), @r#"
    standard     provider  model.request           messages=1 tools=0
    standard     tool      tool.call               name="read_file" call=call-001
    standard     tool      tool.call               name="read_file" call=call-002
    standard     tool      tool.result             name="read_file" outcome=success call=call-001
    standard     tool      tool.result             name="read_file" outcome=success call=call-002
    standard     commit    checkpoint.request      checkpoint=after_work
    standard                 usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
    standard     provider  model.request           messages=3 tools=0
    "#);
}

#[test]
fn standard_protocol_scenario_tool_failure_feedback_reenters_model_after_checkpoint() {
    let run = StandardProtocolScenario::new(TOOL_FAILURE_FEEDBACK.display_name)
        .user_message("search docs")
        .llm_response(
            false,
            vec![tool_call_part(
                "tc1",
                "search",
                r#"{"query":"missing term"}"#,
            )],
        )
        .tool_results(vec![StandardToolResult::failure(
            "tc1",
            "search",
            "search_failed",
            "index unavailable",
            "search failed: index unavailable",
        )])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["search docs"],
            tool_calls: vec![ExpectedToolCall {
                call_id: "tc1".to_string(),
                tool_name: "search".to_string(),
                args: serde_json::json!({"query":"missing term"}),
            }],
            checkpoints: vec![CheckpointKind::AfterWork],
            llm_call_count: Some(2),
            done: Some(false),
            ..StandardProtocolExpectations::default()
        })
        .run();
    insta::assert_snapshot!(run.transcript.render(), @r#"
    standard     provider  model.request           messages=1 tools=0
    standard     tool      tool.call               name="search" call=call-001
    standard     tool      tool.result             name="search" outcome=failure call=call-001
    standard     commit    checkpoint.request      checkpoint=after_work
    standard                 usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
    standard     provider  model.request           messages=3 tools=0
    "#);
}

#[test]
fn standard_protocol_scenario_streamed_text_finishes_without_duplicate_delta() {
    StandardProtocolScenario::new(STREAMED_TEXT_TERMINATION.display_name)
        .user_message("answer directly")
        .llm_response(true, vec![text_part("streamed done")])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["answer directly"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            llm_call_count: Some(1),
            done: Some(true),
            no_text_deltas: true,
            turn_outcome: Some(TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage {
                    text: "streamed done".to_string(),
                },
            )),
            ..StandardProtocolExpectations::default()
        })
        .run();
}

#[test]
fn standard_protocol_scenario_max_turns_terminates_after_tool_result() {
    StandardProtocolScenario::new(MAX_TURN_TERMINATION.display_name)
        .user_message("use a tool once")
        .max_turns(1)
        .llm_response(false, vec![tool_call_part("tc1", "test", "{}")])
        .tool_results(vec![StandardToolResult::ok(
            "tc1",
            "test",
            serde_json::json!("ok"),
            "ok",
        )])
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["use a tool once"],
            tool_calls: vec![ExpectedToolCall {
                call_id: "tc1".to_string(),
                tool_name: "test".to_string(),
                args: serde_json::json!({}),
            }],
            llm_call_count: Some(1),
            done: Some(true),
            turn_outcome: Some(TurnOutcome::Stopped(TurnStop::MaxTurns)),
            ..StandardProtocolExpectations::default()
        })
        .run();
}
