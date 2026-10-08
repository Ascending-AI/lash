use lash_core::llm::types::LlmRole;
use lash_rlm_types::RlmTurnOptions;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

use super::*;
use lash_core::CellRecord;
use lash_core::session_model::{ConversationRecord, MessageRole, Part, SessionHistoryRecord};
use lash_rlm_types::RlmProtocolEvent;

fn user_event(id: &str, text: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord {
        id: id.to_string(),
        role: MessageRole::User,
        parts: vec![Part::text(format!("{id}.p0"), text.to_string(), None)].into(),
        origin: None,
        reply_marker: None,
    })
}

fn step_event(protocol_iteration: usize, code: &str, output: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(Box::new(CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: format!("lashlang_step_{protocol_iteration}"),
            protocol_iteration,
            code: code.to_string(),
            prints: if output.is_empty() {
                Vec::new()
            } else {
                vec![output.to_string().into()]
            },
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellResult::Completed,
        })),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}

fn terminal_step_event(
    protocol_iteration: usize,
    code: &str,
    output: Vec<String>,
    images: Vec<lash_core::AttachmentRef>,
    final_output: serde_json::Value,
) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(Box::new(CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: format!("lashlang_step_{protocol_iteration}"),
            protocol_iteration,
            code: code.to_string(),
            prints: output.into_iter().map(Into::into).collect(),
            images,
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellResult::Finished(final_output.into()),
        })),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}

fn assistant_content_event(id: &str, prose: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmAssistantContent(lash_rlm_types::RlmAssistantContent {
            id: id.to_string(),
            reasoning: String::new(),
            prose: prose.to_string(),
        }),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}

fn assistant_prose_event(id: &str, text: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord {
        id: id.to_string(),
        role: MessageRole::Assistant,
        parts: vec![Part::text(format!("{id}.p0"), text.to_string(), None)].into(),
        origin: None,
        reply_marker: None,
    })
}

pub(super) fn projector(max_output_chars: usize) -> RlmContextProjector {
    RlmContextProjector {
        max_output_chars,
        dialect: Arc::new(SessionDialect::prompt_only(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            LashlangSurface::default(),
        )),
    }
}

pub(crate) fn rendered_bound_variables(
    cache: &mut crate::rlm_support::BoundVariableRenderCache,
    globals: serde_json::Value,
) -> Arc<str> {
    let globals = globals
        .as_object()
        .expect("globals object")
        .iter()
        .map(|(name, value)| (name.clone(), lashlang::from_json(value.clone())))
        .collect::<Vec<_>>();
    crate::rlm_support::render_bound_variables(
        cache,
        &globals,
        &[],
        &crate::dialect::TypescriptDialect,
        &crate::render::BuiltinCodeRenderer,
        &lash_render::RenderParams::preview(),
        crate::RlmPresentationConfig::standard().max_inline_keys,
    )
}

fn project_iteration_request(
    projector: &RlmContextProjector,
    events: &[SessionHistoryRecord],
    protocol_iteration: usize,
    model: &str,
) -> Arc<LlmRequest> {
    project_iteration_request_with_generation(
        projector,
        events,
        protocol_iteration,
        model,
        Default::default(),
        None,
    )
}

/// The request a projector renders from history alone: no prompt section is
/// the projector's to place.
fn project_iteration_request_with_generation(
    projector: &RlmContextProjector,
    events: &[SessionHistoryRecord],
    protocol_iteration: usize,
    model: &str,
    generation: lash_core::GenerationOptions,
    max_context_tokens: Option<usize>,
) -> Arc<LlmRequest> {
    let config = projection_test_config(model, generation, max_context_tokens);
    projector
        .project(ProjectorContext {
            config: &config,
            messages: &lash_core::facade_support::MessageSequence::default(),
            events,
            protocol_iteration,
            use_tools: false,
            environment: &lash_core::sansio::ExecutionEnvironmentSync::default(),
        })
        .expect("valid history fixture")
}

/// A request projected from a one-message history, for the prompt laws
/// that place sections on it.
pub(crate) fn projected_request() -> LlmRequest {
    projected_request_with_facts(crate::RlmChannel::Cell, false, true).0
}

pub(crate) fn prompt_history_fixture(structured: bool) -> Vec<SessionHistoryRecord> {
    let mut history = vec![user_event("u1", "first")];
    if structured {
        history.push(step_event(0, "let value = 1;", "1"));
    }
    history
}

pub(crate) fn projected_request_with_facts(
    channel: crate::RlmChannel,
    structured: bool,
    images: bool,
) -> (LlmRequest, crate::prompt_sections::RlmPromptFacts) {
    let cell = projector(1000);
    let dialect = Arc::clone(&cell.dialect);
    let projector: Arc<dyn ContextProjector<lash_core::HostTurnProtocol>> = match channel {
        crate::RlmChannel::Cell => Arc::new(cell),
        crate::RlmChannel::NativeTool => crate::native::testing_projector(Arc::clone(&dialect)),
    };
    assert!(projector.has_current_context_prefix());
    let events = prompt_history_fixture(structured);
    let messages = lash_core::facade_support::MessageSequence::default();
    let projection =
        lash_core::facade_support::ChronologicalProjection::from_turn_view(&events, &messages);
    let facts = crate::prompt_sections::RlmPromptFacts {
        history_binding: Arc::from(
            crate::prompt_sections::history_binding(&dialect, &projection, images)
                .expect("valid history"),
        ),
        bound_variables: Arc::from("- `scratch_note` = \"saved\""),
        read_only_variables: None,
    };
    let config = projection_test_config("test-model", Default::default(), None);
    let request = projector
        .project(ProjectorContext {
            config: &config,
            messages: &messages,
            events: &events,
            protocol_iteration: 0,
            use_tools: false,
            environment: &Default::default(),
        })
        .expect("valid history");
    (Arc::unwrap_or_clone(request), facts)
}

#[test]
fn fig1123_cell_projector_uses_committed_agent_frame_id() {
    let request = project_iteration_request(&projector(1024), &[], 0, "model");
    assert_eq!(request.scope.agent_frame_id, "prefix-stability-frame");
}

pub(super) fn projection_test_config(
    model: &str,
    generation: lash_core::GenerationOptions,
    max_context_tokens: Option<usize>,
) -> lash_core::TurnMachineConfig {
    lash_core::TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: Arc::new(crate::protocol::RlmDriver::new(Arc::new(
            crate::dialect::TypescriptDialect,
        ))),
        projector: Arc::new(lash_core::sansio::ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder(model.to_string())
                    .context_window_tokens(max_context_tokens.unwrap_or(128_000))
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::Unbounded,
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation,
        session_id: SessionId::from("prefix-stability"),
        agent_frame_id: "prefix-stability-frame".to_string(),
        turn_id: TurnId::from("prefix-stability-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: lash_core::ProtocolTurnOptions::typed(RlmTurnOptions::default())
            .expect("RLM options"),
    }
}

#[test]
fn rlm_projector_sends_no_stop_sequence_without_caller_stops() {
    let request = project_iteration_request(&projector(100), &[], 0, "test-model");
    assert!(request.generation.stop_sequences.is_empty());
    assert!(!request.generation.stop_sequences_suppressed_by_protocol());
}

#[test]
fn rlm_projector_suppresses_every_nonempty_caller_stop_list() {
    for caller_stops in [
        vec!["caller-boundary".to_string()],
        vec!["</typescript>".to_string()],
    ] {
        let request = project_iteration_request_with_generation(
            &projector(100),
            &[],
            0,
            "test-model",
            lash_core::GenerationOptions {
                stop_sequences: caller_stops,
                ..Default::default()
            },
            None,
        );
        assert!(request.generation.stop_sequences.is_empty());
        assert!(request.generation.stop_sequences_suppressed_by_protocol());
    }
}

pub(crate) fn message_text(message: &LlmMessage) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn folded_step_renders_as_emission_cell_not_history_echo() {
    let projector = projector(1000);
    // Regression for the observed glm-5.2 echo: a step preceded by assistant
    // prose folds into ONE assistant message that is the literal `<typescript>`
    // cell — byte-identical to what the model emits — never a
    // `--- history[...] ---` meta-format the model could imitate.
    let events = [
        user_event("u1", "find it"),
        assistant_prose_event("a1", "Found it. Running it now."),
        step_event(0, "loc = run()", "ok"),
    ];

    let messages = build_rlm_history_messages_from_turn(RlmHistoryRenderInput {
        dialect: projector.dialect.as_ref(),
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 1000,
        protocol_iteration: 1,
    })
    .expect("valid history fixture");

    let assistant_texts = messages
        .iter()
        .filter(|message| matches!(message.role, LlmRole::Assistant))
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();

    // The prose folded into the cell: exactly one assistant message, no echo.
    assert_eq!(assistant_texts.len(), 1);
    assert!(!assistant_texts[0].contains("--- history["));
    assert!(!assistant_texts[0].contains("Code:\n"));
    assert_eq!(
        assistant_texts[0],
        projector
            .dialect
            .render_history_cell("Found it. Running it now.", "loc = run()")
    );
}

#[test]
fn committed_transcript_supersedes_terminal_step_by_turn_provenance() {
    let projector = projector(1000);
    let terminal_image = lash_core::AttachmentRef {
        id: lash_core::AttachmentId::parse(
            "e0588aaf696ccc2638d8e207add1b4e512f73e535267eab278525d02bfd1c220",
        )
        .expect("valid attachment id"),
        media_type: lash_core::MediaType::parse("image/png").unwrap(),
        byte_len: 3,
        type_metadata: Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
        label: Some("terminal.png".to_string()),
    };
    let events = [
        user_event("u1", "compute it"),
        step_event(0, "print \"observed mid-turn\"", "observed mid-turn"),
        assistant_content_event("terminal-prose", "Internal terminal commentary."),
        terminal_step_event(
            1,
            "print image\nfinish { answer: 42 }",
            vec!["terminal-only output".to_string()],
            vec![terminal_image],
            serde_json::json!({ "answer": 42 }),
        ),
        // Deliberately differs from the finish value: precedence is based
        // on same-turn provenance, never content equality.
        assistant_prose_event("committed-a1", "Host-rendered answer: forty-two."),
        user_event("u2", "continue"),
        step_event(0, "print \"next turn\"", "next turn"),
    ];

    let messages = render_history_messages(&RlmHistoryRenderInput {
        dialect: projector.dialect.as_ref(),
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 1000,
        protocol_iteration: 0,
    })
    .expect("valid history fixture");
    let rendered = messages
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("<typescript>\nprint \"observed mid-turn\"\n</typescript>"));
    assert!(rendered.contains("observed mid-turn"));
    assert!(rendered.contains("Host-rendered answer: forty-two."));
    assert!(!rendered.contains("Internal terminal commentary."));
    assert!(!rendered.contains("finish { answer: 42 }"));
    assert!(!rendered.contains("terminal-only output"));
    assert!(!rendered.contains("Final output:"));
    assert!(rendered.contains("history[4].output[0]:\nnext turn"));
    assert!(!rendered.contains("history[6].output[0]"));
    assert!(
        messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .all(|block| !matches!(block, LlmContentBlock::Attachment { .. }))
    );
}

#[test]
fn terminal_step_without_committed_transcript_renders_unchanged() {
    for (code, value, expected) in [
        (
            "finish \"string answer\"",
            serde_json::json!("string answer"),
            "\"string answer\"",
        ),
        (
            "finish { answer: 42 }",
            serde_json::json!({ "answer": 42 }),
            "{\n  \"answer\": 42\n}",
        ),
    ] {
        let events = [
            user_event("u1", "compute it"),
            terminal_step_event(0, code, Vec::new(), Vec::new(), value),
        ];
        let history = projector(1000).format_history(&events);

        assert!(history.contains(&format!("<typescript>\n{code}\n</typescript>")));
        assert!(history.contains(&format!("Final output:\n{expected}")));
    }
}

#[test]
fn later_turn_assistant_does_not_supersede_uncommitted_terminal_step() {
    let events = [
        user_event("u1", "typed value not surfaced"),
        terminal_step_event(
            0,
            "finish 42",
            Vec::new(),
            Vec::new(),
            serde_json::json!(42),
        ),
        user_event("u2", "a new turn"),
        assistant_prose_event("a2", "Natural answer from turn two."),
    ];
    let history = projector(1000).format_history(&events);

    assert!(history.contains("<typescript>\nfinish 42\n</typescript>"));
    assert!(history.contains("Final output:\n42"));
    assert!(history.contains("Natural answer from turn two."));
}

#[test]
fn natural_prose_history_is_byte_unchanged() {
    let projector = projector(1000);
    let events = [
        user_event("u1", "Tell me naturally."),
        assistant_prose_event("a1", "A natural prose answer.\n\nSecond paragraph."),
    ];

    let messages = render_history_messages(&RlmHistoryRenderInput {
        dialect: projector.dialect.as_ref(),
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 1000,
        protocol_iteration: 0,
    })
    .expect("valid history fixture");

    assert_eq!(messages.len(), 2);
    assert_eq!(message_text(&messages[0]), "Tell me naturally.");
    assert_eq!(
        message_text(&messages[1]),
        "A natural prose answer.\n\nSecond paragraph."
    );
}

#[test]
fn committed_transcript_remains_the_rolling_cache_fence() {
    let projector = projector(1000);
    let events = [
        user_event("u1", "compute"),
        terminal_step_event(
            0,
            "finish \"done\"",
            Vec::new(),
            Vec::new(),
            serde_json::json!("done"),
        ),
        assistant_prose_event("a1", "done"),
    ];

    let messages = build_rlm_history_messages_from_turn(RlmHistoryRenderInput {
        dialect: projector.dialect.as_ref(),
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 1000,
        protocol_iteration: 0,
    })
    .expect("valid history fixture");

    assert!(matches!(
        messages[1].blocks.first(),
        Some(LlmContentBlock::Text {
            text,
            cache_breakpoint: true,
            ..
        }) if text.as_ref() == "done"
    ));
    assert!(matches!(
        messages[2].blocks.first(),
        Some(LlmContentBlock::Text {
            cache_breakpoint: false,
            ..
        })
    ));
}

#[test]
fn long_user_message_gets_full_history_reference() {
    let projector = projector(10);
    let history = projector.format_history(&[user_event("u1", "abcdefghijklmnopqrstuvwxyz")]);

    assert!(history.contains("re-run `console.log(history[0].content)`"));
    assert!(history.contains("... (16 characters omitted) ..."));
    assert!(!history.contains("user_input_"));
}

#[test]
fn structured_lashlang_step_output_keeps_diagnostic_fields_in_projected_history() {
    let projector = projector(10);
    let raw = serde_json::json!({
        "output": "x".repeat(60 * 1024),
        "status": "failed",
        "error": "boom",
        "exit_code": 2,
        "stderr": "short stderr"
    })
    .to_string();
    let history = projector.format_history(&[step_event(0, "print result", &raw)]);

    assert!(history.contains("history[0].output[0]:\n"), "{history}");
    for field in [
        r#""status":"failed""#,
        r#""error":"boom""#,
        r#""exit_code":2"#,
        r#""stderr":"short stderr""#,
    ] {
        assert!(history.contains(field), "missing {field}: {history}");
    }
    assert!(history.contains(&raw), "{history}");
}

#[test]
fn plugin_origin_is_not_rendered_in_history() {
    let projector = projector(100);
    let event = SessionHistoryRecord::Conversation(ConversationRecord {
        id: "plugin".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text(
            "plugin.p0".to_string(),
            "synthetic plugin message".to_string(),
            None,
        )]
        .into(),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: "test".to_string(),
            transient: false,
        }),
        reply_marker: None,
    });

    let history = projector.format_history(&[event]);
    assert!(history.contains("synthetic plugin message"));
    assert!(!history.contains("from plugin"));
    assert!(!history.contains("test"));
    assert!(!history.contains("--- history["));
}

#[test]
fn printed_images_render_as_llm_image_blocks() {
    let projector = projector(1000);
    let event = SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(Box::new(CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "lashlang_step_1".to_string(),
            protocol_iteration: 1,
            code: "print img".to_string(),
            prints: vec![r#"{"type":"image","id":"img"}"#.to_string().into()],
            images: vec![lash_core::AttachmentRef {
                id: lash_core::AttachmentId::parse(
                    "8f9e0cfb92cb165ce6277b3b26a453fd81ac949a6ab9afba49d348483e8f7c78",
                )
                .expect("valid attachment id"),
                media_type: lash_core::MediaType::parse("image/png").unwrap(),
                byte_len: 3,
                type_metadata: Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
                label: Some("img.png".to_string()),
            }],
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellResult::Completed,
        })),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ));
    let events = [event];

    let messages = build_rlm_history_messages_from_turn(RlmHistoryRenderInput {
        dialect: projector.dialect.as_ref(),
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 1000,
        protocol_iteration: 1,
    })
    .expect("valid history fixture");

    let attachments = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Attachment { reference } => Some(reference.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attachments.len(), 1);
    assert_eq!(
        attachments[0].id.as_str(),
        "8f9e0cfb92cb165ce6277b3b26a453fd81ac949a6ab9afba49d348483e8f7c78"
    );
    assert_eq!(attachments[0].media_type.as_str(), "image/png");
    // The printed image rides the user observation message for the step.
    assert!(messages.iter().any(|message| {
        matches!(message.role, LlmRole::User)
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, LlmContentBlock::Attachment { .. }))
    }));
}

#[test]
fn rlm_prompt_projects_history_as_chat_messages_with_rolling_cache_breakpoint() {
    let projector = projector(1000);
    let events = [user_event("u1", "first"), step_event(0, "print 1", "1")];

    let messages = build_rlm_history_messages_from_turn(RlmHistoryRenderInput {
        dialect: projector.dialect.as_ref(),
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 1000,
        protocol_iteration: 2,
    })
    .expect("valid history fixture");

    // user turn, assistant cell, user observation, volatile current-iteration tail.
    assert_eq!(messages.len(), 4);
    assert!(matches!(messages[0].role, LlmRole::User));
    assert!(matches!(messages[1].role, LlmRole::Assistant));
    assert!(matches!(messages[2].role, LlmRole::User));
    assert!(matches!(messages[3].role, LlmRole::User));
    assert!(matches!(
        messages[0].blocks.first(),
        Some(LlmContentBlock::Text {
            text,
            cache_breakpoint: false,
            ..
        }) if text.as_ref() == "first"
    ));
    assert!(matches!(
        messages[1].blocks.first(),
        Some(LlmContentBlock::Text {
            text,
            cache_breakpoint: false,
            ..
        }) if text.starts_with("<typescript>") && text.contains("print 1")
    ));
    // The last history message (the observation) carries the rolling fence.
    assert!(matches!(
        messages[2].blocks.first(),
        Some(LlmContentBlock::Text {
            text,
            cache_breakpoint: true,
            ..
        }) if text.starts_with("history[1].output[0]")
    ));
    assert!(matches!(
        messages[3].blocks.first(),
        Some(LlmContentBlock::Text {
            text,
            cache_breakpoint: false,
            ..
        }) if text.contains("=== CURRENT ITERATION: 2 ===")
    ));
}

fn required_output_contract(schema: serde_json::Value) -> String {
    crate::dialect::typescript_test_dialect().required_output_contract(&schema)
}

#[test]
fn required_output_contract_renders_the_type_and_a_row_per_noted_field() {
    let rendered = required_output_contract(serde_json::json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["call", "fold"] },
            "confidence": {
                "type": "number", "minimum": 0, "maximum": 1,
                "description": "How sure the call is."
            }
        },
        "required": ["action"]
    }));
    assert_eq!(
        rendered,
        "{ action: \"call\" | \"fold\"; confidence?: number }\nFields:\n- `confidence?: number` (>= 0, <= 1) — How sure the call is."
    );
}

#[test]
fn required_output_contract_is_the_bare_type_when_no_field_carries_notes() {
    assert_eq!(
        required_output_contract(serde_json::json!({ "type": "string" })),
        "string"
    );
    assert_eq!(
        required_output_contract(
            serde_json::json!({ "type": "array", "items": { "type": "integer" } })
        ),
        "Array<number>"
    );
    assert_eq!(
        required_output_contract(serde_json::json!({ "type": ["string", "null"] })),
        "string | null"
    );
}
