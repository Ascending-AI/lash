use super::*;
use crate::SessionCommitStore as _;

#[tokio::test]
async fn durable_turn_commit_rejects_token_usage_overflow() {
    let overflowing_call = || MockCall {
        stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
            input_tokens: i64::MAX,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        })],
        response: Ok(LlmResponse {
            full_text: "accounted".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "accounted".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let transport = mock_provider(vec![overflowing_call(), overflowing_call()]);
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    runtime.state.token_ledger.push(crate::TokenLedgerEntry {
        source: "turn".to_string(),
        model: "mock-model".to_string(),
        usage: crate::TokenUsage {
            input_tokens: 1,
            ..crate::TokenUsage::default()
        },
    });

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("account this turn"),
            CancellationToken::new(),
            named_turn_scope("root", "usage-overflow"),
        )
        .await
        .expect_err("overflow must reject the durable commit");

    assert_eq!(error.code, crate::RuntimeErrorCode::StoreCommitFailed);
    assert_eq!(
        error.message,
        "token usage counter `input_tokens` overflowed while accumulating (turn, mock-model)"
    );
    assert_eq!(*store.runtime_commit_count.lock().expect("commit count"), 0);

    let next_error = runtime
        .run_turn_assembled(
            TurnInput::text("the poisoned ledger must fail closed again"),
            CancellationToken::new(),
            named_turn_scope("root", "usage-overflow-next-turn"),
        )
        .await
        .expect_err("the unconfirmed overflowing row must poison the next turn");
    assert_eq!(next_error.code, crate::RuntimeErrorCode::StoreCommitFailed);
    assert_eq!(next_error.message, error.message);
    assert_eq!(*store.runtime_commit_count.lock().expect("commit count"), 0);
}

#[tokio::test]
async fn multi_call_turn_rejects_cumulative_usage_overflow_before_commit() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
                input_tokens: i64::MAX - 1,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            })],
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "overflow-tool-call".to_string(),
                    tool_name: "echo_tool".to_string(),
                    input_json: serde_json::json!({"value": "continue"}).to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 2,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            })],
            response: Ok(LlmResponse {
                full_text: "must not commit".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "must not commit".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("use the tool, then answer"),
            CancellationToken::new(),
            named_turn_scope("root", "multi-call-usage-overflow"),
        )
        .await
        .expect_err("the second LLM usage event must reject cumulative overflow");

    assert_eq!(error.code, crate::RuntimeErrorCode::StoreCommitFailed);
    assert_eq!(
        error.message,
        "token usage counter `input_tokens` overflowed while accumulating (turn, mock-model)"
    );
    assert_eq!(*store.runtime_commit_count.lock().expect("commit count"), 0);
}

// The in-memory `RecordingStore` stands in for the real store across these
// runtime tests; the conformance suite holds it to the same durability
// contract as the durable backend so it can't silently drift.
#[tokio::test]
async fn recording_store_satisfies_runtime_persistence_conformance() {
    let clock = Arc::new(crate::testing::TestClock::new(10_000));
    let store_clock = Arc::clone(&clock);
    crate::testing::conformance::runtime_persistence(
        move || {
            std::sync::Arc::new(RecordingStore::with_clock(store_clock.clone()))
                as std::sync::Arc<dyn crate::RuntimePersistence>
        },
        crate::testing::conformance::RuntimePersistenceLeaseTiming::controlled({
            let clock = Arc::clone(&clock);
            move |duration_ms| clock.advance(duration_ms)
        }),
    )
    .await;
}

#[tokio::test]
async fn in_memory_append_receipt_replays_after_ancestor_superseded() {
    let store = Arc::new(RecordingStore::default());
    let mutation_store = Arc::clone(&store);
    crate::testing::conformance::append_request_receipt_replays_after_ancestor_superseded(
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        move |leaf_node_id| async move {
            mutation_store.force_active_leaf_for_testing(leaf_node_id);
        },
    )
    .await;
}

#[tokio::test]
async fn in_memory_append_receipt_restores_mixed_usage_envelope() {
    crate::testing::conformance::append_receipt_mixed_usage_envelope(Arc::new(
        RecordingStore::default(),
    ))
    .await;
}

#[tokio::test]
async fn in_memory_append_receipt_rolls_back_failure_after_first_mutation() {
    let store = Arc::new(RecordingStore::default());
    let mut state = RuntimeSessionState::default();
    let nodes = vec![crate::SessionAppendNode::plugin(
        "in-memory-post-mutation-atomicity",
        serde_json::json!({"value": 1}),
    )];
    let operation = crate::runtime::state::boundary_operation(
        "root",
        "in-memory-post-mutation-atomicity",
        "append-session-nodes",
    );
    let stamp =
        crate::RuntimeTurnCommitStamp::append_session_nodes(operation.clone(), None, &nodes)
            .expect("append stamp");
    let draft_namespace = operation.storage_key().expect("operation key");
    crate::runtime::state::append_session_nodes_to_state_with_clock(
        &mut state,
        &nodes,
        &draft_namespace,
        &crate::SystemClock,
    );
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids("root", &operation)
        .expect("derive append ids");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        graph,
        &[],
        operation,
    )
    .expect("append commit");
    commit.turn_commit = stamp;
    store.fail_next_runtime_commit_after_first_mutation(crate::StoreError::Backend(
        "injected failure after first in-memory mutation".to_string(),
    ));

    let error = store
        .commit_runtime_state(commit.clone())
        .await
        .expect_err("post-mutation failure must reject the append");
    assert!(matches!(
        error,
        crate::StoreError::Backend(message)
            if message == "injected failure after first in-memory mutation"
    ));
    assert!(
        store
            .load_session()
            .await
            .expect("load failed append")
            .is_none()
    );
    assert!(
        crate::SessionCommitStore::load_session_meta(store.as_ref())
            .await
            .expect("load rolled-back session meta")
            .is_none()
    );
    assert!(store.raw_runtime_turn_commits_for_testing().is_empty());

    store
        .commit_runtime_state(commit)
        .await
        .expect("retry after honest rollback succeeds");
    assert_eq!(store.raw_runtime_turn_commits_for_testing().len(), 1);
}

fn recording_runtime_persistence() -> Arc<dyn crate::RuntimePersistence> {
    Arc::new(RecordingStore::default())
}

fn controlled_recording_runtime_persistence() -> (
    Arc<dyn crate::RuntimePersistence>,
    crate::testing::conformance::RuntimePersistenceLeaseTiming,
) {
    let clock = Arc::new(crate::testing::TestClock::new(10_000));
    let store =
        Arc::new(RecordingStore::with_clock(clock.clone())) as Arc<dyn crate::RuntimePersistence>;
    let timing = crate::testing::conformance::RuntimePersistenceLeaseTiming::controlled(
        move |duration_ms| clock.advance(duration_ms),
    );
    (store, timing)
}

#[tokio::test]
async fn queued_work_claims_supersede_across_session_lease_generations() {
    let (store, timing) = controlled_recording_runtime_persistence();
    crate::testing::conformance::queued_work_claims_supersede_across_session_lease_generations(
        store, timing,
    )
    .await;
}

#[tokio::test]
async fn turn_input_claims_supersede_across_session_lease_generations() {
    let (store, timing) = controlled_recording_runtime_persistence();
    crate::testing::conformance::turn_input_claims_supersede_across_session_lease_generations(
        store, timing,
    )
    .await;
}

#[tokio::test]
async fn active_turn_input_claim_reacquires_after_unrecorded_checkpoint() {
    crate::testing::conformance::active_turn_input_claim_reacquires_after_unrecorded_checkpoint(
        recording_runtime_persistence(),
    )
    .await;
}

#[tokio::test]
async fn same_generation_claim_scans_reach_rows_beyond_the_scan_surplus() {
    crate::testing::conformance::same_generation_claim_scans_reach_rows_beyond_the_scan_surplus(
        recording_runtime_persistence(),
    )
    .await;
}

#[tokio::test]
async fn checkpoint_claim_probe_avoids_quiescent_write_transactions() {
    let store = Arc::new(RecordingStore::default());
    crate::testing::conformance::checkpoint_claim_probe_transaction_counts(
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
        "root",
        || store.checkpoint_claim_counts(),
    )
    .await;
}

#[tokio::test]
async fn standard_runtime_assembles_stream_only_text_response() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("What time ".to_string()),
            LlmStreamEvent::Delta("is it?".to_string()),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 11,
                output_tokens: 4,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            full_text: "What time is it?".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "What time is it?".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(transport).await;
    let sink = RecordingSink::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hi".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "stream-only-text-turn"),
            )
            .with_events(&sink),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(turn.assistant_output.safe_text, "What time is it?");
    let assistant_messages = active_conversation_messages(&turn.state)
        .into_iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].parts[0].content, "What time is it?");

    let streamed_text: String = sink
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            SessionStreamEvent::TextDelta { content } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "What time is it?");
}

#[tokio::test]
async fn standard_runtime_recovers_streamed_text_when_final_response_is_empty() {
    let expected =
        "I’m continuing with a type-safety cleanup now: replace the remaining raw JSON paths.";
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("I’m continuing with a type-safety cleanup now: ".to_string()),
            LlmStreamEvent::Delta("replace the remaining raw JSON paths.".to_string()),
        ],
        response: Ok(LlmResponse::default()),
    }]);
    let mut runtime = standard_runtime_with_transport(transport).await;
    let sink = RecordingSink::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "continue".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "recover-streamed-text-turn"),
            )
            .with_events(&sink),
        )
        .await
        .expect("turn");

    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. }
    ));
    assert!(matches!(
        &turn.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(turn.assistant_output.safe_text, expected);
    assert!(turn.errors.is_empty());
    let assistant_messages = active_conversation_messages(&turn.state)
        .into_iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].parts[0].content, expected);

    let streamed_text: String = sink
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            SessionStreamEvent::TextDelta { content } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, expected);
}

#[tokio::test]
async fn standard_runtime_text_part_reconciles_without_streaming_duplicate() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("The sentence.".to_string()),
            LlmStreamEvent::Part(LlmOutputPart::Text {
                text: "The sentence.".to_string(),
                response_meta: None,
            }),
        ],
        response: Ok(LlmResponse::default()),
    }]);
    let mut runtime = standard_runtime_with_transport(transport).await;
    let sink = RecordingSink::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "continue".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "text-part-no-duplicate-turn"),
            )
            .with_events(&sink),
        )
        .await
        .expect("turn");

    assert_eq!(turn.assistant_output.safe_text, "The sentence.");
    let streamed_text: String = sink
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            SessionStreamEvent::TextDelta { content } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "The sentence.");
}

#[tokio::test]
async fn standard_runtime_cancels_in_flight_tool_calls_when_token_fires() {
    // Model emits one tool call that would sleep for 10s; we cancel the turn
    // and expect run_tool_calls to tear down promptly (< 2s), either via
    // JoinSet::abort_all or via the tool observing the cancellation token.
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![
                LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                    call_id: "slow-1".to_string(),
                    tool_name: "slow_tool".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }),
                LlmStreamEvent::Usage(LlmUsage {
                    input_tokens: 10,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                }),
            ],
            response: Ok(LlmResponse::default()),
        },
        // Extra call not expected to happen — provided as a safety net in case
        // the turn machine makes a second LLM call before noticing cancel.
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "stopped".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "stopped".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let observed_cancel = Arc::new(AtomicBool::new(false));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(SlowTool {
        observed_cancel: Arc::clone(&observed_cancel),
    });
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;
    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    crate::task::spawn(async move {
        // Give the turn time to spawn the slow tool before we cancel.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel_trigger.cancel();
    });

    let start = std::time::Instant::now();
    let _ = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "trigger slow tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            cancel,
            named_turn_scope("root", "cancel-tool-turn"),
        )
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "turn cancellation did not tear down in-flight tool call quickly: elapsed={elapsed:?}"
    );
    // The tool either saw the cancellation token and returned, or its future
    // was aborted by the JoinSet. Either outcome is acceptable — what matters
    // is the prompt return above. We still assert cooperative observation as a
    // stronger signal that the token is now plumbed through to tool context.
    assert!(
        observed_cancel.load(Ordering::SeqCst),
        "slow tool did not observe cancellation token through ToolContext"
    );
}

#[tokio::test]
async fn standard_runtime_tool_control_finish_emits_terminal_output() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::ToolCall {
                        call_id: "tool-1".to_string(),
                        tool_name: "terminal_tool_0".to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    },
                    LlmOutputPart::ToolCall {
                        call_id: "tool-2".to_string(),
                        tool_name: "terminal_tool_1".to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "unexpected follow-up".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "unexpected follow-up".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(TerminalControlTool {
        controls: vec![
            crate::ToolControl::Finish {
                value: json!("first").into(),
            },
            crate::ToolControl::Finish {
                value: json!("second").into(),
            },
        ],
    });
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run terminal tools".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "terminal-tool-finish-turn"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");

    assert!(
        matches!(
        turn.outcome,
        TurnOutcome::Finished(TurnFinish::ToolValue {
            ref tool_name,
            ref value,
        }) if tool_name == "terminal_tool_0" && *value == json!("first")
        ),
        "outcome={:?} calls={:?}",
        turn.outcome,
        turn.tool_calls
    );
    assert_eq!(turn.tool_calls.len(), 2);
    let events = turn_events.snapshot();
    let first_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { name, .. } if name == "terminal_tool_0"))
        .expect("first completed");
    let second_completed = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { name, .. } if name == "terminal_tool_1"))
        .expect("second completed");
    let terminal = events
        .iter()
        .position(|event| matches!(&event.event, TurnEvent::ToolValue { .. }))
        .expect("terminal output");
    assert!(first_completed < terminal);
    assert!(second_completed < terminal);
    assert!(matches!(
        &events[terminal].event,
        TurnEvent::ToolValue {
            tool_name: name,
            value,
        } if name == "terminal_tool_0" && *value == json!("first")
    ));
}

#[tokio::test]
async fn standard_runtime_tool_control_fail_stops_without_terminal_output_event() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "tool-1".to_string(),
                    tool_name: "terminal_tool_0".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "unexpected follow-up".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "unexpected follow-up".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(TerminalControlTool {
        controls: vec![crate::ToolControl::Fail {
            failure: crate::ToolFailure::tool(
                crate::ToolFailureClass::Execution,
                "terminal_control_failed",
                "failed",
            ),
        }],
    });
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;
    let turn_events = RecordingTurnEvents::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run failing terminal tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "terminal-tool-fail-turn"),
            )
            .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");

    assert!(
        matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::ToolError {
            ref tool_name,
            ref value,
        }) if tool_name == "terminal_tool_0"
            && value["code"] == "terminal_control_failed"
            && value["message"] == "failed"
        ),
        "outcome={:?} calls={:?}",
        turn.outcome,
        turn.tool_calls
    );
    assert!(!turn_events.snapshot().iter().any(|event| matches!(
        &event.event,
        TurnEvent::FinalValue { .. } | TurnEvent::ToolValue { .. }
    )));
}

#[tokio::test]
async fn standard_runtime_executes_streamed_tool_call_when_final_response_is_empty() {
    let transport = mock_provider(vec![
        MockCall {
            stream_events: vec![
                LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                    call_id: "tool-1".to_string(),
                    tool_name: "echo_tool".to_string(),
                    input_json: r#"{"value":"sample"}"#.to_string(),
                    replay: None,
                }),
                LlmStreamEvent::Usage(LlmUsage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                }),
            ],
            response: Ok(LlmResponse::default()),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "done".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EchoTool);
    let mut runtime = runtime_with_plugins_and_tools(Vec::new(), tools, transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "streamed-tool-call-turn"),
        )
        .await
        .expect("turn");

    assert_eq!(turn.assistant_output.safe_text, "done");
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].call_id.as_deref(), Some("tool-1"));
    assert_eq!(
        turn.tool_calls[0].output.value_for_projection(),
        serde_json::json!({
            "payload": "raw:sample"
        })
    );
}

#[tokio::test]
async fn standard_runtime_preserves_part_boundaries_when_response_is_not_streamed() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![],
        response: Ok(LlmResponse {
            full_text: "Intro paragraph.\n\n## Heading".to_string(),
            parts: vec![
                LlmOutputPart::Text {
                    text: "Intro paragraph.".to_string(),
                    response_meta: None,
                },
                LlmOutputPart::Text {
                    text: "## Heading".to_string(),
                    response_meta: None,
                },
            ],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(transport).await;
    let sink = RecordingSink::default();

    let turn = runtime
        .stream_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hi".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope("root", "part-boundaries-turn"),
            )
            .with_events(&sink),
        )
        .await
        .expect("turn");

    assert_eq!(
        turn.assistant_output.safe_text,
        "Intro paragraph.\n\n## Heading"
    );

    let streamed_text: String = sink
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            SessionStreamEvent::TextDelta { content } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "Intro paragraph.\n\n## Heading");
}

#[tokio::test]
async fn standard_runtime_uses_streamed_usage_when_final_usage_missing() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("Hi".to_string()),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 9,
                output_tokens: 3,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            full_text: "Hi".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Hi".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage::default(),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "streamed-usage-turn"),
        )
        .await
        .expect("turn");

    assert_eq!(turn.token_usage.input_tokens, 9);
    assert_eq!(turn.token_usage.output_tokens, 3);
    assert_eq!(turn.token_usage.cache_read_input_tokens, 2);
}

#[tokio::test]
async fn standard_runtime_prefers_final_usage_over_streamed_usage() {
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta("Hi".to_string()),
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 9,
                output_tokens: 3,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            full_text: "Hi".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Hi".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 12,
                output_tokens: 4,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "final-usage-turn"),
        )
        .await
        .expect("turn");

    assert_eq!(turn.token_usage.input_tokens, 12);
    assert_eq!(turn.token_usage.output_tokens, 4);
    assert_eq!(turn.token_usage.cache_read_input_tokens, 1);
}
