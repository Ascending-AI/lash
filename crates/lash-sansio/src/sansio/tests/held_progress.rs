//! A progress boundary is resume-safe: while a tool call is unanswered the
//! machine holds its progress cursor, and the records appended meanwhile
//! ride the next boundary's delta (FIG-5588).

use super::*;

const CALL: &str = "call-note";
const TOOL: &str = "note_tool";
const NOTE: &str = "mid-call-note";

/// The fixture machine's protocol records carry no payload: the one this
/// driver appends is the turn's only one.
fn note() -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(())
}

fn holds_note(records: &[SessionHistoryRecord]) -> bool {
    records
        .iter()
        .any(|record| matches!(record, SessionHistoryRecord::Protocol(())))
}

/// Commits the model's call beside a protocol record, then waits on the
/// call; its result joins the transcript when the round answers.
struct MidCallNoteDriver;

impl ProtocolDriverHandle for MidCallNoteDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::Start(PendingWork::Llm {
            request: ctx
                .project_llm_request(true)
                .expect("fixture history projects"),
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _driver_state: Option<serde_json::Value>,
        _llm_response: LlmResponse,
        _calls: &crate::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let call_id = crate::ToolCallId::fixture(CALL);
        vec![
            DriverAction::AppendEvents(vec![
                conversation_event(Message {
                    id: "call".to_string(),
                    role: MessageRole::Assistant,
                    parts: vec![Part::tool_call(
                        "call.p0".to_string(),
                        "{}".to_string(),
                        call_id.clone(),
                        "provider-call".to_string(),
                        TOOL.to_string(),
                        None,
                    )]
                    .into(),
                    origin: None,
                    reply_marker: None,
                }),
                note(),
            ]),
            DriverAction::Start(PendingWork::WaitingForToolResults {
                settled: None,
                calls: vec![PendingToolCall {
                    call_id,
                    provider_call_id: None,
                    tool_name: TOOL.to_string(),
                    args: serde_json::json!({}),
                    replay: None,
                }],
                expansion: ToolExpansionPlan::default(),
            }),
        ]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let parts = completed
            .into_iter()
            .map(|call| {
                Part::tool_result(
                    "result.p0".to_string(),
                    vec![ModelToolReturnPart::text("ok")],
                    call.call_id,
                    call.tool_name,
                )
            })
            .collect::<Vec<_>>();
        vec![
            DriverAction::AppendEvents(vec![conversation_event(Message {
                id: "result".to_string(),
                role: MessageRole::User,
                parts: parts.into(),
                origin: None,
                reply_marker: None,
            })]),
            DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: CheckpointKind::AfterWork,
                on_empty: CheckpointResumeAction::PrepareIteration,
            }),
        ]
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: serde_json::Value,
        _result: Result<crate::ExecResponse, crate::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn progress_boundaries(effects: &[Effect]) -> Vec<(&MessageSequence, &[SessionHistoryRecord])> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Progress {
                messages,
                event_delta,
                ..
            } => Some((messages, event_delta.as_slice())),
            _ => None,
        })
        .collect()
}

/// A protocol record appended between a tool call and its result reaches no
/// boundary while the call is unanswered, crosses a checkpoint taken
/// meanwhile undelivered, and rides the first resume-safe boundary's delta
/// in the order the machine appended it: after the call, before the result.
#[test]
fn a_record_appended_while_a_call_is_unanswered_rides_the_next_resume_safe_boundary() {
    let mut machine = TurnMachine::new(
        test_config(Arc::new(MidCallNoteDriver)),
        vec![user_message("use the tool")],
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
    let effects = drain_effects(&mut machine);
    let tool_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ToolCalls { id, .. } => Some(*id),
            _ => None,
        })
        .expect("the tool round");
    for (messages, _) in progress_boundaries(&effects) {
        assert!(
            crate::messages_are_prompt_resume_safe(messages.iter()),
            "a progress boundary carries an unanswered call"
        );
    }

    // The redrive: the owner that resumes the round's checkpoint holds the
    // record as one no boundary has delivered.
    let mut restored = TurnMachine::restore_from_checkpoint(
        test_config(Arc::new(MidCallNoteDriver)),
        roundtrip_checkpoint(machine.checkpoint()),
        None,
    )
    .expect("the round's checkpoint restores");
    assert!(
        restored.progressed_boundaries().next().is_none(),
        "the record counts as delivered before any boundary carried it"
    );
    restored.handle_response(Response::ToolResults {
        id: tool_id,
        results: vec![completed_tool(
            CALL,
            TOOL,
            serde_json::json!({}),
            ToolCallOutput::success(serde_json::json!("ok")),
        )],
    });
    let effects = drain_effects(&mut restored);
    let boundaries = progress_boundaries(&effects);
    let (messages, delta) = boundaries.first().expect("the answered round's boundary");
    assert!(crate::messages_are_prompt_resume_safe(messages.iter()));
    let delivered = delta
        .iter()
        .map(|record| match record {
            SessionHistoryRecord::Conversation(record) => record.to_message().id,
            SessionHistoryRecord::Protocol(()) => NOTE.to_string(),
        })
        .collect::<Vec<_>>();
    assert_eq!(delivered, ["call", NOTE, "result"]);
    let progressed = restored.progressed_boundaries().collect::<Vec<_>>();
    let [(progressed_messages, progressed_delta)] = progressed.as_slice() else {
        panic!("the machine lists the one boundary that delivered the record");
    };
    assert_eq!(progressed_messages.len(), messages.len());
    assert!(holds_note(progressed_delta));
}
