//! Request-boundary witnesses for initial instructions and positional feedback.
use lash_core::llm::types::{LlmMessage, LlmRequest, LlmRole};
use lash_core::provider::CacheRetention;
use lash_sansio::sync::MutexExt;
use serde_json::{Value, json};
use std::sync::Arc;

fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        instructions: Some(Arc::from("I")),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("host-selected-model")
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages,

        tools: Arc::new(vec![]),
        tool_choice: Default::default(),
        attachment_acceptance: Default::default(),
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
        req.model
            .metadata_mut()
            .capability
            .native_mid_conversation_system = matches!(self, Self::AnthropicNative);
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
wire_test!(runtime_feedback_chat, Chat);
wire_test!(runtime_feedback_gemini, Gemini);

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
        req.model.metadata_mut().capability.instruction_role =
            lash::provider::InstructionRole::Developer;
        let body = wire.body(&req);
        let messages = flattened(wire.messages(&body));
        assert_eq!(messages.last().unwrap(), &("developer".into(), "F".into()));
        if matches!(wire, Wire::Chat) {
            assert_eq!(messages[0], ("developer".into(), "I".into()));
        }
    }
}

async fn captured_output_limit_retry() -> Vec<LlmRequest> {
    use std::collections::VecDeque;

    let captures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let responses = Arc::new(tokio::sync::Mutex::new(VecDeque::from([
        "partial output before truncation".to_string(),
        "<typescript>\nawait control.finish(42);\n</typescript>".to_string(),
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
    let engine = crate::backend::SimEngine::new(0x5eed_7003)
        .await
        .expect("sim engine");
    let backend = engine.backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
    );
    let core = lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder("cache-regression-model")
                .cache_retention(lash_core::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()
                .expect("cache regression model"),
        )
        .build(crate::sim_process_owner())
        .expect("RLM cache regression core");
    let session = crate::open_created_finish_required_session(
        "cache-regression-model",
        &core,
        "cache-regression-session",
    )
    .await
    .expect("RLM cache regression session");
    engine
        .run_turn(
            &session,
            "cache-regression-turn",
            Arc::new(crate::backend::DiscardedTurnActivity),
            Arc::new(|session: &lash::LashSession| {
                Ok(session.send(lash::TurnInput::text("increment a bound value twice")))
            }),
        )
        .await
        .expect("RLM cache regression handler")
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
                content: vec![lash_core::facade_support::ModelToolReturnPart::text(
                    format!("RESULT{i}"),
                )],
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
    runtime_feedback_tool_results_chat,
    Chat,
    feedback_tool_results
);
feedback_contract_test!(
    runtime_feedback_tool_results_gemini,
    Gemini,
    feedback_tool_results
);
