//! Content laws L4–L7, L20, L21(c), L22(a) and L24.
use super::*;
use crate::*;
use serde_json::{Value, json};

// Tests that consume owned JSON explicitly materialize the borrowed export view.
fn attributes(event: &TraceEvent) -> Vec<(&'static str, Value)> {
    super::attributes(event)
        .map(|(key, value)| (key, serde_json::to_value(value).unwrap()))
        .collect()
}

pub(crate) fn model_event() -> TraceEvent {
    serde_json::from_value(json!({
        "type":"llm_call_completed",
        "request":{"model":"m","system_instructions":"system secret","composition_fingerprint":"fingerprint",
            "messages":[{"role":"user","blocks":[{"kind":"text","text":"input secret"},{"kind":"reasoning","text":"input reason","item_id":null,"summary":[],"has_encrypted":false,"redacted":false}]}],
            "tools":[{"name":"tool","description":"tool secret","input_schema":{"type":"object"},"output_schema":{}}],
            "tool_choice":"auto","stream":true,"output_spec":{"type":"json_schema","name":"answer","schema":{"type":"object"}}},
        "response":{"text":"output secret","duration_ms":1,"request_model":"m","parts":[{"Text":{"text":"output secret","response_meta":null}},{"Reasoning":{"text":"output reason","replay":null}}]},
        "response_metadata":{"body:/cost":"metadata secret"}
    })).unwrap()
}
fn record(event: TraceEvent) -> TraceRecord {
    TraceRecord::host_owned(Default::default(), event).unwrap()
}
fn tool(value: Value) -> TraceEvent {
    TraceEvent::ToolCallCompleted {
        call_id: lash_protocol_types::ToolCallId::fixture("call"),
        provider_call_id: None,
        name: "tool".into(),
        args: json!([1, 2]).into(),
        output: TraceToolCallOutput {
            outcome: TraceToolCallOutcome::Success(value),
            control: None,
        },
        duration_ms: 1,
        issuing_node_id: None,
        attempts: None,
        span_attributes: Default::default(),
    }
}

#[test]
fn categories_require_direction_and_reasoning_consent() {
    for input in [Capture::Omitted, Capture::Captured] {
        for output in [Capture::Omitted, Capture::Captured] {
            for reasoning in [Capture::Omitted, Capture::Captured] {
                let policy = ContentPolicy::omitted()
                    .with(ContentCategory::InputMessages, input)
                    .with(ContentCategory::OutputMessages, output)
                    .with(ContentCategory::Reasoning, reasoning);
                let record = record(model_event()).governed(policy);
                let text = serde_json::to_string(&record).unwrap();
                assert_eq!(
                    text.contains("input reason"),
                    input == Capture::Captured && reasoning == Capture::Captured
                );
                assert_eq!(
                    text.contains("output reason"),
                    output == Capture::Captured && reasoning == Capture::Captured
                );
                assert_eq!(text.contains("input secret"), input == Capture::Captured);
                assert_eq!(text.contains("output secret"), output == Capture::Captured);
                assert!(!text.contains("system secret") && !text.contains("metadata secret"));
                let stream:TraceEvent=serde_json::from_value(json!({"type":"runtime_stream_event","event":{"sequence":0,"elapsed_ms":0,"payload":{"type":"reasoning_part","text":"stream reason","item_id":null}}})).unwrap();
                let stream = TraceRecord::host_owned(Default::default(), stream)
                    .unwrap()
                    .governed(policy);
                assert_eq!(
                    serde_json::to_string(&stream)
                        .unwrap()
                        .contains("stream reason"),
                    output == Capture::Captured && reasoning == Capture::Captured
                );
            }
        }
    }
    for system in [Capture::Omitted, Capture::Captured] {
        let record = record(model_event())
            .governed(ContentPolicy::omitted().with(ContentCategory::SystemInstructions, system));
        let attrs = attributes(&record.event);
        assert_eq!(
            attrs
                .iter()
                .find(|(key, _)| *key == "gen_ai.system_instructions")
                .map(|(_, value)| value.clone()),
            (system == Capture::Captured)
                .then(|| json!([{"type":"text","content":"system secret"}]))
        );
        assert!(!attrs.iter().any(|(key, _)| *key == "gen_ai.input.messages"));
    }
    let record = record(model_event())
        .governed(ContentPolicy::captured().with(ContentCategory::OutputSchema, Capture::Omitted));
    let TraceEvent::LlmCallCompleted { request, .. } = record.event else {
        panic!()
    };
    assert!(request.output_spec.as_ref().unwrap()["schema"].is_null());
    assert_eq!(request.output_spec.as_ref().unwrap()["name"], "answer");
    assert!(!request.tools[0].input_schema.is_null());
    assert_eq!(
        request.system_instructions.as_deref(),
        Some("system secret")
    );
    assert_eq!(request.composition_fingerprint, "fingerprint");
    for encrypted in [false, true] {
        let mut event = model_event();
        if let TraceEvent::LlmCallCompleted { response, .. } = &mut event {
            response.parts = Some(vec![
                lash_protocol_types::llm::types::LlmOutputPart::Reasoning {
                    text: "private reasoning".into(),
                    replay: Some(lash_protocol_types::llm::types::ProviderReasoningReplay {
                        redacted: !encrypted,
                        encrypted_content: encrypted.then(|| "opaque".into()),
                        ..Default::default()
                    }),
                },
            ]);
        }
        assert!(
            !serde_json::to_string(&attributes(&event))
                .unwrap()
                .contains("private reasoning")
        );
    }
}

struct Rewrite;
fn redact(value: &mut Value) {
    match value {
        Value::String(s) => *s = s.replace("secret", "REDACTED"),
        Value::Array(v) => v.iter_mut().for_each(redact),
        Value::Object(v) => v.values_mut().for_each(redact),
        _ => {}
    }
}
impl ContentHook for Rewrite {
    fn on_content(&self, _: &ContentView<'_>, content: &mut RecordContent<'_>) {
        content.for_each(|field| match field.value() {
            ContentValue::Text(s) => *s = s.replace("secret", "REDACTED"),
            ContentValue::Json(v) => redact(v),
        });
        content
            .reference("app.content.ref".into(), "uploaded".into())
            .unwrap();
    }
}
struct Panic;
impl ContentHook for Panic {
    fn on_content(&self, _: &ContentView<'_>, content: &mut RecordContent<'_>) {
        content.messages().unwrap().clear();
        content
            .reference("app.content.ref".into(), "partial".into())
            .unwrap();
        panic!("host failed after redaction");
    }
}
#[test]
fn hook_rewrites_every_captured_field_and_discards_a_panicked_transaction() {
    let policy = TracePolicySnapshot {
        content: ContentPolicy::captured(),
        ..Default::default()
    };
    let mut rewritten = record(model_event());
    assert!(rewritten.apply_content(&policy, Some(&Rewrite)).is_none());
    let encoded = serde_json::to_string(&rewritten).unwrap();
    assert!(!encoded.contains("secret"));
    assert!(encoded.contains("REDACTED"));
    assert_eq!(rewritten.content.references.len(), 1);
    // ContentPolicyAndHookEquivalence: an invalid typed replacement defaults
    // to omission; it must never recover the original attachment reference.
    struct InvalidAttachment;
    impl ContentHook for InvalidAttachment {
        fn on_content(&self, _: &ContentView<'_>, content: &mut RecordContent<'_>) {
            content.for_each(|field| {
                if field.path() == "input_messages[0].parts[2]" {
                    let ContentValue::Json(value) = field.value() else {
                        panic!()
                    };
                    *value = json!(false);
                }
            });
        }
    }
    let mut event = model_event();
    if let TraceEvent::LlmCallCompleted { request, .. } = &mut event {
        request.messages[0]
            .blocks
            .push(TraceContentBlock::Attachment {
                reference: Box::new(TraceAttachment {
                    id: "secret-reference".into(),
                    ..Default::default()
                }),
            });
    }
    let mut invalid = record(event);
    invalid.apply_content(&policy, Some(&InvalidAttachment));
    let TraceEvent::LlmCallCompleted { request, .. } = &invalid.event else {
        panic!()
    };
    assert_eq!(
        request.messages[0].blocks[2],
        TraceContentBlock::Attachment {
            reference: Box::default(),
        }
    );

    let mut failed = record(model_event());
    assert!(failed.apply_content(&policy, Some(&Panic)).is_some());
    assert!(
        failed.content.hook_failed
            && failed.content.captured.is_empty()
            && failed.content.references.is_empty()
    );
    let encoded = serde_json::to_string(&failed).unwrap();
    for content in ["secret", "input reason", "output reason", "partial"] {
        assert!(!encoded.contains(content), "{encoded}");
    }
    assert_eq!(failed.omissions.get(TraceOmission::HookFailed), 1);
}

#[test]
fn truncation_terminates_for_large_strings_numeric_arrays_messages_and_schemas() {
    let limits = ContentLimits::standard();
    // Encoded limits include escaping; cuts retain complete UTF-8 characters.
    let escaped = record(tool(json!({"large": "\\\"\n\u{0000}é🦀".repeat(50000)})))
        .governed(ContentPolicy::captured());
    for (_, value) in attributes(&escaped.event) {
        assert!(serde_json::to_vec(&value).unwrap().len() as u64 <= limits.max_attribute_bytes());
    }
    let max = limits.max_attribute_bytes() as usize;
    let mut string =
        record(tool(json!({"large":"é".repeat(1024*1024)}))).governed(ContentPolicy::captured());
    assert!(!string.content.truncated.is_empty());
    let TraceEvent::ToolCallCompleted { output, .. } = &string.event else {
        panic!()
    };
    let TraceToolCallOutcome::Success(value) = &output.outcome else {
        panic!()
    };
    assert!(value["large"].as_str().unwrap().len() <= limits.max_part_bytes() as usize);
    let numeric = record(tool(json!(vec![123456; 350000]))).governed(ContentPolicy::captured());
    assert!(!numeric.content.oversize.is_empty());
    let TraceEvent::ToolCallCompleted { output, .. } = numeric.event else {
        panic!()
    };
    assert_eq!(output.outcome, TraceToolCallOutcome::Success(Value::Null));
    let mut event = model_event();
    if let TraceEvent::LlmCallCompleted { request, .. } = &mut event {
        request.messages[0].blocks = vec![
            TraceContentBlock::Text {
                text: "x".into(),
                cache_breakpoint: false
            };
            20000
        ];
        request.tools[0].input_schema = json!({"enum":["s".repeat(20_000)]});
    }
    let bounded = record(event).governed(ContentPolicy::captured());
    let TraceEvent::LlmCallCompleted { request, .. } = &bounded.event else {
        panic!()
    };
    assert!(request.messages.is_empty());
    assert_eq!(
        request.tools[0].input_schema["enum"][0]
            .as_str()
            .unwrap()
            .len(),
        20000
    );
    for (_, value) in attributes(&bounded.event) {
        assert!(serde_json::to_vec(&value).unwrap().len() <= max);
    }
    let mut event = model_event();
    if let TraceEvent::LlmCallCompleted { request, .. } = &mut event {
        request.tools = vec![
            TraceToolSpec {
                name: "tool".into(),
                description: "x".repeat(3000),
                input_schema: json!({}),
                output_schema: json!({})
            };
            200
        ];
    }
    let bounded = record(event).governed(ContentPolicy::captured());
    assert!(
        bounded
            .content
            .oversize
            .contains(&"tool_definitions".to_owned())
    );
    let TraceEvent::LlmCallCompleted { request, .. } = &bounded.event else {
        panic!()
    };
    assert_eq!(
        request.tools.len(),
        200,
        "tool identities survive content bounds"
    );
    assert!(request.tools.iter().all(|tool| tool.name == "tool"));

    // Multi-message bounds discard the oldest whole messages, preserving order.
    let mut event = model_event();
    if let TraceEvent::LlmCallCompleted { request, .. } = &mut event {
        request.messages = (0..32)
            .map(|index| TraceLlmMessage {
                role: "user".into(),
                blocks: vec![TraceContentBlock::Text {
                    text: format!("{index}:{}", "x".repeat(8000)),
                    cache_breakpoint: false,
                }],
            })
            .collect();
    }
    let bounded = record(event).governed(ContentPolicy::captured());
    let TraceEvent::LlmCallCompleted { request, .. } = &bounded.event else {
        panic!()
    };
    assert!(bounded.content.dropped_messages > 0);
    assert_eq!(
        request.messages.len() + bounded.content.dropped_messages as usize,
        32
    );
    let TraceContentBlock::Text { text, .. } = &request.messages[0].blocks[0] else {
        panic!()
    };
    assert!(text.starts_with(&format!("{}:", bounded.content.dropped_messages)));

    // The error wrapper alone can push otherwise fitting diagnostics over the
    // attribute bound. Dropping them must retain the failure's classification.
    let mut diagnostics = json!({"message":"failed", "values":vec![0; max / 2 - 1024]});
    let padding = max - 5 - serde_json::to_vec(&diagnostics).unwrap().len() - 13;
    diagnostics["padding"] = json!("x".repeat(padding));
    assert_eq!(serde_json::to_vec(&diagnostics).unwrap().len(), max - 5);
    let classified = crate::TraceToolFailure::new(
        &lash_protocol_types::ToolFailure::io("host_io_failed", "failed"),
        ContentPolicy::standard(),
    );
    let mut failed = tool(Value::Null);
    if let TraceEvent::ToolCallCompleted { output, .. } = &mut failed {
        output.outcome = TraceToolCallOutcome::Failure(crate::TraceToolFailure {
            diagnostics: Some(diagnostics),
            ..classified.clone()
        });
    }
    let bounded = record(failed).governed(ContentPolicy::captured());
    assert!(
        bounded
            .content
            .oversize
            .contains(&"gen_ai.tool.call.result".to_owned())
    );
    let TraceEvent::ToolCallCompleted { output, .. } = bounded.event else {
        panic!()
    };
    assert_eq!(
        output.outcome,
        TraceToolCallOutcome::Failure(classified),
        "the classification survives and the diagnostics are dropped"
    );
    string.event.visit_content(&mut |field| {
        let n = match field.value() {
            ContentValue::Text(s) => serde_json::to_vec(s).unwrap().len(),
            ContentValue::Json(v) => serde_json::to_vec(v).unwrap().len(),
        };
        assert!(n <= max);
    });
}

#[test]
fn selector_skips_children_first_and_cannot_widen_inherited_narrowing() {
    let policy = TracePolicySnapshot {
        content: ContentPolicy::captured(),
        ..Default::default()
    };
    let mut narrow = TraceNarrowing::new(&policy);
    assert!(matches!(
        narrow.skip(TraceSpanKind::Agent),
        Err(TracePolicyError::Structural {
            field: TracePolicyField::Selector(TraceSpanKind::Agent),
            ..
        })
    ));
    assert!(matches!(
        narrow.skip(TraceSpanKind::Extension),
        Err(TracePolicyError::Relation {
            relation: PolicyRelation::ParentOffChildOn,
            ..
        })
    ));
    assert!(narrow.builds(TraceSpanKind::Extension));
    for kind in [
        TraceSpanKind::Point,
        TraceSpanKind::ModelAttempt,
        TraceSpanKind::Model,
        TraceSpanKind::Extension,
    ] {
        narrow.skip(kind).unwrap();
    }
    narrow.omit(ContentCategory::InputMessages);
    let child = narrow;
    assert!(!child.builds(TraceSpanKind::ModelAttempt));
    assert!(!child.content().captures(ContentCategory::InputMessages));
}

struct References;
impl ContentHook for References {
    fn on_content(&self, _: &ContentView<'_>, content: &mut RecordContent<'_>) {
        for i in 0..100 {
            let result = content.reference(format!("app.ref.{i}").into(), "key".into());
            if i >= 16 {
                assert_eq!(result, Err(TraceOmission::References));
            } else {
                result.unwrap();
            }
        }
    }
}
#[test]
fn references_are_charged_before_buffering_and_share_the_span_budget() {
    let mut record = record(model_event());
    let policy = TracePolicySnapshot {
        content: ContentPolicy::captured(),
        ..Default::default()
    };
    record.apply_content(&policy, Some(&References));
    assert_eq!(record.content.references.len(), 16);
    assert_eq!(record.omissions.get(TraceOmission::References), 84);
    let mut writer = TraceAttributeWriter::new(TraceSpanKind::Model, &policy.limits);
    let initial = writer.remaining();
    for (key, value) in record.content.references {
        writer.set(key, value).unwrap();
    }
    assert_eq!(writer.remaining().attributes, initial.attributes - 16);
    assert!(writer.remaining().bytes < initial.bytes);
}

#[test]
fn omitted_categories_never_evaluate_their_builders() {
    let mut built = 0;
    for category in ContentCategory::ALL {
        let _: String = ContentPolicy::omitted().capture(category, || {
            built += 1;
            "secret".into()
        });
    }
    assert_eq!(built, 0);
}

/// L7: each structured attribute validates against its pinned semconv schema.
#[test]
fn content_shapes_validate_against_the_pinned_semconv_schemas() {
    let schemas = [
        (
            "gen_ai.input.messages",
            include_str!("../../tests/semconv/gen-ai-input-messages.json"),
        ),
        (
            "gen_ai.output.messages",
            include_str!("../../tests/semconv/gen-ai-output-messages.json"),
        ),
        (
            "gen_ai.system_instructions",
            include_str!("../../tests/semconv/gen-ai-system-instructions.json"),
        ),
        (
            "gen_ai.tool.definitions",
            include_str!("../../tests/semconv/gen-ai-tool-definitions.json"),
        ),
        (
            "gen_ai.tool.call.arguments",
            include_str!("../../tests/semconv/gen-ai-tool-call-arguments.json"),
        ),
        (
            "gen_ai.tool.call.result",
            include_str!("../../tests/semconv/gen-ai-tool-call-result.json"),
        ),
    ];
    for event in [
        model_event(),
        tool(Value::Null),
        tool(json!(42)),
        tool(json!([1, 2])),
    ] {
        let record = record(event).governed(ContentPolicy::captured());
        for (key, value) in attributes(&record.event) {
            let schema: Value =
                serde_json::from_str(schemas.iter().find(|(name, _)| *name == key).unwrap().1)
                    .unwrap();
            let meta: Value =
                serde_json::from_str(include_str!("../../tests/semconv/draft-07.schema.json"))
                    .unwrap();
            let registry = jsonschema::Registry::new()
                .add("http://json-schema.org/draft-07/schema", &meta)
                .unwrap()
                .add("urn:lash:semconv", &schema)
                .unwrap()
                .prepare()
                .unwrap();
            let validator = jsonschema::options()
                .with_base_uri("urn:lash:semconv")
                .with_registry(&registry)
                .build(&schema)
                .unwrap();
            assert!(validator.is_valid(&value), "{key}: {value}");
        }
    }
}

/// A turn's exported input obeys the same capture, hook and byte limits
/// as model input, using the one exhaustive content visitor.
#[test]
fn turn_closer_input_obeys_capture_redaction_and_bounds() {
    let event = || TraceEvent::TurnCompleted {
        profile: None,
        output: Vec::new(),
        inference_calls: None,
        tool_calls: None,
        input: vec![TraceLlmMessage {
            role: "user".into(),
            blocks: vec![TraceContentBlock::Text {
                text: "secret ".repeat(100_000),
                cache_breakpoint: false,
            }],
        }],
        inputs: 1,
        outcome: TraceTurnOutcome::Completed {
            done_reason: TraceTurnCompletionReason::AssistantMessage,
        },
    };
    let omitted = record(event()).governed(ContentPolicy::omitted());
    assert!(!serde_json::to_string(&omitted).unwrap().contains("secret"));
    assert!(attributes(&omitted.event).is_empty());
    let policy = TracePolicySnapshot {
        content: ContentPolicy::omitted().with(ContentCategory::InputMessages, Capture::Captured),
        ..Default::default()
    };
    let mut captured = record(event());
    captured.apply_content(&policy, Some(&Rewrite));
    assert!(!serde_json::to_string(&captured).unwrap().contains("secret"));
    assert!(!captured.content.truncated.is_empty() || !captured.content.oversize.is_empty());
    for (_, value) in attributes(&captured.event) {
        assert!(
            serde_json::to_vec(&value).unwrap().len() as u64
                <= policy.content_limits.max_attribute_bytes()
        );
    }
}

/// FIG-6431: a hook can redact a completed tool's result by tool name.
#[test]
fn a_content_hook_selects_a_completed_tools_result_by_name() {
    struct PerTool;
    impl ContentHook for PerTool {
        fn on_content(&self, view: &ContentView<'_>, content: &mut RecordContent<'_>) {
            if view.tool_name == Some("tool") {
                content.for_each(|field| {
                    if field.category() == ContentCategory::ToolResults {
                        field.omit();
                    }
                });
            }
        }
    }
    let policy = TracePolicySnapshot {
        content: ContentPolicy::captured(),
        ..Default::default()
    };
    for (name, kept) in [("tool", false), ("other", true)] {
        let mut event = tool(json!({"secret": "tool-specific secret"}));
        if let TraceEvent::ToolCallCompleted { name: target, .. } = &mut event {
            *target = name.into();
        }
        let mut record = record(event);
        record.apply_content(&policy, Some(&PerTool));
        assert_eq!(
            serde_json::to_string(&record)
                .unwrap()
                .contains("tool-specific secret"),
            kept
        );
    }
}

/// FIG-6431: an answer uses OutputMessages consent, the exhaustive host
/// hook and the same part/attribute bounds as model output.
#[test]
fn turn_closer_output_obeys_capture_redaction_and_bounds() {
    let event = || TraceEvent::TurnCompleted {
        profile: None,
        input: Vec::new(),
        output: vec![TraceLlmMessage {
            role: "assistant".into(),
            blocks: vec![TraceContentBlock::Text {
                text: "secret ".repeat(100_000),
                cache_breakpoint: false,
            }],
        }],
        inference_calls: None,
        tool_calls: None,
        inputs: 1,
        outcome: TraceTurnOutcome::Completed {
            done_reason: TraceTurnCompletionReason::AssistantMessage,
        },
    };
    let omitted = record(event()).governed(ContentPolicy::omitted());
    assert!(!serde_json::to_string(&omitted).unwrap().contains("secret"));
    assert!(attributes(&omitted.event).is_empty());
    let policy = TracePolicySnapshot {
        content: ContentPolicy::omitted().with(ContentCategory::OutputMessages, Capture::Captured),
        ..Default::default()
    };
    let mut captured = record(event());
    captured.apply_content(&policy, Some(&Rewrite));
    assert!(!serde_json::to_string(&captured).unwrap().contains("secret"));
    assert!(!captured.content.truncated.is_empty() || !captured.content.oversize.is_empty());
    let attrs = attributes(&captured.event);
    assert!(
        attrs
            .iter()
            .any(|(key, _)| *key == "gen_ai.output.messages")
    );
    for (_, value) in attrs {
        assert!(
            serde_json::to_vec(&value).unwrap().len() as u64
                <= policy.content_limits.max_attribute_bytes()
        );
    }
}

/// BorrowedSemanticShape: bounds and exports count exactly the established
/// wrappers, nulls, escaping, omitted reasoning and parsed argument fallback.
#[test]
fn semantic_views_preserve_encoded_shapes() {
    let attachment = TraceAttachment {
        id: "attachment\"é".into(),
        media_type: "image/png".into(),
        byte_len: 7,
        position: "message".into(),
        delivery_form: None,
    };
    let messages = vec![TraceLlmMessage {
        role: "user".into(),
        blocks: vec![
            TraceContentBlock::Attachment {
                reference: Box::new(attachment.clone()),
            },
            TraceContentBlock::ToolCall {
                call_id: None,
                tool_name: "tool".into(),
                input_json: json!({"escaped":"\n\"é"}),
                item_id: None,
                has_signature: false,
            },
            TraceContentBlock::ToolResult {
                call_id: Some("call".into()),
                tool_name: None,
                content: vec![
                    TraceToolResultBlock::Text {
                        text: "result".into(),
                    },
                    TraceToolResultBlock::Attachment {
                        reference: Box::new(attachment),
                    },
                ],
            },
            TraceContentBlock::Reasoning {
                text: "private".into(),
                item_id: None,
                summary: vec![],
                has_encrypted: true,
                redacted: false,
            },
        ],
    }];
    let event = TraceEvent::TurnCompleted {
        profile: None,
        input: messages,
        output: vec![],
        inputs: 1,
        inference_calls: None,
        tool_calls: None,
        outcome: TraceTurnOutcome::Completed {
            done_reason: TraceTurnCompletionReason::AssistantMessage,
        },
    };
    let a = json!({"type":"lash.attachment","mime_type":"image/png",
        "lash.attachment.id":"attachment\"é","byte_len":7,"delivery_form":null});
    let expected = json!([{"role":"user","parts":[a.clone(),
        {"type":"tool_call","id":null,"name":"tool","arguments":{"escaped":"\n\"é"}},
        {"type":"tool_call_response","id":"call","response":[{"type":"text","content":"result"},a]}]}]);
    let attrs: Vec<_> = super::attributes(&event).collect();
    assert_eq!(
        serde_json::to_vec(&attrs[0].1).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(attrs[0].1.to_string(), expected.to_string());
    for args in ["{\"z\":1,\"a\":null}", "invalid \"é"] {
        let mut event = model_event();
        if let TraceEvent::LlmCallCompleted { response, .. } = &mut event {
            response.parts = Some(vec![
                lash_protocol_types::llm::types::LlmOutputPart::ToolCall {
                    call_id: "call".into(),
                    tool_name: "tool".into(),
                    input_json: args.into(),
                    replay: None,
                },
            ]);
        }
        let attrs: Vec<_> = super::attributes(&event).collect();
        let output = &attrs
            .iter()
            .find(|(key, _)| *key == "gen_ai.output.messages")
            .unwrap()
            .1;
        let expected = json!([{"role":"assistant","finish_reason":"unknown","parts":[{
            "type":"tool_call","id":"call","name":"tool",
            "arguments":serde_json::from_str::<Value>(args).unwrap_or_else(|_| args.into())}]}]);
        assert_eq!(
            serde_json::to_vec(output).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
    for outcome in [
        TraceToolCallOutcome::Failure(TraceToolFailure {
            class: lash_protocol_types::ToolFailureClass::Io,
            code: "io".into(),
            source: lash_protocol_types::ToolFailureSource::Tool,
            suggested_delay_ms: Some(10),
            diagnostics: Some(json!({"message":"failed"})),
        }),
        TraceToolCallOutcome::Cancelled(TraceToolCancellation {
            source: lash_protocol_types::ToolFailureSource::Runtime,
            origin: Some(lash_protocol_types::CancelOrigin::OperatorRequested),
            forced: true,
            diagnostics: Some(json!({"message":"cancelled"})),
        }),
    ] {
        let expected = match &outcome {
            TraceToolCallOutcome::Failure(failure) => json!({"error":failure}),
            TraceToolCallOutcome::Cancelled(cancellation) => json!({"error":cancellation}),
            _ => unreachable!(),
        };
        let mut event = tool(Value::Null);
        if let TraceEvent::ToolCallCompleted { output, .. } = &mut event {
            output.outcome = outcome;
        }
        let attrs: Vec<_> = super::attributes(&event).collect();
        let result = &attrs
            .iter()
            .find(|(key, _)| *key == "gen_ai.tool.call.result")
            .unwrap()
            .1;
        assert_eq!(
            serde_json::to_vec(result).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}
