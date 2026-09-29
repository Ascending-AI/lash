//! The machine folds an expanded step's slots before anything is appended or
//! emitted (ADR 0116 §2.1): only folded calls reach the stream events, the
//! transcript and the driver's `handle_tool_results`.

use super::*;

/// Starts one step of three slots, the last two expanded from one wrapper,
/// and folds each wrapper's slots into one call named after it.
struct ExpandingDriver {
    handed_back: std::sync::Mutex<Vec<crate::ToolCallId>>,
}

fn tc(label: &str) -> crate::ToolCallId {
    crate::ToolCallId::fixture(label)
}

fn slot_call(call_id: crate::ToolCallId, tool_name: &str) -> PendingToolCall {
    PendingToolCall {
        call_id,
        provider_call_id: None,
        tool_name: tool_name.to_string(),
        args: serde_json::json!({}),
        replay: None,
    }
}

fn plan() -> ToolExpansionPlan {
    ToolExpansionPlan {
        wrappers: vec![ExpandedWrapper {
            source_position: 1,
            call_id: tc("wrapper"),
            provider_call_id: Some("provider-wrapper-call".to_string()),
            tool_name: "batch".to_string(),
            args: serde_json::json!({"tool_calls": []}),
            replay: Some(ProviderReplayMeta {
                item_id: Some("provider-wrapper".to_string()),
                ..ProviderReplayMeta::default()
            }),
            rows: vec![
                ExpandedRow::Slot {
                    member_index: 0,
                    tool: "read".to_string(),
                    slot: 1,
                },
                ExpandedRow::Refused {
                    member_index: 1,
                    tool: "batch".to_string(),
                    error: serde_json::json!("nested"),
                },
                ExpandedRow::Slot {
                    member_index: 2,
                    tool: "search".to_string(),
                    slot: 2,
                },
            ],
        }],
    }
}

impl ProtocolDriverHandle for ExpandingDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::Start(PendingWork::Llm {
            request: ctx.project_llm_request(true),
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _driver_state: Option<serde_json::Value>,
        _llm_response: LlmResponse,
        _calls: &ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Start(PendingWork::Tools {
            calls: vec![
                slot_call(tc("native"), "list"),
                slot_call(tc("wrapper").child(0), "read"),
                slot_call(tc("wrapper").child(2), "search"),
            ],
            expansion: plan(),
        })]
    }

    fn fold_tool_results(
        &self,
        plan: &ToolExpansionPlan,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<CompletedToolCall> {
        let wrapper = &plan.wrappers[0];
        let mut completed = completed.into_iter();
        let native = completed.next().expect("the native slot");
        let members = completed
            .map(|member| member.call_id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        vec![
            native,
            completion(
                &wrapper.call_id,
                &wrapper.tool_name,
                serde_json::json!({ "members": members }),
                wrapper.replay.clone(),
            ),
        ]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        *self.handed_back.lock().expect("test mutex") =
            completed.iter().map(|call| call.call_id.clone()).collect();
        vec![DriverAction::Finish(assistant_done("folded"))]
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: serde_json::Value,
        _result: Result<crate::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn completion(
    call_id: &crate::ToolCallId,
    tool_name: &str,
    value: serde_json::Value,
    replay: Option<ProviderReplayMeta>,
) -> CompletedToolCall {
    let output = ToolCallOutput::success(value);
    CompletedToolCall {
        call_id: call_id.clone(),
        provider_call_id: None,
        tool_name: tool_name.to_string(),
        args: serde_json::json!({}),
        model_return: crate::ModelToolReturn::from_output(tool_name.to_string(), &output),
        output,
        intent_outcomes: Vec::new(),
        replay,
    }
}

fn tool_calls_effect(effects: &[Effect]) -> (EffectId, Vec<PendingToolCall>, ToolExpansionPlan) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ToolCalls {
                id,
                calls,
                expansion,
            } => Some((*id, calls.clone(), expansion.clone())),
            _ => None,
        })
        .expect("the step's tool work")
}

#[test]
fn the_machine_folds_expanded_slots_before_emitting_or_handing_back() {
    let driver = Arc::new(ExpandingDriver {
        handed_back: std::sync::Mutex::new(Vec::new()),
    });
    let mut machine = TurnMachine::new(
        test_config(Arc::clone(&driver) as Arc<dyn ProtocolDriverHandle>),
        vec![user_message("use tools")],
        crate::AppendVec::new(),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let (id, calls, expansion) = tool_calls_effect(&drain_effects(&mut machine));
    assert_eq!(
        expansion,
        plan(),
        "the host sees the plan the driver recorded"
    );
    let results = calls
        .iter()
        .map(|call| {
            completion(
                &call.call_id,
                &call.tool_name,
                serde_json::json!("ok"),
                None,
            )
        })
        .collect();
    machine.handle_response(Response::ToolResults { id, results });

    let effects = drain_effects(&mut machine);
    let reported = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Emit(SessionStreamEvent::ToolCall { call_id, .. }) => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reported,
        vec![tc("native"), tc("wrapper")],
        "only folded calls reach the stream; no member call is reported"
    );
    assert_eq!(
        *driver.handed_back.lock().expect("test mutex"),
        vec![tc("native"), tc("wrapper")],
        "the driver's handle_tool_results sees the folded calls"
    );
}

#[test]
fn a_checkpoint_restores_the_step_with_its_expansion() {
    let driver: Arc<dyn ProtocolDriverHandle> = Arc::new(ExpandingDriver {
        handed_back: std::sync::Mutex::new(Vec::new()),
    });
    let mut machine = TurnMachine::new(
        test_config(Arc::clone(&driver)),
        vec![user_message("use tools")],
        crate::AppendVec::new(),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let (id, calls, expansion) = tool_calls_effect(&drain_effects(&mut machine));

    let checkpoint = roundtrip_checkpoint(machine.checkpoint());
    let mut restored = TurnMachine::restore_from_checkpoint(test_config(driver), checkpoint)
        .expect("supported checkpoint");
    let (restored_id, restored_calls, restored_expansion) =
        tool_calls_effect(&drain_effects(&mut restored));
    assert_eq!(restored_id, id);
    assert_eq!(restored_calls.len(), calls.len());
    assert_eq!(restored_expansion, expansion);
}
