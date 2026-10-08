use lash_sansio::TurnId;
use std::sync::Arc;

use lash_core::sansio::{self, ChatContextProjector, ProtocolDriverHandle, Response};
use lash_core::testing::behavior_transcript::Transcript;
use lash_core::testing::sansio_transcript::record_effects;
use lash_core::{
    CheckpointKind, Effect, LlmCallError, LlmOutputPart, LlmRequest, LlmResponse,
    LlmTerminalReason, Message, MessageRole, Part, ToolCallOutput, ToolFailure, ToolFailureClass,
    TurnMachine, TurnMachineConfig, facade_support::SessionStreamEvent, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::TurnStop,
};
use lash_protocol_standard::StandardDriver;

/// Actor name pinned on every Standard Protocol Scenario transcript. The harness
/// executes one sans-io machine for one session, so the whole transcript belongs to
/// this actor.
const STANDARD_TRANSCRIPT_ACTOR: &str = "standard";

#[derive(Clone, Copy, Debug)]
struct StandardProtocolScenarioCoverage {
    display_name: &'static str,
}

const PROJECTION: StandardProtocolScenarioCoverage = StandardProtocolScenarioCoverage {
    display_name: "projection",
};
const EMPTY_MODEL_RESPONSE: StandardProtocolScenarioCoverage = StandardProtocolScenarioCoverage {
    display_name: "empty response",
};
const PROVIDER_ERROR: StandardProtocolScenarioCoverage = StandardProtocolScenarioCoverage {
    display_name: "provider error",
};
const NATIVE_TOOL_LOOP: StandardProtocolScenarioCoverage = StandardProtocolScenarioCoverage {
    display_name: "native tool loop",
};
const PARALLEL_TOOL_CHECKPOINT: StandardProtocolScenarioCoverage =
    StandardProtocolScenarioCoverage {
        display_name: "parallel tool checkpoint",
    };
const TOOL_FAILURE_FEEDBACK: StandardProtocolScenarioCoverage = StandardProtocolScenarioCoverage {
    display_name: "tool failure feedback",
};
const STREAMED_TEXT_TERMINATION: StandardProtocolScenarioCoverage =
    StandardProtocolScenarioCoverage {
        display_name: "streamed text termination",
    };
const BUFFERED_TEXT_TERMINATION: StandardProtocolScenarioCoverage =
    StandardProtocolScenarioCoverage {
        display_name: "buffered text termination",
    };
const MAX_TURN_TERMINATION: StandardProtocolScenarioCoverage = StandardProtocolScenarioCoverage {
    display_name: "max turn termination",
};

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

    fn checkpoint_with_user_message(mut self, message: &'static str) -> Self {
        self.steps
            .push(StandardProtocolStep::CheckpointWithUserMessage(message));
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
            Default::default(),
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
                            Effect::ToolCalls { id, calls, .. } => Some((*id, calls.clone())),
                            _ => None,
                        })
                        .unwrap_or_else(|| {
                            panic!("{} expected pending native tool calls", self.name)
                        });
                    assert_eq!(
                        calls
                            .iter()
                            .map(|call| {
                                (
                                    call.provider_call_id.as_deref().unwrap_or_default(),
                                    call.tool_name.as_str(),
                                )
                            })
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
                            .map(|(call, result)| result.completed_call(call))
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
                StandardProtocolStep::CheckpointWithUserMessage(message) => {
                    let (checkpoint_id, _) = find_checkpoint(&effects)
                        .unwrap_or_else(|| panic!("{} expected checkpoint", self.name));
                    machine.handle_response(Response::Checkpoint {
                        id: checkpoint_id,
                        delivery: sansio::CheckpointDelivery {
                            committed_user_messages: vec![lash_core::Message {
                                id: "checkpoint-user".to_string(),
                                role: MessageRole::User,
                                parts: vec![lash_core::Part::text(
                                    "checkpoint-user.p0".to_string(),
                                    (*message).to_string(),
                                    None,
                                )]
                                .into(),
                                origin: None,
                                reply_marker: None,
                            }],
                        },
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
        let rendered = observed.transcript.render();
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("checkpoint.request"))
                .count(),
            observed.checkpoints.len()
        );
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
    CheckpointWithUserMessage(&'static str),
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

    fn completed_call(&self, call: &sansio::PendingToolCall) -> sansio::CompletedToolCall {
        sansio::CompletedToolCall {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool_name: self.tool_name.to_string(),
            args: call.args.clone(),
            output: self.output.clone(),
            model_return: lash_core::facade_support::ModelToolReturn {
                tool_name: self.tool_name.to_string(),
                parts: std::iter::once(lash_core::facade_support::ModelToolReturnPart::text(
                    self.model_return_text,
                ))
                .chain(self.intent_outcomes.iter().map(|outcome| {
                    lash_core::facade_support::ModelToolReturnPart::text(outcome.model_addendum())
                }))
                .collect(),
                attachment_notices: Vec::new(),
            },
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
                            call_id: call.provider_call_id.clone().unwrap_or_default(),
                            tool_name: call.tool_name.clone(),
                            args: call.args.clone(),
                        }));
                }
                Effect::Checkpoint { checkpoint, .. } => self.checkpoints.push(*checkpoint),
                Effect::Emit(SessionStreamEvent::TextDelta { content, .. }) => {
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
        Arc::new(StandardDriver::default());
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "test-model".to_string(),
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                    lash_sansio::llm::capability::CacheRetention::Short,
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::Unbounded,
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        session_id: lash_core::SessionId::from("standard-protocol-scenario"),
        agent_frame_id: "standard-frame".to_string(),
        turn_id: TurnId::from("standard-protocol-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: lash_core::ProtocolTurnOptions::empty(),
    }
}

fn user_message(content: &str) -> Message {
    Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text("m0.p0".to_string(), content.to_string(), None)].into(),
        origin: None,
        reply_marker: None,
    }
}

/// Every ready effect, with each execution-environment sync answered by an
/// empty environment on the way.
fn drain_effects(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        if let Effect::SyncExecutionEnvironment { id } = effect {
            machine.handle_response(sansio::Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(sansio::ExecutionEnvironmentSync::default()),
            });
            continue;
        }
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
    LlmResponse {
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
        code: Some(lash_sansio::FailureCode::provider("test_provider_error")),
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
fn standard_protocol_scenario_empty_model_response_finishes_after_checkpoint() {
    StandardProtocolScenario::new(EMPTY_MODEL_RESPONSE.display_name)
        .user_message("answer with something")
        .llm_response(false, vec![])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["answer with something"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            llm_call_count: Some(1),
            done: Some(true),
            turn_outcome: Some(TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: String::new(),
            })),
            ..StandardProtocolExpectations::default()
        })
        .run();
}

#[test]
fn post_tool_empty_model_response_finishes_without_repeating_the_tool() {
    StandardProtocolScenario::new("post-tool empty response")
        .user_message("read file and do nothing else")
        .llm_response(
            false,
            vec![tool_call_part("tc1", "read_file", r#"{"path":"foo.txt"}"#)],
        )
        .tool_results(vec![StandardToolResult::ok(
            "tc1",
            "read_file",
            serde_json::json!("file contents"),
            "file contents",
        )])
        .checkpoint()
        .llm_response(false, vec![])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["read file and do nothing else"],
            tool_calls: vec![ExpectedToolCall {
                call_id: "tc1".to_string(),
                tool_name: "read_file".to_string(),
                args: serde_json::json!({"path":"foo.txt"}),
            }],
            checkpoints: vec![CheckpointKind::AfterWork, CheckpointKind::BeforeCompletion],
            llm_call_count: Some(2),
            done: Some(true),
            turn_outcome: Some(TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: String::new(),
            })),
            ..StandardProtocolExpectations::default()
        })
        .run();
}

#[test]
fn empty_model_response_checkpoint_delivers_pending_input_before_completion() {
    StandardProtocolScenario::new("empty response with pending input")
        .user_message("answer only after the pending input")
        .llm_response(false, vec![])
        .checkpoint_with_user_message("pending follow-up")
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["answer only after the pending input"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            llm_call_count: Some(2),
            done: Some(false),
            model_requests_contain: vec!["pending follow-up"],
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
    standard     provider  model.request           messages=3 tools=0
    "#);
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
fn standard_protocol_scenario_buffered_text_finishes_with_the_response_text() {
    StandardProtocolScenario::new(BUFFERED_TEXT_TERMINATION.display_name)
        .user_message("answer directly")
        .llm_response(false, vec![text_part("final answer")])
        .checkpoint()
        .expect(StandardProtocolExpectations {
            initial_request_contains: vec!["answer directly"],
            checkpoints: vec![CheckpointKind::BeforeCompletion],
            llm_call_count: Some(1),
            done: Some(true),
            turn_outcome: Some(TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage {
                    text: "final answer".to_string(),
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

#[derive(Clone, Copy)]
enum PublicWork {
    Model,
    Tool,
    Code,
    Checkpoint,
}
struct ThirdPartyDriver(PublicWork);
impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for ThirdPartyDriver {
    fn prepare_protocol_iteration(
        &self,
        ctx: lash_core::DriverContextView<'_>,
    ) -> Vec<lash_core::DriverAction> {
        use lash_core::sansio::{CheckpointResumeAction, PendingToolCall, PendingWork};
        let work = match self.0 {
            PublicWork::Model => PendingWork::Llm {
                request: match ctx.project_llm_request(true) {
                    Ok(request) => request,
                    Err(error) => {
                        return lash_sansio::sansio::stored_history_refusal_actions(error);
                    }
                },
                driver_state: None,
            },
            PublicWork::Tool => PendingWork::WaitingForToolResults {
                settled: None,
                calls: vec![PendingToolCall {
                    call_id: lash_core::ToolCallId::fixture("third-party-call"),
                    provider_call_id: Some("provider-call".into()),
                    tool_name: "external_tool".into(),
                    args: serde_json::json!({"argument": 17}),
                    replay: None,
                }],
                expansion: Default::default(),
            },
            PublicWork::Code => PendingWork::Exec {
                language: "typescript".into(),
                code: "finish(17)".into(),
                driver_state: lash_core::ProtocolDriverState::new(
                    "external",
                    serde_json::json!({"pending":17}),
                ),
            },
            PublicWork::Checkpoint => PendingWork::Checkpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(public_protocol_outcome()),
            },
        };
        vec![lash_core::DriverAction::Start(work)]
    }
    fn handle_llm_success(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _state: Option<lash_core::ProtocolDriverState>,
        response: LlmResponse,
        _calls: &lash_core::sansio::ResponseToolCalls,
        _streamed: bool,
    ) -> Vec<lash_core::DriverAction> {
        assert_eq!(response.full_text(), "external provider result");
        vec![lash_core::DriverAction::Finish(public_protocol_outcome())]
    }
    fn handle_tool_results(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::Finish(public_protocol_outcome())]
    }
    fn handle_exec_result(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _state: lash_core::ProtocolDriverState,
        _result: Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::Finish(public_protocol_outcome())]
    }
}
fn public_protocol_outcome() -> TurnOutcome {
    TurnOutcome::Finished(TurnFinish::AssistantMessage {
        text: "external terminal witness".into(),
    })
}
fn external_config(work: PublicWork) -> TurnMachineConfig {
    let mut config = standard_config();
    config.protocol_driver = Arc::new(ThirdPartyDriver(work));
    config
}
#[test]
fn third_party_protocol_runs_using_only_public_seams() {
    let mut machine = TurnMachine::new(
        external_config(PublicWork::Model),
        vec![user_message("external input")],
        Default::default(),
        0,
    );
    let effects = drain_effects(&mut machine);
    let (id, request) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LlmCall { id, request } => Some((*id, request)),
            _ => None,
        })
        .unwrap();
    assert!(format!("{:?}", request.messages).contains("external input"));
    machine.handle_response(Response::LlmComplete {
        id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts: vec![text_part("external provider result")],
            ..Default::default()
        }),
    });
    let terminal = drain_effects(&mut machine);
    assert!(machine.is_done());
    assert!(terminal.iter().any(|effect| matches!(effect, Effect::Emit(SessionStreamEvent::TurnOutcome { outcome }) if *outcome == TurnOutcome::Finished(TurnFinish::AssistantMessage { text: "external terminal witness".into() }))));
    assert!(
        terminal
            .iter()
            .any(|effect| matches!(effect, Effect::Done { .. }))
    );
}
#[test]
fn public_effect_emission_contract_matrix() {
    for work in [
        PublicWork::Model,
        PublicWork::Tool,
        PublicWork::Code,
        PublicWork::Checkpoint,
    ] {
        let mut machine = TurnMachine::new(
            external_config(work),
            vec![user_message("public input")],
            Default::default(),
            0,
        );
        let first = drain_effects(&mut machine);
        let checkpoint =
            serde_json::from_slice(&serde_json::to_vec(&machine.checkpoint()).unwrap()).unwrap();
        let mut restored =
            TurnMachine::restore_from_checkpoint(external_config(work), checkpoint, None).unwrap();
        let replayed = drain_effects(&mut restored);
        for effects in [&first, &replayed] {
            let waiting = effects
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        Effect::LlmCall { .. }
                            | Effect::ToolCalls { .. }
                            | Effect::ExecCode { .. }
                            | Effect::Checkpoint { .. }
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(waiting.len(), 1);
            match (work, waiting[0]) {
                (PublicWork::Model, Effect::LlmCall { request, .. }) => {
                    assert!(format!("{:?}", request.messages).contains("public input"))
                }
                (PublicWork::Tool, Effect::ToolCalls { calls, .. }) => {
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].tool_name, "external_tool");
                    assert_eq!(calls[0].args, serde_json::json!({"argument":17}));
                }
                (PublicWork::Code, Effect::ExecCode { code, .. }) => assert_eq!(code, "finish(17)"),
                (PublicWork::Checkpoint, Effect::Checkpoint { checkpoint, .. }) => {
                    assert_eq!(*checkpoint, CheckpointKind::BeforeCompletion)
                }
                _ => panic!("public work emitted the wrong effect"),
            }
        }
        let ids = |effects: &[Effect]| {
            effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::LlmCall { id, .. }
                    | Effect::ToolCalls { id, .. }
                    | Effect::ExecCode { id, .. }
                    | Effect::Checkpoint { id, .. } => Some(*id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&first), ids(&replayed));
    }
}
