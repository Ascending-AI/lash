use super::*;

pub(super) fn standard_protocol_contract_executions() -> Result<Vec<Value>, FixedScriptRunnerError>
{
    Ok(vec![
        standard_initial_request_projection_execution()?,
        standard_empty_provider_response_error_execution()?,
        standard_provider_error_without_checkpoint_execution()?,
        standard_native_tool_loop_reenters_model_execution()?,
        standard_parallel_tool_results_checkpoint_once_execution()?,
        standard_tool_failure_feedback_reenters_model_execution()?,
        standard_streamed_text_finalizes_once_execution()?,
        standard_max_turn_after_tool_result_execution()?,
    ])
}

fn standard_initial_request_projection_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard initial request projection",
        "hello standard protocol",
        None,
        vec![],
    )?;
    contract_execution_payload(
        "standard.initial_request_projection",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_projects_initial_request",
        result,
    )
}

fn standard_empty_provider_response_error_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard empty provider response error",
        "answer with something",
        None,
        vec![StandardContractStep::Llm {
            text_streamed: false,
            parts: vec![],
        }],
    )?;
    contract_execution_payload(
        "standard.empty_provider_response_error",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_empty_model_response_stops_provider_error",
        result,
    )
}

fn standard_provider_error_without_checkpoint_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard provider error without checkpoint",
        "trigger provider failure",
        None,
        vec![StandardContractStep::LlmError(
            "upstream provider unavailable",
        )],
    )?;
    contract_execution_payload(
        "standard.provider_error_without_checkpoint",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_provider_error_stops_without_checkpoint",
        result,
    )
}

fn standard_native_tool_loop_reenters_model_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard native tool loop reenters model",
        "read file",
        None,
        vec![
            StandardContractStep::Llm {
                text_streamed: false,
                parts: vec![
                    standard_text_part("Let me read that."),
                    standard_tool_call_part("tc1", "read_file", r#"{"path":"foo.txt"}"#),
                ],
            },
            StandardContractStep::ToolResults(vec![StandardContractToolResult::ok(
                "tc1",
                "read_file",
                json!("file contents"),
                "file contents",
            )]),
            StandardContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "standard.native_tool_loop_reenters_model",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint",
        result,
    )
}

fn standard_parallel_tool_results_checkpoint_once_execution()
-> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard parallel tool results checkpoint once",
        "read two files",
        None,
        vec![
            StandardContractStep::Llm {
                text_streamed: false,
                parts: vec![
                    standard_tool_call_part("tc1", "read_file", r#"{"path":"left.txt"}"#),
                    standard_tool_call_part("tc2", "read_file", r#"{"path":"right.txt"}"#),
                ],
            },
            StandardContractStep::ToolResults(vec![
                StandardContractToolResult::ok(
                    "tc1",
                    "read_file",
                    json!("left contents"),
                    "left contents",
                ),
                StandardContractToolResult::ok(
                    "tc2",
                    "read_file",
                    json!("right contents"),
                    "right contents",
                ),
            ]),
            StandardContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "standard.parallel_tool_results_checkpoint_once",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_parallel_tool_results_checkpoint_once",
        result,
    )
}

fn standard_tool_failure_feedback_reenters_model_execution() -> Result<Value, FixedScriptRunnerError>
{
    let result = run_standard_protocol_contract(
        "standard tool failure feedback reenters model",
        "search docs",
        None,
        vec![
            StandardContractStep::Llm {
                text_streamed: false,
                parts: vec![standard_tool_call_part(
                    "tc1",
                    "search",
                    r#"{"query":"missing term"}"#,
                )],
            },
            StandardContractStep::ToolResults(vec![StandardContractToolResult::failure(
                "tc1",
                "search",
                "search_failed",
                "index unavailable",
                "search failed: index unavailable",
            )]),
            StandardContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "standard.tool_failure_feedback_reenters_model",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_tool_failure_feedback_reenters_model_after_checkpoint",
        result,
    )
}

fn standard_streamed_text_finalizes_once_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard streamed text finalizes once",
        "answer directly",
        None,
        vec![
            StandardContractStep::Llm {
                text_streamed: true,
                parts: vec![standard_text_part("streamed done")],
            },
            StandardContractStep::Checkpoint,
        ],
    )?;
    contract_execution_payload(
        "standard.streamed_text_finalizes_once",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_streamed_text_finishes_without_duplicate_delta",
        result,
    )
}

fn standard_max_turn_after_tool_result_execution() -> Result<Value, FixedScriptRunnerError> {
    let result = run_standard_protocol_contract(
        "standard max turns after tool result",
        "use a tool once",
        Some(1),
        vec![
            StandardContractStep::Llm {
                text_streamed: false,
                parts: vec![standard_tool_call_part("tc1", "test", "{}")],
            },
            StandardContractStep::ToolResults(vec![StandardContractToolResult::ok(
                "tc1",
                "test",
                json!("ok"),
                "ok",
            )]),
        ],
    )?;
    contract_execution_payload(
        "standard.max_turns_after_tool_result",
        "crates/lash-protocol-standard/tests/protocol_scenarios.rs",
        "standard_protocol_scenario_max_turns_terminates_after_tool_result",
        result,
    )
}

#[derive(Clone)]
pub(super) enum StandardContractStep {
    Llm {
        text_streamed: bool,
        parts: Vec<LlmOutputPart>,
    },
    LlmError(&'static str),
    ToolResults(Vec<StandardContractToolResult>),
    Checkpoint,
}

#[derive(Clone)]
pub(super) struct StandardContractToolResult {
    call_id: &'static str,
    tool_name: &'static str,
    output: lash_core::ToolCallOutput,
    model_return_text: &'static str,
    status: &'static str,
    error_code: Option<&'static str>,
}

impl StandardContractToolResult {
    fn ok(
        call_id: &'static str,
        tool_name: &'static str,
        output: Value,
        model_return_text: &'static str,
    ) -> Self {
        Self {
            call_id,
            tool_name,
            output: lash_core::ToolCallOutput::success(output),
            model_return_text,
            status: "success",
            error_code: None,
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
            output: lash_core::ToolCallOutput::failure(lash_core::ToolFailure::tool(
                lash_core::ToolFailureClass::Execution,
                code,
                message,
            )),
            model_return_text,
            status: "failure",
            error_code: Some(code),
        }
    }

    fn completed_call(&self, args: Value) -> lash_core::sansio::CompletedToolCall {
        lash_core::sansio::CompletedToolCall {
            call_id: self.call_id.to_string(),
            tool_name: self.tool_name.to_string(),
            args,
            output: self.output.clone(),
            model_return: lash_core::facade_support::ModelToolReturn {
                call_id: self.call_id.to_string(),
                tool_name: self.tool_name.to_string(),
                parts: vec![lash_core::facade_support::ModelToolReturnPart::text(
                    self.model_return_text,
                )],
            },
            duration_ms: 1,
            replay: None,
        }
    }

    fn summary(&self) -> Value {
        json!({
            "call_id": self.call_id,
            "tool_name": self.tool_name,
            "status": self.status,
            "error_code": self.error_code,
            "model_return_text": self.model_return_text,
        })
    }
}

#[derive(Default)]
struct StandardContractObserved {
    initial_request_text: Option<String>,
    tool_calls: Vec<Value>,
    tool_results: Vec<Value>,
    checkpoints: Vec<&'static str>,
    llm_response_full_texts: Vec<String>,
    llm_response_parts: Vec<Vec<Value>>,
    llm_call_count: usize,
    text_deltas: Vec<String>,
    errors: Vec<String>,
    turn_outcomes: Vec<lash_core::facade_support::TurnOutcome>,
}

impl StandardContractObserved {
    fn record(&mut self, effects: &[lash_core::Effect]) {
        for effect in effects {
            match effect {
                lash_core::Effect::LlmCall { request, .. } => {
                    if self.initial_request_text.is_none() {
                        self.initial_request_text = Some(format!("{:?}", request.messages));
                    }
                    self.llm_call_count += 1;
                }
                lash_core::Effect::ToolCalls { calls, .. } => {
                    self.tool_calls.extend(calls.iter().map(|call| {
                        json!({
                            "call_id": call.call_id,
                            "tool_name": call.tool_name,
                            "args": call.args,
                        })
                    }));
                }
                lash_core::Effect::Checkpoint { checkpoint, .. } => {
                    self.checkpoints.push(checkpoint_kind_name(*checkpoint));
                }
                lash_core::Effect::Emit(
                    lash_core::facade_support::SessionStreamEvent::TextDelta { content },
                ) => {
                    self.text_deltas.push(content.clone());
                }
                lash_core::Effect::Emit(lash_core::facade_support::SessionStreamEvent::Error {
                    message,
                    ..
                }) => {
                    self.errors.push(message.clone());
                }
                lash_core::Effect::Emit(
                    lash_core::facade_support::SessionStreamEvent::TurnOutcome { outcome },
                ) => {
                    self.turn_outcomes.push(outcome.clone());
                }
                _ => {}
            }
        }
    }
}

pub(super) fn run_standard_protocol_contract(
    scenario_name: &'static str,
    user_message: &'static str,
    max_turns: Option<usize>,
    steps: Vec<StandardContractStep>,
) -> Result<Value, FixedScriptRunnerError> {
    let mut config = standard_contract_turn_machine_config();
    config.max_turns = max_turns;
    let mut machine = lash_core::TurnMachine::new(
        config,
        vec![contract_user_message(user_message)],
        Arc::new(Vec::new()),
        0,
    );
    let mut observed = StandardContractObserved::default();
    let mut effects = drain_contract_turn_machine_effects(&mut machine);
    observed.record(&effects);

    for step in steps {
        match step {
            StandardContractStep::Llm {
                text_streamed,
                parts,
            } => {
                let llm_id = *find_contract_llm_call(&effects).ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "{scenario_name} expected a pending LLM call"
                    ))
                })?;
                let expected_parts = parts.clone();
                let expected_full_text = standard_full_text(&expected_parts);
                let expected_part_summary = llm_output_parts_contract_summary(&expected_parts);
                let response = llm_response_with_parts(expected_full_text.clone(), parts);
                require(
                    response.full_text == expected_full_text,
                    format!(
                        "{scenario_name} provider response full_text changed: expected {:?}, got {:?}",
                        expected_full_text, response.full_text
                    ),
                )?;
                let response_part_summary = llm_output_parts_contract_summary(&response.parts);
                require(
                    response_part_summary == expected_part_summary,
                    format!(
                        "{scenario_name} provider response parts changed: expected {:?}, got {:?}",
                        expected_part_summary, response_part_summary
                    ),
                )?;
                observed
                    .llm_response_full_texts
                    .push(response.full_text.clone());
                observed.llm_response_parts.push(response_part_summary);
                machine.handle_response(lash_core::sansio::Response::LlmComplete {
                    id: llm_id,
                    text_streamed,
                    result: Ok(response),
                });
            }
            StandardContractStep::LlmError(message) => {
                let llm_id = *find_contract_llm_call(&effects).ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "{scenario_name} expected a pending LLM call before provider error"
                    ))
                })?;
                machine.handle_response(lash_core::sansio::Response::LlmComplete {
                    id: llm_id,
                    text_streamed: false,
                    result: Err(standard_llm_error(message)),
                });
            }
            StandardContractStep::ToolResults(results) => {
                let (tool_id, calls) = effects
                    .iter()
                    .find_map(|effect| match effect {
                        lash_core::Effect::ToolCalls { id, calls } => Some((*id, calls.clone())),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "{scenario_name} expected pending native tool calls"
                        ))
                    })?;
                require(
                    calls.len() == results.len()
                        && calls.iter().zip(&results).all(|(call, result)| {
                            call.call_id == result.call_id && call.tool_name == result.tool_name
                        }),
                    format!("{scenario_name} native tool-call shape changed"),
                )?;
                observed
                    .tool_results
                    .extend(results.iter().map(StandardContractToolResult::summary));
                machine.handle_response(lash_core::sansio::Response::ToolResults {
                    id: tool_id,
                    results: calls
                        .iter()
                        .zip(results)
                        .map(|(call, result)| result.completed_call(call.args.clone()))
                        .collect(),
                });
            }
            StandardContractStep::Checkpoint => {
                let checkpoint_id = effects
                    .iter()
                    .find_map(|effect| match effect {
                        lash_core::Effect::Checkpoint { id, .. } => Some(*id),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "{scenario_name} expected pending checkpoint"
                        ))
                    })?;
                machine.handle_response(lash_core::sansio::Response::Checkpoint {
                    id: checkpoint_id,
                    delivery: lash_core::sansio::CheckpointDelivery::default(),
                });
            }
        }
        effects = drain_contract_turn_machine_effects(&mut machine);
        observed.record(&effects);
    }

    Ok(json!({
        "execution_api": "lash_core::sansio::TurnMachine",
        "driver": "lash_protocol_standard::StandardDriver",
        "scenario_name": scenario_name,
        "user_message": user_message,
        "max_turns": max_turns,
        "initial_request_contains_user_message": observed
            .initial_request_text
            .as_deref()
            .is_some_and(|request| request.contains(user_message)),
        "llm_call_count": observed.llm_call_count,
        "llm_response_full_texts": observed.llm_response_full_texts,
        "llm_response_parts": observed.llm_response_parts,
        "done": machine.is_done(),
        "tool_calls": observed.tool_calls,
        "tool_results": observed.tool_results,
        "checkpoints": observed.checkpoints,
        "text_delta_count": observed.text_deltas.len(),
        "text_deltas": observed.text_deltas,
        "errors": observed.errors,
        "turn_outcomes": observed.turn_outcomes.iter().map(turn_outcome_contract_json).collect::<Vec<_>>(),
    }))
}

pub(super) fn standard_text_part(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}

fn standard_tool_call_part(call_id: &str, tool_name: &str, input_json: &str) -> LlmOutputPart {
    LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        input_json: input_json.to_string(),
        replay: None,
    }
}

fn standard_full_text(parts: &[LlmOutputPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn llm_response_with_parts(full_text: String, parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        full_text,
        parts,
        response_metadata: Default::default(),
        ..Default::default()
    }
}

pub(super) fn text_llm_response(text: impl Into<String>) -> LlmResponse {
    let text = text.into();
    llm_response_with_parts(
        text.clone(),
        vec![LlmOutputPart::Text {
            text,
            response_meta: None,
        }],
    )
}

pub(super) fn tool_call_llm_response(
    call_id: &str,
    tool_name: &str,
    input_json: &str,
) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            input_json: input_json.to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..Default::default()
    }
}

pub(super) fn llm_output_parts_contract_summary(parts: &[LlmOutputPart]) -> Vec<Value> {
    parts
        .iter()
        .map(|part| match part {
            LlmOutputPart::Text { text, .. } => json!({
                "kind": "text",
                "text": text,
            }),
            LlmOutputPart::Reasoning { text, .. } => json!({
                "kind": "reasoning",
                "text": text,
            }),
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => json!({
                "kind": "tool_call",
                "call_id": call_id,
                "tool_name": tool_name,
                "input_json": input_json,
            }),
        })
        .collect()
}

pub(super) fn response_text_part(response: &LlmResponse) -> Option<&str> {
    response.parts.iter().find_map(|part| match part {
        LlmOutputPart::Text { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn standard_llm_error(message: &str) -> lash_core::LlmCallError {
    lash_core::LlmCallError {
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
