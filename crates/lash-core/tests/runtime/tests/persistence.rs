use super::*;
use lash_core::SessionCommitStore as _;
use lash_sansio::sync::MutexExt;

#[tokio::test]
async fn durable_turn_commit_rejects_token_usage_overflow() {
    let backend = memory_backend().await;
    let overflowing_call = || MockCall {
        stream_events: vec![LlmStreamEvent::Usage(LlmUsage {
            input_tokens: i64::MAX,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        })],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "accounted".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let transport = mock_provider(vec![overflowing_call(), overflowing_call()]);
    let store = unbound_recording_store(&backend).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;
    runtime
        .state
        .token_ledger
        .push(lash_core::TokenLedgerEntry {
            source: "turn".to_string(),
            model: "mock-model".to_string(),
            usage: lash_core::TokenUsage {
                input_tokens: 1,
                ..lash_core::TokenUsage::default()
            },
            usage_disposition: Default::default(),
        });

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("account this turn"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("usage-overflow"),
            ),
        )
        .await
        .expect_err("overflow must reject the durable commit");

    assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreCommitFailed);
    assert_eq!(
        error.message,
        "token usage counter `input_tokens` overflowed while accumulating (turn, mock-model)"
    );
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);

    let next_error = runtime
        .run_turn_assembled(
            TurnInput::text("the poisoned ledger must fail closed again"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("usage-overflow-next-turn"),
            ),
        )
        .await
        .expect_err("the unconfirmed overflowing row must poison the next turn");
    assert_eq!(
        next_error.code,
        lash_core::RuntimeErrorCode::StoreCommitFailed
    );
    assert_eq!(next_error.message, error.message);
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);
}

#[tokio::test]
async fn multi_call_turn_rejects_cumulative_usage_overflow_before_commit() {
    let backend = memory_backend().await;
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
                parts: vec![LlmOutputPart::Text {
                    text: "must not commit".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let store = unbound_recording_store(&backend).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("use the tool, then answer"),
            CancellationToken::new(),
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("multi-call-usage-overflow"),
            ),
        )
        .await
        .expect_err("the second LLM usage event must reject cumulative overflow");

    assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreCommitFailed);
    assert_eq!(
        error.message,
        "token usage counter `input_tokens` overflowed while accumulating (turn, mock-model)"
    );
    assert_eq!(*store.runtime_commit_count.lock_recover(), 0);
}

#[tokio::test]
async fn standard_runtime_assembles_stream_only_text_response() {
    let backend = memory_backend().await;
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "What time ".to_string(),
            },
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "is it?".to_string(),
            },
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 11,
                output_tokens: 4,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "What time is it?".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
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
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("stream-only-text-turn"),
                ),
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
    // A turn that finishes *as* an assistant message leaves that text committed
    // exactly once. Whether the protocol committed it during the turn or the
    // turn boundary materialized it at the end is the runtime's business; the
    // count is the contract, and hosts read it as "this reply is already durable
    // and is not mine to commit again" (FIG-984).
    let assistant_messages = active_conversation_messages(&turn.state)
        .into_iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_messages
            .iter()
            .map(|message| message.parts[0].content())
            .collect::<Vec<_>>(),
        vec!["What time is it?"],
        "the assistant reply a turn finishes with must be committed exactly once"
    );

    let streamed_text: String = sink
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            SessionStreamEvent::TextDelta { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "What time is it?");
}

#[tokio::test]
async fn standard_runtime_recovers_streamed_text_when_final_response_is_empty() {
    let backend = memory_backend().await;
    let expected =
        "I’m continuing with a type-safety cleanup now: replace the remaining raw JSON paths.";
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "I’m continuing with a type-safety cleanup now: ".to_string(),
            },
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "replace the remaining raw JSON paths.".to_string(),
            },
        ],
        response: Ok(LlmResponse::default()),
    }]);
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
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
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("recover-streamed-text-turn"),
                ),
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
    assert_eq!(assistant_messages[0].parts[0].content(), expected);

    let streamed_text: String = sink
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            SessionStreamEvent::TextDelta { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, expected);
}

#[tokio::test]
async fn standard_runtime_text_part_reconciles_without_streaming_duplicate() {
    let backend = memory_backend().await;
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "The sentence.".to_string(),
            },
            LlmStreamEvent::Part(LlmOutputPart::Text {
                text: "The sentence.".to_string(),
                response_meta: None,
            }),
        ],
        response: Ok(LlmResponse::default()),
    }]);
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
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
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("text-part-no-duplicate-turn"),
                ),
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
            SessionStreamEvent::TextDelta { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "The sentence.");
}

#[tokio::test]
async fn standard_runtime_cancels_in_flight_tool_calls_when_token_fires() {
    let backend = memory_backend().await;
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
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(SlowTool {
        observed_cancel: Arc::clone(&observed_cancel),
    });
    let mut runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;
    let cancel = CancellationToken::new();
    let cancel_trigger = cancel.clone();
    lash_core::task::spawn(async move {
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
                turn_context: lash_core::TurnContext::default(),
            },
            cancel,
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("cancel-tool-turn"),
            ),
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
    let backend = memory_backend().await;
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
                parts: vec![LlmOutputPart::Text {
                    text: "unexpected follow-up".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(TerminalControlTool {
        controls: vec![
            lash_core::ToolControl::Finish {
                value: lash_core::ToolValue::untrusted_json(json!("first")),
            },
            lash_core::ToolControl::Finish {
                value: lash_core::ToolValue::untrusted_json(json!("second")),
            },
        ],
    });
    let mut runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;
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
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from("terminal-tool-finish-turn"),
                ),
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
    let backend = memory_backend().await;
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
                parts: vec![LlmOutputPart::Text {
                    text: "unexpected follow-up".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(TerminalControlTool {
        controls: vec![lash_core::ToolControl::Fail {
            failure: lash_core::ToolFailure::tool(
                lash_core::ToolFailureClass::Execution,
                "terminal_control_failed",
                "failed",
            ),
        }],
    });
    let mut runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;
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
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from("terminal-tool-fail-turn"),
                ),
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
    let backend = memory_backend().await;
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
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(EchoTool);
    let mut runtime = runtime_with_plugins_and_tools(&backend, Vec::new(), tools, transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            host_turn_scope(
                &runtime.host.core,
                &SessionId::from("root"),
                &TurnId::from("streamed-tool-call-turn"),
            ),
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
    let backend = memory_backend().await;
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![],
        response: Ok(LlmResponse {
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
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;
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
                turn_context: lash_core::TurnContext::default(),
            },
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("part-boundaries-turn"),
                ),
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
            SessionStreamEvent::TextDelta { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "Intro paragraph.\n\n## Heading");
}

#[tokio::test]
async fn standard_runtime_uses_streamed_usage_when_final_usage_missing() {
    let backend = memory_backend().await;
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "Hi".to_string(),
            },
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 9,
                output_tokens: 3,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Hi".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage::default(),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("streamed-usage-turn"),
            ),
        )
        .await
        .expect("turn");

    assert_eq!(turn.token_usage.input_tokens, 9);
    assert_eq!(turn.token_usage.output_tokens, 3);
    assert_eq!(turn.token_usage.cache_read_input_tokens, 2);
}

#[tokio::test]
async fn standard_runtime_prefers_final_usage_over_streamed_usage() {
    let backend = memory_backend().await;
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "Hi".to_string(),
            },
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 9,
                output_tokens: 3,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
        ],
        response: Ok(LlmResponse {
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
    let mut runtime = standard_runtime_with_transport(&backend, transport).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("final-usage-turn"),
            ),
        )
        .await
        .expect("turn");

    assert_eq!(turn.token_usage.input_tokens, 12);
    assert_eq!(turn.token_usage.output_tokens, 4);
    assert_eq!(turn.token_usage.cache_read_input_tokens, 1);
}

// ADR 0069: direct turns enter through the same durable acceptance the queued
// ingress uses, so the in-memory store owes the same laws the durable backends
// do.

#[tokio::test]
async fn rejected_refresh_does_not_retain_stale_checkpoint_components() {
    let backend = memory_backend().await;
    struct BrokenRead {
        inner: Arc<RecordingStore>,
        read: Mutex<Option<lash_core::testing::runtime_internals::PersistedSessionRead>>,
        loads: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl lash_core::store::RuntimePersistenceDecorator for BrokenRead {
        fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
            self.inner.as_ref()
        }
        async fn load_session(
            &self,
        ) -> Result<
            Option<lash_core::testing::runtime_internals::PersistedSessionRead>,
            lash_core::StoreError,
        > {
            if let Some(read) = self.read.lock_recover().clone() {
                self.loads.fetch_add(1, Ordering::SeqCst);
                return Ok(Some(read));
            }
            lash_core::SessionCommitStore::load_session(self.inner.as_ref()).await
        }
        async fn load_session_head_meta(
            &self,
        ) -> Result<Option<lash_core::store::SessionHeadMeta>, lash_core::StoreError> {
            if let Some(read) = self.read.lock_recover().as_ref() {
                return Ok(Some(lash_core::store::SessionHeadMeta::assemble(
                    &read.session_id,
                    lash_core::store::SessionHeadPayload {
                        schema_version: lash_core::CURRENT_SESSION_STATE_VERSION,
                        session_id: read.session_id.clone(),
                        config: read.config.clone(),
                        current_frame_node_id: read.current_frame_node_id.clone(),
                    },
                    read.head_revision,
                    read.checkpoint_ref.clone(),
                    read.graph.leaf_node_id.clone(),
                )?));
            }
            lash_core::SessionCommitStore::load_session_head_meta(self.inner.as_ref()).await
        }
    }
    let store = Arc::new(BrokenRead {
        inner: unbound_recording_store(&backend).await,
        read: Mutex::new(None),
        loads: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        store.clone(),
    )
    .await;
    runtime
        .state
        .set_execution_state_snapshot(Some(b"old-frame-root".to_vec().into()));
    let old_frame = runtime.state.current_frame_node_id.clone();
    let mut replacement = runtime.state.clone();
    lash_core::runtime::state::open_agent_frame_in_state_with_clock(
        &mut replacement,
        lash_core::testing::runtime_internals::OpenAgentFrameRequest::new(
            lash_core::FrameKey::from_caller_material("review-new-frame").unwrap(),
            lash_core::AgentFrameReason::new("review"),
        ),
        &lash_core::testing::TestClock::new(1000),
    )
    .expect("open a fresh review frame");
    assert_ne!(replacement.current_frame_node_id, old_frame);
    replacement
        .session_graph
        .validate_resident_integrity()
        .unwrap();
    let config = lash_core::RuntimeCommit::persisted_state_for_test(&replacement, &[]).config;
    let mut checkpoint = lash_core::HydratedSessionCheckpoint::default();
    checkpoint.turn_state.turn_index = usize::MAX;
    *store.read.lock_recover() = Some(
        lash_core::testing::runtime_internals::PersistedSessionRead {
            session_id: replacement.session_id.clone(),
            head_revision: runtime.state.head_revision + 1,
            config,
            current_frame_node_id: replacement.current_frame_node_id.clone(),
            graph: replacement.session_graph.clone(),
            checkpoint_ref: Some("new-checkpoint".to_string().into()),
            checkpoint: Some(checkpoint),
            token_ledger: Vec::new(),
            turn_failure_settlements: Vec::new(),
        },
    );
    let first = runtime.refresh_session_graph_from_store().await;
    assert!(matches!(
        first,
        Err(SessionError::Store {
            source: lash_core::StoreError::CheckpointTurnIndexOutOfRange { .. },
            ..
        })
    ));
    let second = runtime.refresh_session_graph_from_store().await;
    assert!(second.is_ok());
    eprintln!(
        "STALE_REFRESH first={first:?} second={second:?} loads={} old_frame={old_frame:?} new_frame={:?} execution={:?}",
        store.loads.load(Ordering::SeqCst),
        runtime.state.current_frame_node_id,
        runtime.state.execution_state_hydration()
    );
    eprintln!(
        "STALE_CAPTURE {:?}",
        runtime
            .state
            .checkpoint_components
            .build_checkpoint(lash_core::PersistedTurnState::default())
            .map(|c| c.components.keys().cloned().collect::<Vec<_>>())
    );
    assert!(
        runtime.state.execution_state_hydration().unwrap().is_none(),
        "failed checkpoint adoption retained the previous frame execution under the new head; retry skipped hydration"
    );
    assert!(matches!(
        runtime
            .state
            .checkpoint_components
            .build_checkpoint(lash_core::PersistedTurnState::default()),
        Err(lash_core::StoreError::IncompleteCheckpointComponentSet)
    ));
}

// A turn commit whose reply is lost after the store applied it must not keep
// its staged usage pending: the durable journal already carries those rows,
// and a live ledger that adds both counts the turn twice.
#[tokio::test]
async fn ambiguous_turn_commit_does_not_double_count_live_usage() {
    let backend = memory_backend().await;
    struct LostCommitReplyStore {
        inner: Arc<RecordingStore>,
        armed: AtomicBool,
    }
    #[async_trait::async_trait]
    impl lash_core::store::RuntimePersistenceDecorator for LostCommitReplyStore {
        fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
            self.inner.as_ref()
        }
        async fn commit_runtime_state(
            &self,
            commit: lash_core::store::RuntimeCommit,
        ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
            let is_turn_final = commit.turn_commit.operation.key == "final";
            let result =
                lash_core::SessionCommitStore::commit_runtime_state(self.inner.as_ref(), commit)
                    .await;
            if is_turn_final && result.is_ok() && self.armed.swap(false, Ordering::SeqCst) {
                return Err(lash_core::StoreError::Backend(
                    "injected lost commit reply".to_string(),
                ));
            }
            result
        }
    }
    let usage_call = |input_tokens: i64, output_tokens: i64| MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "accounted".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let inner_store = unbound_recording_store(&backend).await;
    let store = Arc::new(LostCommitReplyStore {
        inner: Arc::clone(&inner_store),
        armed: AtomicBool::new(true),
    });
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![usage_call(12, 4), usage_call(5, 2)]),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("account this turn"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("ambiguous-commit-turn"),
            ),
        )
        .await
        .expect_err("the landed commit's reply is lost");
    assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreCommitFailed);

    // The commit landed: the durable journal already holds the turn's usage.
    let durable = lash_core::SessionCommitStore::load_session(inner_store.as_ref())
        .await
        .expect("load the durable session")
        .expect("the session has a committed head")
        .token_ledger;
    assert_eq!(
        durable
            .iter()
            .map(|entry| entry.usage.input_tokens)
            .sum::<i64>(),
        12
    );
    // Its staged copies are gone: nothing pending can count them again.
    assert!(
        runtime.shared_token_ledger.lock_recover().is_empty(),
        "a landed commit's staged rows must be discarded: {:?}",
        runtime.shared_token_ledger.lock_recover()
    );

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("reload the landed head");
    let report = runtime.usage_report();
    assert_eq!(report.usage.usage.input_tokens, 12);
    assert_eq!(report.usage.usage.output_tokens, 4);

    let handle = RuntimeHandle::new(runtime);
    let observation = handle.observe();
    assert_eq!(observation.usage_report.usage.usage.input_tokens, 12);
    assert_eq!(observation.usage_report.usage.usage.output_tokens, 4);

    {
        let mut runtime = handle.runtime.lock().await;
        runtime
            .run_turn_assembled(
                TurnInput::text("account the next turn"),
                CancellationToken::new(),
                backend_turn_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("after-ambiguous-commit-turn"),
                ),
            )
            .await
            .expect("the next turn commits normally");
        handle.publish_from(&runtime);
        // The resident ledger matches the durable journal exactly: the lost
        // reply's usage is not folded in a second time.
        let resident_input = runtime
            .state
            .token_ledger
            .iter()
            .map(|entry| entry.usage.input_tokens)
            .sum::<i64>();
        assert_eq!(resident_input, 17);
        let resident_output = runtime
            .state
            .token_ledger
            .iter()
            .map(|entry| entry.usage.output_tokens)
            .sum::<i64>();
        assert_eq!(resident_output, 6);
    }
    let observation = handle.observe();
    assert_eq!(observation.usage_report.usage.usage.input_tokens, 17);
    assert_eq!(observation.usage_report.usage.usage.output_tokens, 6);
    let durable = lash_core::SessionCommitStore::load_session(inner_store.as_ref())
        .await
        .expect("load the durable session")
        .expect("the session has a committed head")
        .token_ledger;
    assert_eq!(
        durable
            .iter()
            .map(|entry| entry.usage.input_tokens)
            .sum::<i64>(),
        17
    );
}
