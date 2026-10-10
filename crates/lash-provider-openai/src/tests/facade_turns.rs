//! Facade turns over this crate's providers: a `LashCore` on the durable
//! backend over a SQLite memory store set, whose own node serves each
//! session's turn on the durable path (ADR 0132).

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDefinition,
    ToolOutcome,
};
use lash::{LashCore, TurnEvent, TurnInput};
use lash_core::provider::{ProviderHandle, ProviderOptions};
use lash_sansio::sync::MutexExt;
use serde_json::{Value, json};

use crate::CodexProvider;
use crate::codex::ws_testing::{ScriptedWsAction, ScriptedWsServer, spawn_scripted_websocket};

/// How long a law waits for a turn that can only hang: a deadlock watchdog,
/// no part of any law.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(120);

/// A core on the durable backend over a fresh SQLite memory store set,
/// serving `model` through `provider`, with `tools` when given.
pub(super) async fn durable_core(
    model: &str,
    metadata: lash::LlmProfileMetadata,
    provider: ProviderHandle,
    tools: Option<Arc<dyn lash::tools::ToolProvider>>,
) -> LashCore {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("an in-memory store set opens");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend builds");
    let builder = LashCore::standard_builder(backend)
        .llm_profiles(Arc::new(
            lash::LlmProfileRegistry::new()
                .register(model, lash::RegisteredLlmProfile::new(metadata, provider))
                .expect("register the test model"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended());
    let builder = match tools {
        Some(tools) => builder.tools(tools),
        None => builder,
    };
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(format!("{model}-facade-turns")),
            lash::persistence::LeaseIncarnationId::new(format!("{model}-facade-turns-boot")),
        ))
        .expect("the core builds")
}

/// Create the root session `session_id` on `model`.
pub(super) async fn session(
    core: &LashCore,
    model: &str,
    session_id: &str,
) -> lash::DurableSession {
    core.session(lash::SessionId::fixture(session_id.to_owned()))
        .create(lash::SessionCreation::root(
            lash_core::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("the session is created")
}

/// Send `session` the text `input`; the core's node runs the turn and the
/// handle answers its settled output.
pub(super) async fn send(session: &lash::DurableSession, input: &str) -> lash::TurnOutput {
    tokio::time::timeout(WATCHDOG, session.send(TurnInput::text(input)).output())
        .await
        .expect("deadlock watchdog: the turn settles")
        .expect("the turn answers")
}

fn websocket_provider(server: &ScriptedWsServer) -> ProviderHandle {
    let provider = CodexProvider::new(std::sync::Arc::new(lash_core::provider::ProviderToken::new("access")))
        .force_websocket_transport()
        // The SSE URL is never dialed on the pinned WebSocket path; point it
        // somewhere unroutable so a regression that falls back to HTTP fails
        // loudly instead of silently passing.
        .with_endpoint_urls("http://127.0.0.1:9/unused-sse", server.url.clone())
        .with_options(ProviderOptions {
            reliability: crate::CodexProvider::reliability()
                .request_timeout_ms(Some(5_000))
                .stream_chunk_timeout_ms(Some(2_000)),
            ..ProviderOptions::default()
        });
    ProviderHandle::new(provider.into_components())
}

fn websocket_metadata() -> lash::LlmProfileMetadata {
    lash::LlmProfileMetadata::builder("gpt-5.4")
        .cache_retention(lash::provider::CacheRetention::Short)
        .context_window_tokens(16_000)
        .build()
        .expect("valid model spec")
}

/// A facade turn on the Codex WebSocket transport streams the assistant's
/// text as deltas and settles it, over one handshake carrying the session
/// scope and one `response.create` for the configured model.
#[tokio::test(flavor = "multi_thread")]
async fn codex_websocket_facade_turn_streams_text_from_local_server() {
    let server = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_ws_1",
        message_id: "msg_ws_1",
        text: "hello over the websocket",
    }])
    .await;
    let core = durable_core(
        "gpt-5.4",
        websocket_metadata(),
        websocket_provider(&server),
        None,
    )
    .await;
    let session = session(&core, "gpt-5.4", "codex-ws-runtime-text").await;

    let handle = tokio::time::timeout(WATCHDOG, session.send(TurnInput::text("say hello")))
        .await
        .expect("deadlock watchdog: the send is accepted")
        .expect("send");
    let mut stream = handle.events();
    let mut streamed = String::new();
    while let Some(activity) = tokio::time::timeout(WATCHDOG, stream.next_activity())
        .await
        .expect("deadlock watchdog: the turn's activity ends")
    {
        let activity = activity.expect("turn activity");
        if let TurnEvent::StreamBlock(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            text,
            ..
        }) = activity.event
        {
            streamed.push_str(&text);
        }
    }
    let output = tokio::time::timeout(WATCHDOG, handle.output())
        .await
        .expect("deadlock watchdog: the turn settles")
        .expect("turn result");

    assert_eq!(
        output.assistant_message().unwrap_or_default(),
        "hello over the websocket"
    );
    assert_eq!(
        streamed, "hello over the websocket",
        "assistant text must arrive as streamed deltas, not only in the final result"
    );
    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["type"], "response.create");
    assert_eq!(captured[0]["model"], "gpt-5.4");
    assert_eq!(captured[0]["stream"], true);
    let handshakes = server.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert!(
        handshakes[0]
            .iter()
            .any(|(name, value)| name == "session-id" && !value.is_empty()),
        "handshake must carry the runtime session scope, got {handshakes:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

struct EchoProbe {
    seen: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl StaticToolExecute for EchoProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "echo_probe");
            self.seen.lock_recover().push(call.args.clone());
            ToolOutcome::ok(json!({ "echo": call.args }))
        })
        .await
        .into()
    }
}

fn echo_probe_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:echo_probe",
        "echo_probe",
        "Echo the provided arguments.",
        json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A tool call streamed over the Codex WebSocket runs once, and the
/// follow-up request carries its output back as `function_call_output`.
#[tokio::test(flavor = "multi_thread")]
async fn codex_websocket_facade_turn_round_trips_a_tool_call() {
    let server = spawn_scripted_websocket(vec![
        ScriptedWsAction::ToolCall {
            response_id: "resp_ws_tool",
            call_id: "call_echo_1",
            tool_name: "echo_probe",
            arguments: r#"{"value":"ping"}"#,
        },
        ScriptedWsAction::Complete {
            response_id: "resp_ws_2",
            message_id: "msg_ws_2",
            text: "tool round trip complete",
        },
    ])
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = durable_core(
        "gpt-5.4",
        websocket_metadata(),
        websocket_provider(&server),
        Some(Arc::new(StaticToolProvider::new(
            vec![echo_probe_definition()],
            EchoProbe {
                seen: Arc::clone(&seen),
            },
        ))),
    )
    .await;
    let session = session(&core, "gpt-5.4", "codex-ws-runtime-tool").await;

    let output = send(&session, "probe the echo tool").await;

    assert_eq!(
        output.assistant_message().unwrap_or_default(),
        "tool round trip complete"
    );
    assert_eq!(
        seen.lock_recover().as_slice(),
        [json!({"value": "ping"})],
        "the runtime must execute the tool call streamed over the websocket"
    );
    let captured = server.captured();
    assert_eq!(captured.len(), 2, "expected two response.create requests");
    assert!(captured.iter().all(|req| req["type"] == "response.create"));
    let follow_up_input = captured[1]["input"].as_array().expect("input items");
    assert!(
        follow_up_input.iter().any(|item| {
            item["type"] == "function_call_output" && item["call_id"] == "call_echo_1"
        }),
        "follow-up request must echo the tool output as function_call_output, got {follow_up_input:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// FIG-5450: cached rejection, empty-stream retry, partial-output retry and
/// overflow recovery compose without replenishing any owner's allowance.
/// The adapter's full-context resend stays inside one visible attempt;
/// overflow ends that call and the next host input gets a bounded rebuild.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_model_stream_recovery_obeys_recorded_allowances() {
    use lash_core::llm::types::{
        AttemptOutcome, ProtocolPosition, RetryClass, RetryDecision, RetryDeclineCause,
    };
    use std::time::Duration;

    for max_attempts in [2, 3] {
        let server = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_seed",
                message_id: "msg_seed",
                text: "committed seed",
            },
            ScriptedWsAction::RecordedFrames {
                frames: vec![
                    json!({
                        "type": "error", "error": {"code": "previous_response_not_found"}
                    })
                    .to_string(),
                ],
                close_after_frames: false,
            },
            ScriptedWsAction::RecordedFrames {
                frames: vec![
                    json!({"type": "response.created", "response": {
                        "id": "resp_empty", "status": "in_progress", "output": []
                    }})
                    .to_string(),
                ],
                close_after_frames: true,
            },
            ScriptedWsAction::CloseAfterStart {
                response_id: "resp_partial",
                message_id: "msg_partial",
                text: "uncommitted partial",
            },
            ScriptedWsAction::RecordedFrames {
                frames: vec![
                    json!({"type": "error", "error": {
                        "code": "context_length_exceeded", "message": "context window exceeded"
                    }})
                    .to_string(),
                ],
                close_after_frames: false,
            },
            ScriptedWsAction::Complete {
                response_id: "resp_summary",
                message_id: "msg_summary",
                text: "rebuilt context",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_answer",
                message_id: "msg_answer",
                text: "recovered answer",
            },
        ])
        .await;
        let provider =
            CodexProvider::new(Arc::new(lash_core::provider::ProviderToken::new("access")))
                .with_endpoint_urls("http://127.0.0.1:9/unused-sse", server.url.clone())
                .with_options(ProviderOptions {
                    reliability: CodexProvider::reliability()
                        .request_timeout_ms(Some(5_000))
                        .stream_chunk_timeout_ms(Some(2_000))
                        .base_delay_ms(0)
                        .max_delay_ms(0),
                    ..ProviderOptions::default()
                });
        let mut metadata = lash::LlmProfileMetadata::builder("gpt-5.4")
            .cache_retention(lash::provider::CacheRetention::Short)
            .context_window_tokens(200_000)
            .build()
            .expect("model metadata");
        metadata.capability.stream_termination =
            Some(lash_core::provider::StreamTermination::RequireTerminalEvidence);
        let mut config = lash::ExecutionBudgetsConfig::recommended();
        config.provider = lash::ProviderAttemptLimits::new(
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(2),
            max_attempts,
        )
        .expect("bounded attempts");
        let mut compaction = lash::plugins::StandardCompactionConfig::standard();
        compaction.overflow_max_attempts = std::num::NonZeroU32::MIN;
        let overflow_max_attempts = compaction.overflow_max_attempts.get();
        let stores = lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory stores");
        let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
            .build()
            .expect("durable backend");
        let core = LashCore::standard_builder(backend)
            .llm_profiles(Arc::new(
                lash::LlmProfileRegistry::new()
                    .register(
                        "mixed-recovery",
                        lash::RegisteredLlmProfile::new(
                            metadata,
                            ProviderHandle::new(provider.into_components()),
                        ),
                    )
                    .expect("registered model"),
            ))
            .plugin(Arc::new(
                lash::plugins::StandardCompactionPluginFactory::new(compaction),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::new(config).expect("recorded budgets"))
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new(format!("mixed-recovery-{max_attempts}")),
                lash::persistence::LeaseIncarnationId::new("mixed-recovery-boot"),
            ))
            .expect("core");
        let session = core
            .session(lash::SessionId::fixture(format!(
                "mixed-recovery-{max_attempts}"
            )))
            .create(lash::SessionCreation::root(
                lash_core::SessionToolAccess::ambient(),
                lash::SessionSpec::new(
                    "mixed-recovery",
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                )
                .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
                .charge_safety(
                    lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                        max_unsafe_retries: 1,
                        max_duplicate_cost_tokens: None,
                    },
                ),
            ))
            .await
            .expect("session");

        let seed = send(&session, "seed the cache").await;
        assert_eq!(seed.result.llm_calls[0].attempts.len(), 1);
        let mixed = send(&session, "continue the seed").await;
        let calls = &mixed.result.llm_calls;
        assert_eq!(calls.len(), 1, "recovery never hides an extra model call");
        let attempts = &calls[0].attempts;
        assert_eq!(attempts.len(), max_attempts as usize);
        assert_eq!(
            attempts.iter().map(|a| a.ordinal).collect::<Vec<_>>(),
            (1..=max_attempts).collect::<Vec<_>>()
        );
        assert!(attempts.iter().all(|a| a.retry_budget_consumed));
        assert_eq!(
            attempts[0].protocol_position,
            ProtocolPosition::ResponseObserved,
            "attempts={attempts:?}, requests={:?}",
            server.captured()
        );
        assert!(matches!(
            attempts[0].retry_decision,
            Some(RetryDecision::Scheduled {
                class: RetryClass::EmptyStreamPartial,
                ..
            })
        ));
        assert_eq!(attempts[1].outcome, AttemptOutcome::Interrupted);
        assert_eq!(
            attempts[1].protocol_position,
            ProtocolPosition::OutputStarted
        );
        assert!(
            attempts[1].usage.is_some(),
            "partial usage stays on its attempt"
        );
        assert!(
            !mixed
                .assistant_message()
                .unwrap_or_default()
                .contains("uncommitted partial")
        );

        let captured = server.captured();
        assert_eq!(
            captured.len(),
            2 + max_attempts as usize,
            "one seed and one adapter resend, plus the visible attempt allowance"
        );
        assert_eq!(captured[1]["previous_response_id"], "resp_seed");
        assert_eq!(
            captured[1]["input"]
                .as_array()
                .expect("cached suffix")
                .len(),
            1
        );
        assert!(captured[2].get("previous_response_id").is_none());
        assert!(captured[2]["input"].to_string().contains("committed seed"));
        assert_eq!(
            captured[2]["input"], captured[3]["input"],
            "retry restores the admitted context, never the partial output"
        );

        if max_attempts == 2 {
            assert!(
                !mixed.result.is_context_overflow(),
                "overflow must remain unsent at the cap"
            );
            assert_eq!(
                attempts[1].retry_decision,
                Some(RetryDecision::Declined(
                    RetryDeclineCause::RetryBudgetExhausted
                ))
            );
        } else {
            assert!(matches!(
                attempts[1].retry_decision,
                Some(RetryDecision::Scheduled {
                    class: RetryClass::ChargeAuthorized {
                        attempt_number: 1,
                        ..
                    },
                    ..
                })
            ));
            assert!(
                mixed.result.is_context_overflow(),
                "typed overflow ends the mixed call"
            );
            let rebuilt = send(&session, "recover after overflow").await;
            assert_eq!(
                rebuilt.assistant_message().unwrap_or_default(),
                "recovered answer"
            );
            let captured = server.captured();
            assert_eq!(
                captured.len(),
                7,
                "one bounded summary and one answer rebuild the frame"
            );
            assert!(captured[5]["input"].to_string().contains("committed seed"));
            assert!(captured[6]["input"].to_string().contains("rebuilt context"));
            assert!(!captured[6]["input"].to_string().contains("committed seed"));
            assert!(
                !captured[6]["input"]
                    .to_string()
                    .contains("uncommitted partial")
            );
            assert!(captured[6].get("previous_response_id").is_none());
            // Two host calls before overflow, at most one summary per
            // recovery allowance, and the next host call. Each uses the
            // recorded provider limit. The one rejected-cache resend is
            // the only wire request outside their visible attempt ledgers.
            let total_model_attempt_bound = max_attempts * (3 + overflow_max_attempts);
            assert!(
                captured.len() - 1 <= total_model_attempt_bound as usize,
                "mixed recovery cannot multiply or replenish recorded allowances"
            );
            assert!(
                rebuilt
                    .result
                    .llm_calls
                    .iter()
                    .all(|call| call.attempts.len() <= max_attempts as usize)
            );
        }
        core.shutdown().await.expect("core shuts down");
    }
}
