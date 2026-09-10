//! Request-boundary witnesses for initial instructions and positional feedback.
use lash_core::llm::types::{LlmMessage, LlmRequest, LlmRole};
use lash_core::provider::CacheRetention;
use serde_json::{Value, json};
use std::sync::Arc;

fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        instructions: Some(Arc::from("I")),
        model: "host-selected-model".into(),
        messages,
        resolved_stored: Default::default(),
        tools: Arc::new(vec![]),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: Default::default(),
        generation: Default::default(),
        scope: lash_core::LlmRequestScope::new("session", "frame", "request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

#[derive(Clone, Copy, Debug)]
enum Wire {
    Responses,
    Codex,
    Chat,
    AnthropicNative,
    AnthropicFallback,
    Gemini,
    CodeAssist,
}
impl Wire {
    fn body(self, req: &LlmRequest) -> Value {
        let mut req = req.clone();
        req.model_capability.native_mid_conversation_system = matches!(self, Self::AnthropicNative);
        match self {
            Self::Responses => lash_provider_openai::testing::serialize_responses_request(
                &req,
                CacheRetention::None,
            )
            .unwrap(),
            Self::Codex => {
                lash_provider_openai::testing::serialize_codex_request(&req, CacheRetention::None)
                    .unwrap()
            }
            Self::Chat => {
                lash_provider_openai::testing::serialize_chat_request(&req, CacheRetention::None)
                    .unwrap()
                    .0
            }
            Self::AnthropicNative | Self::AnthropicFallback => {
                lash_provider_anthropic::testing::serialize_request(&req, CacheRetention::None)
                    .unwrap()
            }
            Self::Gemini => {
                lash_provider_google::testing::serialize_request(&req, CacheRetention::None)
                    .unwrap()["request"]
                    .clone()
            }
            Self::CodeAssist => {
                lash_provider_google::testing::serialize_request(&req, CacheRetention::None)
                    .unwrap()
            }
        }
    }
    fn inner(self, body: &Value) -> &Value {
        if matches!(self, Self::CodeAssist) {
            &body["request"]
        } else {
            body
        }
    }
    fn messages(self, body: &Value) -> &Value {
        &self.inner(body)[match self {
            Self::Responses | Self::Codex => "input",
            Self::Gemini | Self::CodeAssist => "contents",
            _ => "messages",
        }]
    }
    fn assert_instructions(self, body: &Value, expected: Option<&str>) {
        let inner = self.inner(body);
        let value = match self {
            Self::Responses | Self::Codex => inner.get("instructions"),
            Self::AnthropicNative | Self::AnthropicFallback => {
                inner.get("system").map(|value| &value[0]["text"])
            }
            Self::Gemini | Self::CodeAssist => inner
                .get("systemInstruction")
                .map(|value| &value["parts"][0]["text"]),
            Self::Chat => {
                if let Some(expected) = expected {
                    assert_eq!(inner["messages"][0]["content"][0]["text"], expected);
                }
                return;
            }
        };
        match expected {
            Some(text) => assert_eq!(value, Some(&json!(text))),
            None if matches!(self, Self::Codex) => assert_eq!(value, Some(&json!(""))),
            None => assert!(value.is_none(), "{self:?}: {body}"),
        }
    }
    fn tagged(self) -> bool {
        matches!(
            self,
            Self::AnthropicNative | Self::AnthropicFallback | Self::Gemini | Self::CodeAssist
        )
    }
}

fn text(role: LlmRole, text: &str) -> LlmMessage {
    LlmMessage::text(role, text.to_owned())
}
fn flattened(messages: &Value) -> Vec<(String, String)> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| {
            let Some(role) = message["role"].as_str() else {
                return vec![];
            };
            let content = message.get("content").or_else(|| message.get("parts"));
            match content {
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| {
                        part["text"]
                            .as_str()
                            .map(|text| (role.to_owned(), text.to_owned()))
                    })
                    .collect(),
                Some(Value::String(text)) => vec![(role.to_owned(), text.clone())],
                _ => vec![],
            }
        })
        .collect()
}

fn witness(wire: Wire) {
    let req = request(vec![
        text(LlmRole::User, "U1"),
        text(LlmRole::Assistant, "partial"),
        text(LlmRole::System, "retry\n exact "),
        text(LlmRole::User, "U2"),
    ]);
    let body = wire.body(&req);
    wire.assert_instructions(&body, Some("I"));
    let mut actual = flattened(wire.messages(&body));
    if matches!(wire, Wire::Chat) {
        assert_eq!(actual.remove(0), ("system".into(), "I".into()));
    }
    let feedback = if wire.tagged() {
        "<runtime_feedback>retry\n exact </runtime_feedback>"
    } else {
        "retry\n exact "
    };
    assert_eq!(
        actual,
        vec![
            ("user".into(), "U1".into()),
            (
                if matches!(wire, Wire::Gemini | Wire::CodeAssist) {
                    "model"
                } else {
                    "assistant"
                }
                .into(),
                "partial".into()
            ),
            (
                if wire.tagged() { "user" } else { "system" }.into(),
                feedback.into()
            ),
            ("user".into(), "U2".into())
        ],
        "{wire:?}"
    );
    // Leading feedback, both with and without initial instructions (mutation pin).
    for instructions in [Some(Arc::from("I")), None] {
        let mut req = request(vec![
            text(LlmRole::System, "leading"),
            text(LlmRole::User, "U"),
        ]);
        req.instructions = instructions;
        let body = wire.body(&req);
        wire.assert_instructions(&body, req.instructions.as_deref());
        let mut actual = flattened(wire.messages(&body));
        if matches!(wire, Wire::Chat) && req.instructions.is_some() {
            actual.remove(0);
        }
        assert_eq!(
            actual,
            vec![
                (
                    if wire.tagged() { "user" } else { "system" }.into(),
                    if wire.tagged() {
                        "<runtime_feedback>leading</runtime_feedback>"
                    } else {
                        "leading"
                    }
                    .into()
                ),
                ("user".into(), "U".into())
            ]
        );
    }
}
macro_rules! wire_test {
    ($name:ident, $wire:ident) => {
        #[test]
        fn $name() {
            witness(Wire::$wire);
        }
    };
}
wire_test!(runtime_feedback_responses, Responses);
wire_test!(runtime_feedback_codex, Codex);
wire_test!(runtime_feedback_chat, Chat);
wire_test!(runtime_feedback_anthropic_native, AnthropicNative);
wire_test!(runtime_feedback_anthropic_fallback, AnthropicFallback);
wire_test!(runtime_feedback_gemini, Gemini);
wire_test!(runtime_feedback_code_assist, CodeAssist);

#[test]
fn runtime_feedback_anthropic_per_message_legality_and_coalescing() {
    let cases = [
        (
            vec![
                text(LlmRole::User, "U"),
                text(LlmRole::System, "F"),
                text(LlmRole::User, "U2"),
            ],
            json!([{"role":"user","content":[{"type":"text","text":"U"},{"type":"text","text":"<runtime_feedback>F</runtime_feedback>"},{"type":"text","text":"U2"}]}]),
        ),
        (
            vec![text(LlmRole::User, "U"), text(LlmRole::System, "F")],
            json!([{"role":"user","content":[{"type":"text","text":"U"}]},{"role":"system","content":[{"type":"text","text":"F"}]}]),
        ),
        (
            vec![
                text(LlmRole::User, "U"),
                text(LlmRole::Assistant, "A"),
                text(LlmRole::System, "F"),
            ],
            json!([{"role":"user","content":[{"type":"text","text":"U"}]},{"role":"assistant","content":[{"type":"text","text":"A"}]},{"role":"user","content":[{"type":"text","text":"<runtime_feedback>F</runtime_feedback>"}]}]),
        ),
        (
            vec![
                text(LlmRole::User, "U"),
                text(LlmRole::System, "F1"),
                text(LlmRole::System, "F2"),
                text(LlmRole::Assistant, "A"),
                text(LlmRole::System, "F3"),
                text(LlmRole::User, "U2"),
            ],
            json!([{"role":"user","content":[{"type":"text","text":"U"}]},{"role":"system","content":[{"type":"text","text":"F1"},{"type":"text","text":"F2"}]},{"role":"assistant","content":[{"type":"text","text":"A"}]},{"role":"user","content":[{"type":"text","text":"<runtime_feedback>F3</runtime_feedback>"},{"type":"text","text":"U2"}]}]),
        ),
    ];
    for (messages, expected) in cases {
        assert_eq!(
            Wire::AnthropicNative.body(&request(messages))["messages"],
            expected
        );
    }
    let req = request(vec![
        text(LlmRole::User, "U"),
        text(LlmRole::Assistant, "partial"),
        text(LlmRole::System, "retry"),
        text(LlmRole::User, "U2"),
    ]);
    assert_eq!(
        Wire::AnthropicNative.body(&req),
        Wire::AnthropicFallback.body(&req)
    );
}

#[test]
fn runtime_feedback_host_instruction_role_controls_all_openai_wires() {
    for wire in [Wire::Responses, Wire::Codex, Wire::Chat] {
        let mut req = request(vec![text(LlmRole::User, "U"), text(LlmRole::System, "F")]);
        req.model_capability.instruction_role = lash::provider::InstructionRole::Developer;
        let body = wire.body(&req);
        let messages = flattened(wire.messages(&body));
        assert_eq!(messages.last().unwrap(), &("developer".into(), "F".into()));
        if matches!(wire, Wire::Chat) {
            assert_eq!(messages[0], ("developer".into(), "I".into()));
        }
    }
}

use lash::rlm::RlmTurnBuilderExt;
use lash_sansio::sync::MutexExt;
async fn captured_output_limit_retry() -> Vec<LlmRequest> {
    use std::collections::VecDeque;

    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = Arc::new(tokio::sync::Mutex::new(VecDeque::from([
        "partial output before truncation".to_string(),
        "<lashlang>\nfinish 42\n</lashlang>".to_string(),
    ])));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("cache-regression-rlm")
        .complete({
            let captures = Arc::clone(&captures);
            move |request| {
                let captures = Arc::clone(&captures);
                let responses = Arc::clone(&responses);
                async move {
                    captures.lock_recover().push(request);
                    let text = responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("RLM response script");
                    let terminal_reason = if text == "partial output before truncation" {
                        lash_core::LlmTerminalReason::OutputLimit
                    } else {
                        lash_core::LlmTerminalReason::Stop
                    };
                    Ok(lash_core::LlmResponse {
                        terminal_reason,
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .with_native_queued_work()
        .effect_host(Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>)
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("cache-regression-model")
                .context_window_tokens(200_000)
                .build()
                .expect("cache regression model"),
        )
        .build(crate::sim_process_owner())
        .expect("RLM cache regression core");
    let session = core
        .session("cache-regression-session")
        .open()
        .await
        .expect("RLM cache regression session");
    session
        .turn(lash::TurnInput::text("increment a bound value twice"))
        .require_finish()
        .expect("finish-required RLM turn")
        .run()
        .await
        .expect("RLM cache regression turn");

    captures.lock_recover().clone()
}

#[tokio::test]
async fn runtime_feedback_real_rlm_output_limit_retry_reaches_provider_wires() {
    let requests = captured_output_limit_retry().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].instructions.is_some());
    assert_eq!(requests[0].instructions, requests[1].instructions);
    let second = &requests[1];
    let feedback_index = second.messages.iter().position(|message| message.role == LlmRole::System && message.blocks.iter().any(|block| matches!(block, lash_core::llm::types::LlmContentBlock::Text { text, .. } if text.contains("Your answer was cut off by the output limit")))).expect("real finish.rs retry feedback");
    assert_eq!(second.messages[feedback_index - 1].role, LlmRole::Assistant);
    for wire in [
        Wire::Responses,
        Wire::Codex,
        Wire::Chat,
        Wire::AnthropicNative,
        Wire::AnthropicFallback,
        Wire::Gemini,
        Wire::CodeAssist,
    ] {
        let body = wire.body(second);
        wire.assert_instructions(&body, second.instructions.as_deref());
        let messages = flattened(wire.messages(&body));
        let partial = messages
            .iter()
            .position(|(_, text)| text.contains("partial output before truncation"))
            .expect("assistant partial at provider boundary");
        let feedback = messages
            .iter()
            .position(|(_, text)| text.contains("Your answer was cut off by the output limit"))
            .expect("retry at provider boundary");
        assert!(partial < feedback, "{wire:?}");
        assert!(
            !second
                .instructions
                .as_deref()
                .unwrap()
                .contains("Your answer was cut off by the output limit")
        );
    }
}

struct FeedbackPlugin;
impl lash_core::plugin::PluginFactory for FeedbackPlugin {
    fn id(&self) -> &'static str {
        "runtime-feedback-witness"
    }
    fn build(
        &self,
        ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spec = lash_core::plugin::PluginSpec::new().with_checkpoint(Arc::new(move |ctx| {
            let injected = injected.clone();
            Box::pin(async move {
                if ctx.checkpoint == lash_core::CheckpointKind::BeforeCompletion
                    && !injected.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    Ok(vec![
                        lash_core::plugin::TurnPluginDirective::EnqueueMessages(
                            lash_core::plugin::EnqueueMessagesDirective {
                                messages: vec![lash_core::PluginMessage::text(
                                    lash_core::MessageRole::System,
                                    "checkpoint feedback nonce 2505",
                                )],
                            },
                        ),
                    ])
                } else {
                    Ok(vec![])
                }
            })
        }));
        lash_core::plugin::PluginFactory::build(
            &lash_core::plugin::StaticPluginFactory::new(self.id(), spec),
            ctx,
        )
    }
}
async fn captured_standard_feedback(checkpoint: bool) -> Vec<LlmRequest> {
    use std::collections::VecDeque;

    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = Arc::new(tokio::sync::Mutex::new(VecDeque::from([
        "first".to_string(),
        "last".to_string(),
    ])));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("cache-regression-rlm")
        .complete({
            let captures = Arc::clone(&captures);
            move |request| {
                let captures = Arc::clone(&captures);
                let responses = Arc::clone(&responses);
                async move {
                    captures.lock_recover().push(request);
                    let text = responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("RLM response script");
                    let parts = if !checkpoint && text == "first" {
                        vec![lash_core::LlmOutputPart::ToolCall {
                            call_id: "call-1".into(),
                            tool_name: "missing_tool".into(),
                            input_json: "{}".into(),
                            replay: None,
                        }]
                    } else {
                        vec![lash_core::LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }]
                    };
                    Ok(lash_core::LlmResponse {
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        parts,
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut builder = lash::LashCore::standard_builder(if checkpoint {
        lash::TurnBudget::Unbounded
    } else {
        lash::TurnBudget::bounded(1)
    });
    if checkpoint {
        builder = builder.plugin(Arc::new(FeedbackPlugin));
    }
    let core = builder
        .with_native_queued_work()
        .effect_host(Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>)
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("cache-regression-model")
                .context_window_tokens(200_000)
                .build()
                .expect("cache regression model"),
        )
        .build(crate::sim_process_owner())
        .expect("RLM cache regression core");
    let session = core
        .session("cache-regression-session")
        .open()
        .await
        .expect("RLM cache regression session");
    session
        .turn(lash::TurnInput::text("increment a bound value twice"))
        .run()
        .await
        .expect("RLM cache regression turn");

    if !checkpoint {
        session
            .turn(lash::TurnInput::text("after limit"))
            .run()
            .await
            .expect("next user turn exposes the limit feedback");
    }
    captures.lock_recover().clone()
}

fn assert_captured_feedback(requests: &[LlmRequest], needle: &str) {
    assert_eq!(requests.len(), 2);
    assert!(requests[0].instructions.is_some());
    assert_eq!(requests[0].instructions, requests[1].instructions);
    let second = &requests[1];
    let index = second.messages.iter().position(|message| message.role == LlmRole::System && message.blocks.iter().any(|block| matches!(block, lash_core::llm::types::LlmContentBlock::Text { text, .. } if text.contains(needle)))).expect("runtime feedback in projected conversation");
    assert!(index > 0);
    assert!(!second.instructions.as_deref().unwrap().contains(needle));
    for wire in [
        Wire::Responses,
        Wire::Codex,
        Wire::Chat,
        Wire::AnthropicNative,
        Wire::AnthropicFallback,
        Wire::Gemini,
        Wire::CodeAssist,
    ] {
        let body = wire.body(second);
        wire.assert_instructions(&body, second.instructions.as_deref());
        let messages = flattened(wire.messages(&body));
        let feedback = messages
            .iter()
            .position(|(_, text)| text.contains(needle))
            .expect("feedback at provider boundary");
        assert!(feedback > 0, "{wire:?}");
        if let Some(after) = messages.iter().position(|(_, text)| text == "after limit") {
            assert!(feedback < after);
        }
    }
}
#[tokio::test]
async fn runtime_feedback_real_checkpoint_directive_reaches_provider_wires() {
    assert_captured_feedback(
        &captured_standard_feedback(true).await,
        "checkpoint feedback nonce 2505",
    );
}
#[tokio::test]
async fn runtime_feedback_real_standard_turn_limit_reaches_provider_wires() {
    assert_captured_feedback(
        &captured_standard_feedback(false).await,
        "Turn limit reached (1)",
    );
}

#[test]
fn runtime_feedback_anthropic_legality_uses_emitted_neighbors() {
    for messages in [
        vec![text(LlmRole::User, ""), text(LlmRole::System, "F")],
        vec![
            text(LlmRole::Assistant, "A"),
            text(LlmRole::User, " "),
            text(LlmRole::System, "F"),
        ],
        vec![
            text(LlmRole::User, "U"),
            text(LlmRole::System, "F"),
            text(LlmRole::Assistant, ""),
            text(LlmRole::User, "U2"),
        ],
    ] {
        let body = Wire::AnthropicNative.body(&request(messages));
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "system")
        );
        assert!(
            flattened(&body["messages"])
                .iter()
                .any(|(_, text)| text == "<runtime_feedback>F</runtime_feedback>")
        );
    }
}

#[test]
fn runtime_feedback_native_prefix_changes_when_a_later_user_makes_the_slot_illegal() {
    let first = request(vec![text(LlmRole::User, "U"), text(LlmRole::System, "F")]);
    let mut second = first.clone();
    second.messages.push(text(LlmRole::User, "later"));
    let before = Wire::AnthropicNative.body(&first);
    let after = Wire::AnthropicNative.body(&second);
    assert_eq!(before["system"], after["system"]);
    assert_eq!(before["messages"][1]["role"], "system");
    assert_eq!(
        after["messages"],
        json!([{"role":"user","content":[{"type":"text","text":"U"},{"type":"text","text":"<runtime_feedback>F</runtime_feedback>"},{"type":"text","text":"later"}]}])
    );
    assert_ne!(before["messages"][0], after["messages"][0]);
}

fn feedback_tool_results(wire: Wire) {
    use lash_core::llm::types::LlmContentBlock as B;
    for (count, split) in [(1, false), (2, false), (2, true)] {
        let calls = (0..count)
            .map(|i| B::ToolCall {
                call_id: format!("call{i}"),
                tool_name: "lookup".into(),
                input_json: "{}".into(),
                replay: None,
            })
            .collect::<Vec<_>>();
        let results = (0..count)
            .map(|i| B::ToolResult {
                call_id: format!("call{i}"),
                tool_name: Some("lookup".into()),
                content: format!("RESULT{i}"),
            })
            .collect::<Vec<_>>();
        let mut messages = vec![
            text(LlmRole::User, "U"),
            LlmMessage::new(LlmRole::Assistant, calls),
        ];
        if split {
            messages.push(LlmMessage::new(LlmRole::User, vec![results[0].clone()]));
        }
        messages.push(text(LlmRole::System, "F"));
        messages.push(LlmMessage::new(
            LlmRole::User,
            if split {
                vec![results[1].clone()]
            } else {
                results
            },
        ));
        messages.push(text(LlmRole::Assistant, "AFTER"));
        let body = wire.body(&request(messages));
        let serialized = wire.messages(&body).to_string();
        let feedback = serialized
            .find(if wire.tagged() {
                "<runtime_feedback>F</runtime_feedback>"
            } else {
                "\"F\""
            })
            .expect("feedback");
        for i in 0..count {
            assert!(
                serialized.find(&format!("RESULT{i}")).unwrap() < feedback,
                "{wire:?}, split={split}: {body}"
            );
        }
        assert!(feedback < serialized.find("AFTER").unwrap());
        if wire.tagged() {
            let turn = &wire.messages(&body)[2];
            assert_eq!(turn["role"], "user", "{wire:?}: {body}");
            let parts = turn
                .get("content")
                .or_else(|| turn.get("parts"))
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(
                parts.len(),
                count + 1,
                "feedback belongs in the result turn: {body}"
            );
            for part in &parts[..count] {
                assert!(
                    part["type"] == "tool_result" || part.get("functionResponse").is_some(),
                    "{body}"
                );
            }
            assert_eq!(
                parts[count]["text"],
                "<runtime_feedback>F</runtime_feedback>"
            );
        }
    }
}

fn feedback_image(wire: Wire) {
    use lash_core::llm::types::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
        AttachmentMimeSource, LlmContentBlock as B,
    };
    for role in [
        lash_core::InstructionRole::System,
        lash_core::InstructionRole::Developer,
    ] {
        for (bytes, encoded) in [(vec![1, 2, 3], "AQID"), (vec![4, 5, 6], "BAUG")] {
            let mut req = request(vec![
                text(LlmRole::User, "U"),
                LlmMessage::new(
                    LlmRole::System,
                    vec![
                        text(LlmRole::System, "F").blocks[0].clone(),
                        B::Attachment {
                            source: Box::new(lash_core::AttachmentSource::inline(
                                lash_core::MediaType::parse("image/png").unwrap(),
                                bytes,
                            )),
                        },
                    ],
                ),
                text(LlmRole::System, "NATIVE"),
                text(LlmRole::Assistant, "AFTER"),
            ]);
            req.model_capability.instruction_role = role;
            req.model_capability.attachment_acceptance = Arc::new(AttachmentCapabilitySnapshot {
                revision: "feedback-image".into(),
                acceptors: [
                    "OpenAI Responses",
                    "OpenAI Chat Completions",
                    "Anthropic Messages",
                    "Google Gemini",
                ]
                .into_iter()
                .map(|provider| AttachmentAcceptor {
                    provider: provider.into(),
                    rules: vec![AttachmentAcceptanceRule::Mime {
                        source: AttachmentMimeSource::Inline,
                        media_types: vec!["image/png".into()],
                        media_families: vec![],
                    }],
                })
                .collect(),
            });
            let body = wire.body(&req);
            wire.assert_instructions(&body, Some("I"));
            let messages = wire.messages(&body).as_array().unwrap();
            let message = messages
                .iter()
                .find(|m| {
                    m.to_string()
                        .contains("<runtime_feedback>F</runtime_feedback>")
                })
                .expect("image feedback must be tagged");
            assert_eq!(message["role"], "user");
            assert!(
                message.to_string().contains(encoded),
                "attachment bytes lost on {wire:?}: {body}"
            );
            let serialized = messages.iter().map(Value::to_string).collect::<String>();
            assert!(serialized.find(encoded).unwrap() < serialized.find("AFTER").unwrap());
            if matches!(wire, Wire::Responses | Wire::Codex | Wire::Chat) {
                assert!(
                    flattened(wire.messages(&body))
                        .contains(&(role.as_str().into(), "NATIVE".into())),
                    "only the attachment-bearing feedback downgrades"
                );
            }
        }
    }
}
macro_rules! feedback_contract_test {
    ($name:ident, $wire:ident, $witness:ident) => {
        #[test]
        fn $name() {
            $witness(Wire::$wire);
        }
    };
}
feedback_contract_test!(
    runtime_feedback_tool_results_responses,
    Responses,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_codex,
    Codex,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_chat,
    Chat,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_anthropic_native,
    AnthropicNative,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_anthropic_fallback,
    AnthropicFallback,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_gemini,
    Gemini,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_code_assist,
    CodeAssist,
    feedback_tool_results
);
feedback_contract_test!(runtime_feedback_image_responses, Responses, feedback_image);
feedback_contract_test!(runtime_feedback_image_codex, Codex, feedback_image);
feedback_contract_test!(runtime_feedback_image_chat, Chat, feedback_image);
feedback_contract_test!(
    runtime_feedback_image_anthropic_native,
    AnthropicNative,
    feedback_image
);
feedback_contract_test!(
    runtime_feedback_image_anthropic_fallback,
    AnthropicFallback,
    feedback_image
);
feedback_contract_test!(runtime_feedback_image_gemini, Gemini, feedback_image);
feedback_contract_test!(
    runtime_feedback_image_code_assist,
    CodeAssist,
    feedback_image
);

#[test]
fn runtime_feedback_unresolved_attachment_errors_retain_message_index() {
    use lash_core::llm::types::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
        AttachmentMimeSource, LlmContentBlock,
    };
    let source = lash_core::AttachmentSource::stored(lash_core::AttachmentRef {
        id: lash_core::AttachmentId::parse("feedback-image").unwrap(),
        media_type: lash_core::MediaType::parse("image/png").unwrap(),
        byte_len: 3,
        type_metadata: None,
        label: None,
    });
    let mut req = request(vec![
        text(LlmRole::User, "U"),
        LlmMessage::new(
            LlmRole::System,
            vec![LlmContentBlock::Attachment {
                source: Box::new(source),
            }],
        ),
    ]);
    req.model_capability.attachment_acceptance = Arc::new(AttachmentCapabilitySnapshot {
        revision: "feedback-stored".into(),
        acceptors: [
            "OpenAI Responses",
            "OpenAI Chat Completions",
            "Anthropic Messages",
            "Google Gemini",
        ]
        .into_iter()
        .map(|provider| AttachmentAcceptor {
            provider: provider.into(),
            rules: vec![AttachmentAcceptanceRule::Mime {
                source: AttachmentMimeSource::Stored,
                media_types: vec!["image/png".into()],
                media_families: vec![],
            }],
        })
        .collect(),
    });
    for native in [false, true] {
        req.model_capability.native_mid_conversation_system = native;
        for error in [
            lash_provider_openai::testing::serialize_responses_request(&req, CacheRetention::None)
                .unwrap_err(),
            lash_provider_openai::testing::serialize_codex_request(&req, CacheRetention::None)
                .unwrap_err(),
            lash_provider_openai::testing::serialize_chat_request(&req, CacheRetention::None)
                .unwrap_err(),
            lash_provider_anthropic::testing::serialize_request(&req, CacheRetention::None)
                .unwrap_err(),
            lash_provider_google::testing::serialize_request(&req, CacheRetention::None)
                .unwrap_err(),
        ] {
            assert_eq!(error.kind, lash_core::ProviderFailureKind::Validation);
            assert_eq!(
                error.code.as_deref(),
                Some("stored_attachment_not_resolved")
            );
            assert!(error.message.contains("message index 1"), "{error}");
        }
    }
}
