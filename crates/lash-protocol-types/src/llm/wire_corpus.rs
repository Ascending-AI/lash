//! The request histories every provider's lowering is pinned against: the
//! byte-identity law (FIG-6443) lowers each one and compares the exact body
//! with a digest recorded before lowering became incremental, and lowers
//! each prefix of the growing history through one segment cache to prove a
//! reused prefix sends the bytes a cold lowering sends.

use crate::llm::types::{
    LlmContentBlock, LlmMessage, LlmRole, ProviderReasoningReplay, ProviderReplayMeta,
    ProviderRouteIdentity, ResponseTextMeta,
};
use crate::{AttachmentId, AttachmentRef, MediaType, ModelToolReturnPart};

fn text(text: &str) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: None,
        cache_breakpoint: false,
    }
}

fn marked(text: &str) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: None,
        cache_breakpoint: true,
    }
}

#[expect(
    clippy::expect_used,
    reason = "the id and media type are fixed literals"
)]
fn image(seed: char) -> AttachmentRef {
    AttachmentRef {
        id: AttachmentId::parse(seed.to_string().repeat(64)).expect("attachment id"),
        media_type: MediaType::parse("image/png").expect("media type"),
        byte_len: 2048,
        type_metadata: None,
        label: None,
    }
}

fn message(role: LlmRole, blocks: Vec<LlmContentBlock>) -> LlmMessage {
    LlmMessage::new(role, blocks)
}

fn user(blocks: Vec<LlmContentBlock>) -> LlmMessage {
    let mut message = message(LlmRole::User, blocks);
    message.starts_user_segment = true;
    message
}

fn tool_call(id: &str, name: &str, input: &str, route: &ProviderRouteIdentity) -> LlmContentBlock {
    LlmContentBlock::ToolCall {
        call_id: id.into(),
        tool_name: name.into(),
        input_json: input.into(),
        replay: Some(ProviderReplayMeta {
            item_id: Some(format!("fc_{}", id.len())),
            opaque: None,
            origin: Some(route.clone()),
        }),
    }
}

fn tool_result(id: &str, content: Vec<ModelToolReturnPart>) -> LlmContentBlock {
    LlmContentBlock::ToolResult {
        status: crate::ToolCallStatus::Success,
        call_id: id.into(),
        content,
        tool_name: Some("read_file".into()),
    }
}

fn result_text(text: &str) -> ModelToolReturnPart {
    ModelToolReturnPart::Text { text: text.into() }
}

fn reasoning(text: &str, route: &ProviderRouteIdentity) -> LlmContentBlock {
    LlmContentBlock::Reasoning {
        text: text.into(),
        replay: Some(ProviderReasoningReplay {
            item_id: Some("rs_1".into()),
            encrypted_content: Some("ZW5jcnlwdGVk".into()),
            signature: Some("c2lnbmF0dXJl".into()),
            redacted: false,
            summary: vec![format!("summary of {text}")],
            origin: Some(route.clone()),
        }),
    }
}

fn answer(text: &str, id: &str, route: &ProviderRouteIdentity) -> LlmContentBlock {
    LlmContentBlock::Text {
        text: text.into(),
        response_meta: Some(ResponseTextMeta {
            id: Some(id.into()),
            status: Some("completed".into()),
            provider_payload: Some("c2ln".into()),
            origin: Some(route.clone()),
            ..ResponseTextMeta::default()
        }),
        cache_breakpoint: false,
    }
}

/// A long tool output: multi-line, above one 32 KiB stored chunk.
fn long_output() -> String {
    (0..1200)
        .map(|line| format!("line {line}: fn example_{line}() -> usize {{ {line} }}\n"))
        .collect()
}

/// One coding-style history, every message a feature some provider lowers
/// specially: reasoning with replay, interleaved text and tool calls (one
/// with an id Anthropic must map), tool results with text, attachments and
/// nothing, runtime feedback with and without attachments between a call
/// and its result, a cache breakpoint, whitespace-only text, a
/// reasoning-only message beside a same-role text message, and a long
/// multi-line result. Its prefixes are the growing history.
pub fn growing_history(route: &ProviderRouteIdentity) -> Vec<LlmMessage> {
    vec![
        user(vec![text("Read src/lib.rs and fix the failing test.")]),
        message(
            LlmRole::Assistant,
            vec![
                reasoning("I should read the file first.", route),
                answer("Reading the file.", "msg_1", route),
                tool_call("call_1", "read_file", r#"{"path":"src/lib.rs"}"#, route),
                tool_call("call:2/odd id", "grep", r#"{"pattern":"fn "}"#, route),
            ],
        ),
        message(
            LlmRole::System,
            vec![text("Tool output is truncated at 64 KiB.")],
        ),
        message(
            LlmRole::User,
            vec![
                tool_result("call_1", vec![result_text(&long_output())]),
                tool_result(
                    "call:2/odd id",
                    vec![
                        result_text("see the screenshot"),
                        ModelToolReturnPart::Attachment(image('b')),
                    ],
                ),
            ],
        ),
        message(
            LlmRole::Assistant,
            vec![reasoning("The test asserts the wrong value.", route)],
        ),
        message(
            LlmRole::Assistant,
            vec![
                answer("The assertion is wrong; editing it.", "msg_2", route),
                tool_call(
                    "call_3",
                    "edit_file",
                    r#"{"path":"src/lib.rs","line":12}"#,
                    route,
                ),
            ],
        ),
        message(LlmRole::User, vec![tool_result("call_3", Vec::new())]),
        message(
            LlmRole::System,
            vec![
                text("A screenshot of the failing run is attached."),
                LlmContentBlock::Attachment {
                    reference: Box::new(image('c')),
                },
            ],
        ),
        user(vec![
            text("   "),
            marked("Also run the whole suite."),
            LlmContentBlock::Attachment {
                reference: Box::new(image('d')),
            },
        ]),
        message(
            LlmRole::Assistant,
            vec![
                tool_call("call_4", "run_tests", "{}", route),
                tool_call("call_5", "run_tests", r#"{"filter":"lib"}"#, route),
            ],
        ),
        message(
            LlmRole::System,
            vec![text("The suite is slow; be patient.")],
        ),
        message(
            LlmRole::User,
            vec![
                tool_result("call_4", vec![result_text("ok: 112 passed")]),
                tool_result("call_5", vec![result_text("ok: 9 passed")]),
            ],
        ),
        message(
            LlmRole::Assistant,
            vec![answer("All tests pass.", "msg_3", route)],
        ),
        user(vec![text("Thanks. Summarize the change.")]),
    ]
}

/// Named histories: each prefix of [`growing_history`] and a few shapes it
/// does not cover.
pub fn wire_corpus(route: &ProviderRouteIdentity) -> Vec<(String, Vec<LlmMessage>)> {
    let history = growing_history(route);
    let mut corpus: Vec<(String, Vec<LlmMessage>)> = (1..=history.len())
        .map(|len| (format!("prefix-{len:02}"), history[..len].to_vec()))
        .collect();
    corpus.push(("empty-history".into(), Vec::new()));
    corpus.push((
        "only-feedback".into(),
        vec![message(LlmRole::System, vec![text("Only feedback.")])],
    ));
    corpus.push((
        "unicode".into(),
        vec![user(vec![text(
            "Ünïcödé \u{1F600} \"quoted\" \\ back\u{0007}slash\n\ttab",
        )])],
    ));
    corpus
}

/// One recorded line of the byte-identity law: the body's serialized
/// template (route, mode, receipt, features, literals and slots) by digest,
/// or the refusal.
pub fn digest_line(
    name: &str,
    lowered: Result<&crate::llm::types::RecordedRequestTemplate, String>,
) -> String {
    match lowered {
        Ok(template) => {
            let bytes = serde_json::to_vec(template).unwrap_or_default();
            let digest = blake3::hash(&bytes).to_hex();
            format!("{name} {}", &digest[..24])
        }
        Err(error) => format!("{name} refused: {error}"),
    }
}

/// A tool every variant with tools offers.
#[expect(clippy::expect_used, reason = "the schemas are fixed object literals")]
pub fn corpus_tool() -> crate::llm::types::LlmToolSpec {
    crate::llm::types::LlmToolSpec {
        name: "read_file".into(),
        description: "Read a file.".into(),
        input_schema: lash_vocabulary::schema_contract::SchemaContract::admit(serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }))
        .expect("valid declared schema"),
        output_schema: lash_vocabulary::schema_contract::SchemaContract::admit(
            lash_vocabulary::ToolDefinition::any_output(),
        )
        .expect("valid declared schema"),
    }
}

/// `messages` with every attachment dropped: the corpus for a route that
/// carries text only.
pub fn text_only(messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
    messages
        .into_iter()
        .map(|message| {
            let blocks = message
                .blocks
                .iter()
                .filter(|block| !matches!(block, LlmContentBlock::Attachment { .. }))
                .map(|block| match block {
                    LlmContentBlock::ToolResult {
                        status,
                        call_id,
                        content,
                        tool_name,
                    } => LlmContentBlock::ToolResult {
                        status: *status,
                        call_id: call_id.clone(),
                        content: content
                            .iter()
                            .filter(|part| part.attachment().is_none())
                            .cloned()
                            .collect(),
                        tool_name: tool_name.clone(),
                    },
                    other => other.clone(),
                })
                .collect();
            let mut text = LlmMessage::new(message.role, blocks);
            text.starts_user_segment = message.starts_user_segment;
            text
        })
        .collect()
}

/// Histories lowered in order through one segment cache, each compared
/// with a cold lowering: the invalidation laws (FIG-6443). A reused segment
/// never carries stale content when the history grows, when a message is
/// edited (its blocks are a new allocation), or when compaction replaces
/// the leading history and shifts every later message.
pub fn reuse_cases(route: &ProviderRouteIdentity) -> Vec<(&'static str, Vec<Vec<LlmMessage>>)> {
    let history = growing_history(route);
    let growing = (1..=history.len())
        .map(|len| history[..len].to_vec())
        .collect();
    let mut edited = history.clone();
    let mut changed = (*edited[3].blocks).clone();
    if let Some(LlmContentBlock::ToolResult { content, .. }) = changed.first_mut() {
        *content = vec![result_text("line 0: edited by the host\n")];
    }
    edited[3] = LlmMessage::new(edited[3].role.clone(), changed);
    let mut renamed = history.clone();
    renamed[0] = user(vec![text("Read src/main.rs and fix the failing test.")]);
    let mut compacted = vec![user(vec![text(
        "<summary>The test asserted the wrong value; it was fixed.</summary>",
    )])];
    compacted.extend(history[8..].iter().cloned());
    let mut compacted_again = compacted.clone();
    compacted_again.push(user(vec![text("And now the docs.")]));
    vec![
        ("growing", growing),
        (
            "edit",
            vec![history.clone(), edited, history.clone(), renamed],
        ),
        (
            "compaction",
            vec![history.clone(), compacted, compacted_again, history],
        ),
    ]
}
