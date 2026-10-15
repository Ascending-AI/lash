//! Content laws L4–L7, L20, L21(c), L22(a) and L24.
use super::*;
use crate::*;
use serde_json::{Value, json};

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
        call_id: lash_sansio::ToolCallId::fixture("call"),
        provider_call_id: None,
        name: "tool".into(),
        args: json!([1, 2]),
        output: TraceToolCallOutput {
            outcome: TraceToolCallOutcome::Success(value),
            control: None,
        },
        duration_ms: 1,
        issuing_node_id: None,
        attempts: None,
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
            response.parts = Some(vec![lash_sansio::llm::types::LlmOutputPart::Reasoning {
                text: "private reasoning".into(),
                replay: Some(lash_sansio::llm::types::ProviderReasoningReplay {
                    redacted: !encrypted,
                    encrypted_content: encrypted.then(|| "opaque".into()),
                    ..Default::default()
                }),
            }]);
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

    // The error wrapper alone can push an otherwise fitting failure over the
    // attribute bound. Dropping its content must retain the failure class.
    let mut failure = json!({"class":"retryable", "values":vec![0; max / 2 - 1024]});
    let padding = max - 5 - serde_json::to_vec(&failure).unwrap().len() - 13;
    failure["padding"] = json!("x".repeat(padding));
    assert_eq!(serde_json::to_vec(&failure).unwrap().len(), max - 5);
    let mut failed = tool(Value::Null);
    if let TraceEvent::ToolCallCompleted { output, .. } = &mut failed {
        output.outcome = TraceToolCallOutcome::Failure(failure);
    }
    let bounded = record(failed).governed(ContentPolicy::captured());
    let TraceEvent::ToolCallCompleted { output, .. } = bounded.event else {
        panic!()
    };
    let TraceToolCallOutcome::Failure(value) = output.outcome else {
        panic!("truncation changed the tool outcome")
    };
    assert_eq!(value["class"], "retryable");
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
