use super::*;
use crate::store::{QueuedWorkStore, SessionCommitStore, SessionExecutionLeaseStore};

// The in-memory `RecordingStore` stands in for the real store across these
// runtime tests; the conformance suite holds it to the same durability
// contract as the durable backend so it can't silently drift.
#[tokio::test]
async fn recording_store_satisfies_runtime_persistence_conformance() {
    crate::testing::conformance::runtime_persistence(|| {
        std::sync::Arc::new(RecordingStore::default())
            as std::sync::Arc<dyn crate::RuntimePersistence>
    })
    .await;
}

fn recording_runtime_persistence() -> Arc<dyn crate::RuntimePersistence> {
    Arc::new(RecordingStore::default())
}

#[tokio::test]
async fn queued_work_claims_supersede_across_session_lease_generations() {
    crate::testing::conformance::queued_work_claims_supersede_across_session_lease_generations(
        recording_runtime_persistence(),
    )
    .await;
}

#[tokio::test]
async fn turn_input_claims_supersede_across_session_lease_generations() {
    crate::testing::conformance::turn_input_claims_supersede_across_session_lease_generations(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_memory_claim_validation_serializes_takeover_before_mutation() {
    let store = Arc::new(RecordingStore::default());
    let session_id = "atomic-generation-claim";
    let batch = store
        .enqueue_queued_work(crate::testing::conformance::queued_process_wake_draft(
            session_id,
            "atomic generation claim",
            crate::DeliveryPolicy::EarliestSafeBoundary,
            crate::SlotPolicy::Exclusive,
        ))
        .await
        .expect("enqueue atomic-generation work");
    let pid = std::process::id();
    let stale_owner = crate::LeaseOwnerIdentity {
        owner_id: "atomic-stale-owner".to_string(),
        incarnation_id: "atomic-stale-owner:incarnation".to_string(),
        liveness: crate::LeaseOwnerLiveness::local_process_for_test(
            "atomic-host",
            "atomic-boot",
            pid,
            "not-the-current-process-start",
        ),
    };
    let live_owner = crate::LeaseOwnerIdentity {
        owner_id: "atomic-live-owner".to_string(),
        incarnation_id: "atomic-live-owner:incarnation".to_string(),
        liveness: crate::LeaseOwnerLiveness::local_process_for_test(
            "atomic-host",
            "atomic-boot",
            pid,
            "atomic-live-start",
        ),
    };
    let stale_lease = store
        .try_claim_session_execution_lease(session_id, &stale_owner, 60_000)
        .await
        .expect("claim stale owner lease")
        .acquired()
        .expect("stale owner lease is free");

    let validation_entered = Arc::new(std::sync::Barrier::new(2));
    let release_validation = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&validation_entered);
    let hook_release = Arc::clone(&release_validation);
    store.set_claim_after_lease_validation_hook(Arc::new(move || {
        hook_entered.wait();
        hook_release.wait();
    }));

    let stale_claim_store = Arc::clone(&store);
    let stale_claim_owner = stale_owner.clone();
    let stale_claim_fence = stale_lease.fence();
    let stale_claim = crate::task::spawn(async move {
        stale_claim_store
            .claim_ready_queued_work(
                session_id,
                &stale_claim_fence,
                &stale_claim_owner,
                crate::QueuedWorkClaimBoundary::Idle,
                1,
            )
            .await
    });
    tokio::task::spawn_blocking(move || validation_entered.wait())
        .await
        .expect("wait for claim validation hook");

    let takeover_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let takeover_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let takeover_store = Arc::clone(&store);
    let takeover_owner = live_owner.clone();
    let observed_stale = stale_lease.fence();
    let takeover_started_thread = Arc::clone(&takeover_started);
    let takeover_completed_thread = Arc::clone(&takeover_completed);
    let takeover = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build takeover test runtime");
        takeover_started_thread.store(true, std::sync::atomic::Ordering::SeqCst);
        let outcome = runtime.block_on(takeover_store.reclaim_session_execution_lease(
            session_id,
            &takeover_owner,
            &observed_stale,
            60_000,
        ));
        takeover_completed_thread.store(true, std::sync::atomic::Ordering::SeqCst);
        outcome
    });
    while !takeover_started.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !takeover_completed.load(std::sync::atomic::Ordering::SeqCst),
        "takeover must wait for the validated claim mutation to leave its transaction"
    );

    tokio::task::spawn_blocking(move || release_validation.wait())
        .await
        .expect("release claim validation hook");
    let stale_claim = stale_claim
        .await
        .expect("join stale claim")
        .expect("stale claim succeeds before serialized takeover")
        .expect("stale claim exists");
    assert_eq!(stale_claim.batches[0].batch_id, batch.batch_id);
    let live_lease = tokio::task::spawn_blocking(move || takeover.join())
        .await
        .expect("join takeover thread task")
        .expect("join takeover thread")
        .expect("takeover store call succeeds")
        .acquired()
        .expect("dead stale owner is reclaimable");
    assert!(live_lease.fencing_token > stale_lease.fencing_token);

    let live_claim = store
        .claim_ready_queued_work(
            session_id,
            &live_lease.fence(),
            &live_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            1,
        )
        .await
        .expect("claim through generation mismatch")
        .expect("successor generation reclaims retained row");
    assert_eq!(live_claim.batches[0].batch_id, batch.batch_id);
    assert!(live_claim.fencing_token > stale_claim.fencing_token);

    let mut state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..crate::RuntimeSessionState::default()
    };
    let incarnation_id = store
        .ensure_session_incarnation(&state.session_id, &state.policy)
        .await
        .expect("realize contention-test session lifetime");
    state.bind_durable_incarnation(incarnation_id);
    let err = store
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(stale_claim.completion()),
        )
        .await
        .expect_err("serialized takeover supersedes the stale claim completion");
    assert!(matches!(
        err,
        crate::StoreError::QueuedWorkClaimSuperseded { .. }
    ));
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
