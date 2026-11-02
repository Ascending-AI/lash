//! A turn machine's journal replays to the machine that recorded it: a fresh
//! machine built the same way and handed the committed inputs, read back
//! from their stored form, reaches the state the live machine had when they
//! were committed, at every input. A host input the journal does not record,
//! or one it does not read back, leaves the replayed machine elsewhere.

use super::*;

fn answer(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        usage: LlmUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..LlmUsage::default()
        },
        ..LlmResponse::default()
    }
}

/// [`ToolBatchDriver`], naming the records it appends as a protocol driver
/// does: by what they answer, never by a counter outside the machine.
struct JournalDriver;

impl ProtocolDriverHandle for JournalDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        ToolBatchDriver.prepare_protocol_iteration(ctx)
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        request: Arc<LlmRequest>,
        driver_state: Option<serde_json::Value>,
        llm_response: LlmResponse,
        calls: &crate::ResponseToolCalls,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        ToolBatchDriver.handle_llm_success(
            ctx,
            request,
            driver_state,
            llm_response,
            calls,
            text_streamed,
        )
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let id = format!("summary-{}", completed[0].call_id.as_str());
        let summary = Message {
            id: id.clone(),
            role: MessageRole::User,
            parts: vec![Part::text(
                format!("{id}.p0"),
                completed.len().to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        };
        vec![
            DriverAction::AppendEvents(vec![conversation_event(summary)]),
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
        _result: Result<lash_protocol_types::ExecResponse, lash_protocol_types::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn steering(checkpoints: usize) -> Message {
    let id = format!("steering-{checkpoints}");
    Message {
        id: id.clone(),
        role: MessageRole::User,
        parts: vec![Part::text(format!("{id}.p0"), id.clone(), None)].into(),
        origin: None,
        reply_marker: None,
    }
}

/// The environment the host adopts in protocol `iteration`: each differs,
/// so a replay answered with another iteration's sync reaches another state.
fn sync_for(iteration: usize) -> ExecutionEnvironmentSync {
    ExecutionEnvironmentSync {
        truncated_names: [(format!("tool_{iteration}"), format!("tools.{iteration}"))].into(),
        ..ExecutionEnvironmentSync::default()
    }
}

fn fresh() -> TurnMachine {
    TurnMachine::new(
        test_config(Arc::new(JournalDriver)),
        vec![user_message("hello")],
        lash_protocol_types::AppendVec::new(),
        0,
    )
}

fn state(machine: &TurnMachine) -> serde_json::Value {
    let saved = machine.checkpoint();
    serde_json::json!({
        "checkpoint": serde_json::to_value(&saved.checkpoint).expect("checkpoint"),
        "content": serde_json::to_value(&saved.content).expect("content"),
    })
}

/// Drive a turn through every host input kind: a sync, an admitted model
/// call answered, a tool round, a checkpoint that delivers steering input,
/// usage the host settles, a second call, and a host-decided finish. After
/// every host action the state and the journal's length are taken.
fn live_run() -> (Vec<MachineInput<()>>, Vec<(usize, serde_json::Value)>) {
    let mut machine = fresh();
    machine.journal_inputs(0);
    let mut cuts = Vec::new();
    let mut calls = 0;
    let mut checkpoints = 0;
    let cut = |machine: &mut TurnMachine, cuts: &mut Vec<(usize, serde_json::Value)>| {
        let (_, mark) = machine.stage_inputs();
        cuts.push((mark, state(machine)));
    };
    'turn: loop {
        let Some(effect) = machine.poll_effect() else {
            break;
        };
        cut(&mut machine, &mut cuts);
        match effect {
            Effect::SyncExecutionEnvironment { id } => {
                let sync = sync_for(machine.protocol_iteration());
                machine.handle_response(Response::ExecutionEnvironmentSynced {
                    id,
                    result: Ok(sync),
                });
            }
            Effect::LlmCall { id, request } => {
                calls += 1;
                let mut admitted = (*request).clone();
                admitted.edit_envelope(|request| {
                    request.instructions = Some(Arc::from(format!("composed prompt {calls}")));
                });
                assert!(machine.admit_request(id, Arc::new(admitted)));
                cut(&mut machine, &mut cuts);
                if calls == 2 {
                    machine
                        .record_unanswered_call_usage(&LlmUsage {
                            input_tokens: 3,
                            ..LlmUsage::default()
                        })
                        .expect("usage");
                    cut(&mut machine, &mut cuts);
                    machine.finish_with_outcome(assistant_done("host finished"));
                    cut(&mut machine, &mut cuts);
                    continue 'turn;
                }
                machine
                    .try_handle_response(Response::LlmComplete {
                        id,
                        result: Ok(answer("calling tools")),
                        text_streamed: false,
                    })
                    .expect("usage");
            }
            Effect::ToolCalls { id, calls, .. } => {
                let results = calls
                    .iter()
                    .map(|call| CompletedToolCall {
                        call_id: call.call_id.clone(),
                        ..completed_tool(
                            "fixture",
                            &call.tool_name,
                            call.args.clone(),
                            ToolCallOutput::success(serde_json::json!({"ok": call.tool_name})),
                        )
                    })
                    .collect();
                machine.handle_response(Response::ToolResults { id, results });
            }
            Effect::Checkpoint { id, .. } => {
                checkpoints += 1;
                machine.handle_response(Response::Checkpoint {
                    id,
                    delivery: CheckpointDelivery {
                        committed_user_messages: vec![steering(checkpoints)],
                    },
                });
            }
            Effect::Done { .. } => break,
            Effect::Emit(_) | Effect::Progress { .. } | Effect::Log { .. } => {}
            other => panic!("the fixture turn does not issue {other:?}"),
        }
        cut(&mut machine, &mut cuts);
    }
    assert_eq!(calls, 2, "the fixture turn reaches its second call");
    let (row, _) = machine.stage_inputs();
    let stored = serde_json::to_string(&row).expect("the row encodes");
    let read: JournalRow<()> = serde_json::from_str(&stored).expect("the row reads back");
    assert_eq!(read.from, 0, "the uncommitted row starts the journal");
    (read.inputs, cuts)
}

/// A response held for a later commit reaches the store in the same commit
/// as the row staged after it, which holds its inputs again: the journal
/// holds each input once, and a row past the inputs before it is refused.
#[test]
fn overlapping_journal_rows_assemble_to_each_input_once() {
    let (inputs, _) = live_run();
    let half = inputs.len() / 2;
    let rows = vec![
        JournalRow {
            from: 0,
            inputs: inputs[..half].to_vec(),
        },
        JournalRow {
            from: half / 2,
            inputs: inputs[half / 2..].to_vec(),
        },
    ];
    let assembled = JournalRow::assemble(rows).expect("overlapping rows assemble");
    assert_eq!(
        serde_json::to_value(&assembled).unwrap(),
        serde_json::to_value(&inputs).unwrap(),
        "each input once, in order"
    );
    let gap = vec![
        JournalRow {
            from: 0,
            inputs: inputs[..half].to_vec(),
        },
        JournalRow {
            from: half + 1,
            inputs: inputs[half + 1..].to_vec(),
        },
    ];
    let refused = JournalRow::assemble(gap).expect_err("a gap is refused");
    assert_eq!(refused.index, half, "the refusal names the missing input");
}

#[test]
fn a_replayed_journal_reaches_the_recorded_state_at_every_input() {
    let (inputs, cuts) = live_run();
    let stored = serde_json::to_vec(&inputs).expect("the journal encodes");
    let read: Vec<MachineInput<()>> = serde_json::from_slice(&stored).expect("it reads back");
    for (mark, expected) in cuts {
        let mut replayed = fresh();
        replayed
            .replay_inputs(read[..mark].iter().cloned(), &|iteration| {
                Some(sync_for(iteration))
            })
            .unwrap_or_else(|divergence| panic!("{mark} inputs replay: {divergence}"));
        assert_eq!(
            state(&replayed),
            expected,
            "the machine replayed through {mark} inputs"
        );
        assert_eq!(replayed.journaled_inputs(), mark);
    }
}

#[test]
fn a_journal_another_machine_recorded_is_refused_where_it_diverges() {
    let (inputs, _) = live_run();
    let mut other = TurnMachine::new(
        test_config(Arc::new(ProseDriver)),
        vec![user_message("hello")],
        lash_protocol_types::AppendVec::new(),
        0,
    );
    let divergence = other
        .replay_inputs(inputs, &|iteration| Some(sync_for(iteration)))
        .expect_err("a prose turn waits on no tool round");
    assert!(divergence.reason.contains("answers effect"), "{divergence}");
}
