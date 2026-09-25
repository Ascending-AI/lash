//! The bytes a turn commits are pinned across a change to how commit content
//! is assembled.
//!
//! Each scenario runs one turn over a clocked backend and a recording store,
//! and asserts a digest of every runtime commit the store applied, plus the
//! assembled turn's committed facts, against values captured before commit
//! content moved from the observation stream onto the driver's recorded state
//! (FIG-3672 P6). A difference here is a durable-format change and needs a
//! version bump, not a new pin.
//!
//! The two cancelled pins were retaken once, for a change of value and not of
//! shape (FIG-3672 P9): a host-local stop is now a durable request with
//! lash's internal evidence for the turn, so the committed cancellation names
//! `internal:{turn_id}` rather than the provider-abort id the live token used
//! to mint.
//!
//! Every pin was retaken once more for a change of value and not of shape
//! (FIG-3600): drive admission mints a turn's root from its durable input, so
//! a direct turn's accepted input now carries its turn id in the existing
//! optional `source_key` field. With that field removed, each commit digests
//! to its previous pin.
//!
//! The digest is over the commit's serialized form with the values that differ
//! between two runs of the same turn masked: worker and lease identities,
//! random ids, wall-clock timestamps, and the hashes computed over them. Every
//! other byte, including every committed tool call, usage row and outcome, is
//! pinned.

use super::*;

fn usage(input_tokens: i64, output_tokens: i64) -> LlmStreamEvent {
    LlmStreamEvent::Usage(LlmUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    })
}

fn text_call(text: &str, input_tokens: i64) -> MockCall {
    MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: text.to_string(),
            },
            usage(input_tokens, 3),
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

fn tool_call(call_id: &str, value: &str) -> MockCall {
    MockCall {
        stream_events: vec![usage(7, 2)],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: call_id.to_string(),
                tool_name: "echo_tool".to_string(),
                input_json: serde_json::json!({ "value": value }).to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }
}

struct Pinned {
    commit_hashes: Vec<String>,
    assembled: serde_json::Value,
}

/// Run one turn over a clocked backend and a recording store, with recording
/// host sinks attached: attaching sinks must not change a committed byte.
async fn run_pinned_turn(
    calls: Vec<MockCall>,
    tools: Arc<dyn lash_core::ToolProvider>,
    cancel: CancellationToken,
    turn_id: &str,
) -> Pinned {
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(1_000));
    let backend = memory_backend_with_clock(Arc::clone(&clock)).await;
    let store = unbound_recording_store_with_clock(&backend, clock).await;
    let session_id = format!("pin:{turn_id}");
    let mut runtime = crate::runtime_support::commit_pins::pinned_runtime(
        &session_id,
        Vec::new(),
        tools,
        mock_provider(calls),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;
    let sessions = RecordingSink::default();
    let activities = RecordingTurnEvents::default();
    let scope = host_turn_scope(
        &runtime.host.core,
        &SessionId::from(session_id.as_str()),
        &TurnId::from(turn_id),
    );
    let turn = runtime
        .stream_turn(
            TurnInput::text("use the tool, then answer"),
            TurnOptions::new(cancel, scope)
                .with_events(&sessions)
                .with_turn_events(&activities),
        )
        .await
        .expect("the turn assembles");
    assert!(
        matches!(sessions.snapshot().last(), Some(SessionStreamEvent::Done)),
        "the host stream ends with `Done`"
    );
    Pinned {
        commit_hashes: store
            .runtime_commits()
            .iter()
            .map(crate::runtime_support::commit_pins::commit_digest)
            .collect(),
        assembled: assembled_facts(&turn),
    }
}

fn assembled_facts(turn: &AssembledTurn) -> serde_json::Value {
    let tool_calls = turn
        .tool_calls
        .iter()
        .map(|record| {
            serde_json::json!({
                "call_id": record.call_id,
                "tool": record.tool,
                "args": record.args,
                "output": record.output,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "outcome": turn.outcome,
        "tool_calls": tool_calls,
        "omitted": turn.omitted,
        "token_usage": turn.token_usage,
        "had_tool_calls": turn.execution.had_tool_calls,
        "had_code_execution": turn.execution.had_code_execution,
        "assistant_output": turn.assistant_output.safe_text,
        "errors": turn.errors.iter().map(|issue| &issue.message).collect::<Vec<_>>(),
    })
}

fn assert_pinned(scenario: &str, pinned: &Pinned, hashes: &[&str], assembled: &str) {
    assert_eq!(
        pinned.commit_hashes, hashes,
        "{scenario}: the committed bytes changed"
    );
    assert_eq!(
        pinned.assembled,
        serde_json::from_str::<serde_json::Value>(assembled).expect("pinned assembled turn"),
        "{scenario}: the assembled turn changed"
    );
}

#[tokio::test]
async fn tool_turn_commits_the_pinned_bytes() {
    let pinned = Box::pin(run_pinned_turn(
        vec![tool_call("pin-call-1", "alpha"), text_call("done", 11)],
        Arc::new(EchoTool),
        CancellationToken::new(),
        "commit-pin-tool-turn",
    ))
    .await;
    assert_pinned(
        "tool turn",
        &pinned,
        &["9f539fffb93abd5ffb3fb0b0b5504765a24d0cbddffa42b35c90c45d6442f877"],
        r#"{
            "assistant_output": "done",
            "errors": [],
            "had_code_execution": false,
            "had_tool_calls": true,
            "omitted": null,
            "outcome": {
                "finished": {
                    "assistant_message": {
                        "text": "done"
                    }
                }
            },
            "token_usage": {
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "input_tokens": 18,
                "output_tokens": 5,
                "reasoning_output_tokens": 0
            },
            "tool_calls": [
                {
                    "args": {
                        "value": "alpha"
                    },
                    "call_id": "pin-call-1",
                    "output": {
                        "outcome": {
                            "payload": {
                                "$lash_tool_value": "untrusted_json",
                                "value": {
                                    "payload": "raw:alpha"
                                }
                            },
                            "status": "success"
                        }
                    },
                    "tool": "echo_tool"
                }
            ]
        }"#,
    );
}

#[tokio::test]
async fn parallel_tool_turn_commits_the_pinned_bytes() {
    let parallel = MockCall {
        stream_events: vec![usage(9, 4)],
        response: Ok(LlmResponse {
            parts: ["beta", "gamma", "delta"]
                .into_iter()
                .enumerate()
                .map(|(index, value)| LlmOutputPart::ToolCall {
                    call_id: format!("pin-parallel-{index}"),
                    tool_name: "echo_tool".to_string(),
                    input_json: serde_json::json!({ "value": value }).to_string(),
                    replay: None,
                })
                .collect(),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let pinned = Box::pin(run_pinned_turn(
        vec![parallel, text_call("all three echoed", 13)],
        Arc::new(EchoTool),
        CancellationToken::new(),
        "commit-pin-parallel-tool-turn",
    ))
    .await;
    assert_pinned(
        "parallel tool turn",
        &pinned,
        &["af9d036811f0172cdc3ab01bd482093db7d755269c9f3e9bede5f9496880079a"],
        r#"{
            "assistant_output": "all three echoed",
            "errors": [],
            "had_code_execution": false,
            "had_tool_calls": true,
            "omitted": null,
            "outcome": {
                "finished": {
                    "assistant_message": {
                        "text": "all three echoed"
                    }
                }
            },
            "token_usage": {
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "input_tokens": 22,
                "output_tokens": 7,
                "reasoning_output_tokens": 0
            },
            "tool_calls": [
                {
                    "args": {
                        "value": "beta"
                    },
                    "call_id": "pin-parallel-0",
                    "output": {
                        "outcome": {
                            "payload": {
                                "$lash_tool_value": "untrusted_json",
                                "value": {
                                    "payload": "raw:beta"
                                }
                            },
                            "status": "success"
                        }
                    },
                    "tool": "echo_tool"
                },
                {
                    "args": {
                        "value": "gamma"
                    },
                    "call_id": "pin-parallel-1",
                    "output": {
                        "outcome": {
                            "payload": {
                                "$lash_tool_value": "untrusted_json",
                                "value": {
                                    "payload": "raw:gamma"
                                }
                            },
                            "status": "success"
                        }
                    },
                    "tool": "echo_tool"
                },
                {
                    "args": {
                        "value": "delta"
                    },
                    "call_id": "pin-parallel-2",
                    "output": {
                        "outcome": {
                            "payload": {
                                "$lash_tool_value": "untrusted_json",
                                "value": {
                                    "payload": "raw:delta"
                                }
                            },
                            "status": "success"
                        }
                    },
                    "tool": "echo_tool"
                }
            ]
        }"#,
    );
}

#[tokio::test]
async fn provider_failure_turn_commits_the_pinned_bytes() {
    let pinned = Box::pin(run_pinned_turn(
        vec![MockCall {
            stream_events: vec![usage(5, 0)],
            response: Err(LlmTransportError::new("pinned provider failure")
                .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::Forbidden)),
        }],
        Arc::new(EchoTool),
        CancellationToken::new(),
        "commit-pin-provider-failure",
    ))
    .await;
    assert_pinned(
        "provider failure",
        &pinned,
        &["a9c87de169d6084b27d2c2e3810d381c7ec6b0eddae3823e0d66ba85b2540f62"],
        r#"{
            "assistant_output": "",
            "errors": [
                "LLM error: pinned provider failure"
            ],
            "had_code_execution": false,
            "had_tool_calls": false,
            "omitted": null,
            "outcome": {
                "stopped": "provider_error"
            },
            "token_usage": {
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "input_tokens": 0,
                "output_tokens": 0,
                "reasoning_output_tokens": 0
            },
            "tool_calls": []
        }"#,
    );
}

#[tokio::test]
async fn cancelled_turn_commits_the_pinned_bytes() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let pinned = Box::pin(run_pinned_turn(
        vec![text_call("never streamed", 1)],
        Arc::new(EchoTool),
        cancel,
        "commit-pin-cancelled",
    ))
    .await;
    assert_pinned(
        "cancelled",
        &pinned,
        &["7d24d31afee8de86abfa925f9e7522ad09c27df9947cbf025822efc198ad7dc4"],
        r#"{
            "assistant_output": "",
            "errors": [],
            "had_code_execution": false,
            "had_tool_calls": false,
            "omitted": null,
            "outcome": {
                "stopped": {
                    "cancelled": {
                        "evidence": {
                            "request_id": "internal:commit-pin-cancelled"
                        }
                    }
                }
            },
            "token_usage": {
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "input_tokens": 0,
                "output_tokens": 0,
                "reasoning_output_tokens": 0
            },
            "tool_calls": []
        }"#,
    );
}

/// A cancel that lands while a tool is running: the tool itself requests it
/// as it starts, then observes it, and the turn stops cancelled.
#[tokio::test]
async fn cancelled_mid_tool_turn_commits_the_pinned_bytes() {
    let slow = MockCall {
        stream_events: vec![usage(6, 1)],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "pin-slow-call".to_string(),
                tool_name: "slow_tool".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let cancel = CancellationToken::new();
    let pinned = Box::pin(run_pinned_turn(
        vec![slow, text_call("never reached", 1)],
        Arc::new(CancelOnStart {
            cancel: cancel.clone(),
            inner: SlowTool {
                observed_cancel: Arc::new(AtomicBool::new(false)),
                started: Arc::new(tokio::sync::Notify::new()),
            },
        }),
        cancel,
        "commit-pin-cancelled-mid-tool",
    ))
    .await;
    assert_pinned(
        "cancelled mid tool",
        &pinned,
        &["7b8f1d4f588f5d1104f34b6ad96820129c7af307cf3c5b3961abf7e0b7556c08"],
        r#"{
            "assistant_output": "",
            "errors": [],
            "had_code_execution": false,
            "had_tool_calls": true,
            "omitted": null,
            "outcome": {
                "stopped": {
                    "cancelled": {
                        "evidence": {
                            "request_id": "internal:commit-pin-cancelled-mid-tool"
                        }
                    }
                }
            },
            "token_usage": {
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "input_tokens": 6,
                "output_tokens": 1,
                "reasoning_output_tokens": 0
            },
            "tool_calls": [
                {
                    "args": {},
                    "call_id": "pin-slow-call",
                    "output": {
                        "outcome": {
                            "payload": {
                                "message": "tool call cancelled",
                                "source": "cancellation"
                            },
                            "status": "cancelled"
                        }
                    },
                    "tool": "slow_tool"
                }
            ]
        }"#,
    );
}

/// `SlowTool`, which cancels the turn as it starts.
struct CancelOnStart {
    cancel: CancellationToken,
    inner: SlowTool,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CancelOnStart {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.inner.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.inner.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.cancel.cancel();
        self.inner.execute(call).await
    }
}

/// A host sink that blocks until released holds neither the commit nor the
/// turn's decisions, and the turn announces itself finished only after the
/// host has received its whole stream (the `TurnObserver` host contract).
#[tokio::test]
async fn a_blocked_host_sink_holds_neither_the_commit_nor_its_bytes() {
    const TURN_ID: &str = "commit-pin-parallel-tool-turn";
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(1_000));
    let backend = memory_backend_with_clock(Arc::clone(&clock)).await;
    let store = unbound_recording_store_with_clock(&backend, clock).await;
    let parallel = MockCall {
        stream_events: vec![usage(9, 4)],
        response: Ok(LlmResponse {
            parts: ["beta", "gamma", "delta"]
                .into_iter()
                .enumerate()
                .map(|(index, value)| LlmOutputPart::ToolCall {
                    call_id: format!("pin-parallel-{index}"),
                    tool_name: "echo_tool".to_string(),
                    input_json: serde_json::json!({ "value": value }).to_string(),
                    replay: None,
                })
                .collect(),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    // The same session and turn as the parallel tool pin, so the commit
    // digests match it.
    let session_id = format!("pin:{TURN_ID}");
    let mut runtime = crate::runtime_support::commit_pins::pinned_runtime(
        &session_id,
        Vec::new(),
        Arc::new(EchoTool),
        mock_provider(vec![parallel, text_call("all three echoed", 13)]),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;
    let turn_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        Arc::clone(&runtime.host.core.control.effect_host),
        session_id.as_str(),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    );
    let address =
        lash_core::facade_support::TurnAddress::new(SessionId::from(session_id.as_str()), TURN_ID);
    let scope = host_turn_scope(
        &runtime.host.core,
        &SessionId::from(session_id.as_str()),
        &TurnId::from(TURN_ID),
    );
    let host = GatedHost::default();
    let turn = lash_core::task::spawn({
        let host = host.clone();
        async move {
            runtime
                .stream_turn(
                    TurnInput::text("use the tool, then answer"),
                    TurnOptions::new(CancellationToken::new(), scope)
                        .with_events(&host)
                        .with_turn_events(&host),
                )
                .await
        }
    });

    // The commit lands while the host takes nothing, and commits the pinned
    // bytes.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while store.runtime_commits().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the turn commits while its host sink is blocked");
    crate::runtime_support::commit_pins::assert_commit_pins(
        "blocked host",
        &store.runtime_commits(),
        &["af9d036811f0172cdc3ab01bd482093db7d755269c9f3e9bede5f9496880079a"],
    );
    assert!(host.received().is_empty(), "the host has taken nothing yet");

    // "Finished" waits for the host: no terminal, and the turn call has not
    // returned.
    assert!(
        turn_driver
            .await_terminal_with_timeout(&address, std::time::Duration::from_millis(200))
            .await
            .is_err(),
        "the terminal is not published before the host has the stream"
    );
    assert!(!turn.is_finished(), "the turn call waits for the host");

    host.release();
    let assembled = turn
        .await
        .expect("the turn task completes")
        .expect("the turn assembles");
    assert!(matches!(assembled.outcome, TurnOutcome::Finished(_)));
    let terminal = turn_driver
        .await_terminal_with_timeout(&address, std::time::Duration::from_secs(5))
        .await
        .expect("the terminal is published once the host has the stream");
    assert!(matches!(
        terminal,
        lash_core::facade_support::TurnTerminal::Committed { .. }
    ));

    // The host received the whole stream, in order.
    let sessions = host
        .received()
        .into_iter()
        .filter_map(|received| match received {
            Received::Session(event) => Some(event),
            Received::Activity(_) => None,
        })
        .collect::<Vec<_>>();
    let tool_calls = sessions
        .iter()
        .filter_map(|event| match event {
            SessionStreamEvent::ToolCall { call_id, .. } => call_id.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_calls,
        ["pin-parallel-0", "pin-parallel-1", "pin-parallel-2"]
    );
    let tail = &sessions[sessions.len().saturating_sub(2)..];
    assert!(
        matches!(
            tail,
            [
                SessionStreamEvent::TurnOutcome {
                    outcome: TurnOutcome::Finished(_)
                },
                SessionStreamEvent::Done
            ]
        ),
        "the stream ends with the outcome, then `Done`: {tail:?}"
    );
}

#[derive(Debug)]
enum Received {
    Session(SessionStreamEvent),
    Activity(TurnActivity),
}

/// Host sinks that take nothing until released, then record everything in
/// the order it arrives.
#[derive(Clone)]
struct GatedHost {
    released: Arc<tokio::sync::watch::Sender<bool>>,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Default for GatedHost {
    fn default() -> Self {
        Self {
            released: Arc::new(tokio::sync::watch::channel(false).0),
            received: Arc::default(),
        }
    }
}

impl GatedHost {
    fn release(&self) {
        self.released.send_replace(true);
    }

    fn received(&self) -> Vec<Received> {
        self.received
            .lock_recover()
            .iter()
            .map(|received| match received {
                Received::Session(event) => Received::Session(event.clone()),
                Received::Activity(activity) => Received::Activity(activity.clone()),
            })
            .collect()
    }

    async fn gate(&self) {
        let mut released = self.released.subscribe();
        let _ = released.wait_for(|released| *released).await;
    }
}

#[async_trait::async_trait]
impl EventSink for GatedHost {
    async fn emit(&self, event: SessionStreamEvent) {
        self.gate().await;
        self.received.lock_recover().push(Received::Session(event));
    }
}

#[async_trait::async_trait]
impl TurnActivitySink for GatedHost {
    async fn emit(&self, activity: TurnActivity) {
        self.gate().await;
        self.received
            .lock_recover()
            .push(Received::Activity(activity));
    }
}
