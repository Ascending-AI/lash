use crate::driver::history::build_rlm_history_messages_from_turn;
use lash_core::llm::types::LlmRole;
use lash_core::sansio::ContextProjector;
use lash_core::{LlmRequest, ProjectorContext};
use lash_kernel_value::identity::SessionId;
use lash_rlm_types::RlmTurnOptions;
use lash_vocabulary::identity::TurnId;

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
        RlmProtocolEvent::RlmTrajectoryEntry(std::sync::Arc::new(CellRecord {
            read_facts: Vec::new(),
            consumption: Vec::new(),
            language: "typescript".to_string(),
            prints_retained: None,
            id: format!("lash_vm_step_{protocol_iteration}"),
            protocol_iteration,
            code: code.to_string(),
            prints: if output.is_empty() {
                Vec::new()
            } else {
                vec![output.to_string().into()]
            },
            images: Vec::new(),
            calls: Vec::new(),
            bindings: Default::default(),
            result: lash_core::CellOutcome::Completed,
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
        RlmProtocolEvent::RlmTrajectoryEntry(std::sync::Arc::new(CellRecord {
            read_facts: Vec::new(),
            consumption: Vec::new(),
            language: "typescript".to_string(),
            prints_retained: None,
            id: format!("lash_vm_step_{protocol_iteration}"),
            protocol_iteration,
            code: code.to_string(),
            prints: output.into_iter().map(Into::into).collect(),
            images,
            calls: Vec::new(),
            bindings: Default::default(),
            result: lash_core::CellOutcome::finished_by(
                crate::FINISH_TOOL_NAME,
                lash_core::ToolCallId::fixture("finish-call"),
                final_output,
            ),
        })),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}

/// Assistant prose the RLM driver commits beside a cell.
fn cell_prose_event(id: &str, text: &str) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord {
        id: id.to_string(),
        role: MessageRole::Assistant,
        parts: vec![Part::text(format!("{id}.p0"), text.to_string(), None)].into(),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
        reply_marker: None,
    })
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
            crate::dialect::CellDialect::typescript(),
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
        .map(|(name, value)| crate::session_globals::SessionGlobal {
            name: name.clone(),
            kind: crate::session_globals::SessionGlobalKind::Data(value.clone()),
        })
        .collect::<Vec<_>>();
    let presentation = crate::RlmPresentationConfig::standard();
    crate::rlm_support::render_bound_variables(
        cache,
        globals,
        &crate::dialect::TypescriptPrompts,
        &crate::render::BuiltinCodeRenderer,
        &lash_render::RenderParams::preview(),
        presentation.max_inline_keys,
        presentation.max_bound_variable_bytes,
    )
    .expect("presentable fixture")
    .0
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
    projected_request_with_facts(false).0
}

pub(crate) fn prompt_history_fixture(structured: bool) -> Vec<SessionHistoryRecord> {
    let mut history = vec![user_event("u1", "first")];
    if structured {
        history.push(step_event(0, "let value = 1;", "1"));
    }
    history
}

pub(crate) fn projected_request_with_facts(
    structured: bool,
) -> (LlmRequest, crate::prompt_sections::RlmPromptFacts) {
    let cell = projector(1000);
    let projector: Arc<dyn lash_core::sansio::ContextProjector<lash_core::HostTurnProtocol>> =
        Arc::new(cell);
    assert!(projector.has_current_context_prefix());
    let events = prompt_history_fixture(structured);
    let messages = lash_core::facade_support::MessageSequence::default();
    let facts = crate::prompt_sections::RlmPromptFacts {
        reserved_names: Vec::new(),
        bound_variables: Arc::from("- `scratch_note` = \"saved\""),

        packages: None,
        globals: Default::default(),
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
        protocol_driver: Arc::new(crate::protocol::RlmDriver::new(
            crate::dialect::CellDialect::typescript(),
        )),
        projector: Arc::new(lash_core::sansio::ChatContextProjector::default()),
        model: lash_protocol_types::llm_profile::LlmProfileConfig::new(
            lash_protocol_types::llm_profile::RecordedLlmProfile::mint(
                lash_protocol_types::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_protocol_types::llm_profile::LlmProfileMetadata::builder(model.to_string())
                    .context_window_tokens(max_context_tokens.unwrap_or(128_000))
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_protocol_types::llm::capability::CacheRetention::Short)
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
        vec!["other-caller-boundary".to_string()],
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
        cell_prose_event("terminal-prose", "Internal terminal commentary."),
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

    assert!(rendered.contains("Earlier program:\nprint \"observed mid-turn\""));
    assert!(rendered.contains("observed mid-turn"));
    assert!(rendered.contains("Host-rendered answer: forty-two."));
    assert!(!rendered.contains("Internal terminal commentary."));
    assert!(!rendered.contains("finish { answer: 42 }"));
    assert!(!rendered.contains("terminal-only output"));
    assert!(!rendered.contains("Final output:"));
    assert!(rendered.contains("Output:\nnext turn"));
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

        assert!(history.contains(&format!("Earlier program:\n{code}")));
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

    assert!(history.contains("Earlier program:\nfinish 42"));
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

/// A cut message states what was shown and its total size, and names no
/// address to read the rest from (ADR 0147).
#[test]
fn long_user_message_states_its_cut() {
    let projector = projector(10);
    let history = projector.format_history(&[user_event("u1", "abcdefghijklmnopqrstuvwxyz")]);

    assert!(
        history.contains("(cut: 10 of 26 characters shown)"),
        "{history}"
    );
    assert!(!history.contains("rlm.history"), "{history}");
    assert!(history.contains("... (16 characters omitted) ..."));
    assert!(!history.contains("user_input_"));
}

#[test]
fn structured_lash_vm_step_output_keeps_diagnostic_fields_in_projected_history() {
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

    assert!(history.contains("Output:\n"), "{history}");
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
        RlmProtocolEvent::RlmTrajectoryEntry(std::sync::Arc::new(CellRecord {
            read_facts: Vec::new(),
            consumption: Vec::new(),
            language: "typescript".to_string(),
            prints_retained: None,
            id: "lash_vm_step_1".to_string(),
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
            bindings: Default::default(),
            result: lash_core::CellOutcome::Completed,
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
    let step = step_event(0, "print 1", "1");
    let events = [user_event("u1", "first"), native_context(&step), step];

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
    assert!(
        matches!(messages[1].blocks.first(), Some(LlmContentBlock::ToolCall { tool_name, input_json, .. }) if tool_name == "execute_code" && input_json.contains("print 1"))
    );
    // The portable fence follows the typed result, so all result bytes enter the cache prefix.
    assert!(
        matches!(messages[1].blocks.get(1), Some(LlmContentBlock::Text { text, cache_breakpoint: false, .. }) if text.as_ref() == "Running the program.")
    );
    assert!(
        matches!(messages[2].blocks.first(), Some(LlmContentBlock::ToolResult { content, .. }) if content.iter().any(|part| matches!(part, lash_core::facade_support::ModelToolReturnPart::Text { text } if text.starts_with("Output:\n"))))
    );
    assert!(matches!(
        messages[2].blocks.last(),
        Some(LlmContentBlock::Text {
            cache_breakpoint: true,
            ..
        })
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

/// A growing turn validates each retained event once, rather than decoding
/// its tool-value graph again at every model call; a fresh projector over
/// the same events decodes nothing again (FIG-6443). A rewritten payload is
/// a new value and is validated again.
#[test]
fn growing_turn_decodes_only_new_history_events() {
    let projector = projector(100);
    let mut events = Vec::new();
    crate::projection::EVENT_DECODES.with(|count| count.set(0));
    for round in 0..16 {
        let step = step_event(round, "console.log(1)", "ok");
        events.push(native_context(&step));
        events.push(step);
        let request = project_iteration_request(&projector, &events, round, "test-model");
        assert_eq!(request.messages.len(), (round + 1) * 2 + 1);
    }
    let decodes = crate::projection::EVENT_DECODES.with(std::cell::Cell::get);
    assert_eq!(
        decodes,
        events.len() / 2,
        "unchanged history must not be decoded again"
    );
    let fresh = self::projector(100);
    project_iteration_request(&fresh, &events, 16, "test-model");
    assert_eq!(
        crate::projection::EVENT_DECODES.with(std::cell::Cell::get),
        decodes
    );
    let SessionHistoryRecord::Protocol(first) = &mut events[1] else {
        unreachable!()
    };
    first.payload.make_mut()["format"] = serde_json::json!(0);
    let config = projection_test_config("test-model", Default::default(), None);
    let error = projector
        .project(ProjectorContext {
            config: &config,
            messages: &Default::default(),
            events: &events,
            protocol_iteration: 17,
            use_tools: false,
            environment: &Default::default(),
        })
        .expect_err("rewritten corrupt content must validate again");
    assert_eq!(error.record_kind, "RLM protocol event");
}

pub(crate) fn native_context(event: &SessionHistoryRecord) -> SessionHistoryRecord {
    let SessionHistoryRecord::Protocol(event) = event else {
        panic!("expected a trajectory")
    };
    let Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step)) = Some(
        serde_json::from_value::<lash_rlm_types::RlmProtocolEvent>(event.payload["event"].clone())
            .expect("typed fixture"),
    ) else {
        panic!("expected a cell")
    };
    SessionHistoryRecord::Conversation(lash_core::session_model::ConversationRecord::from_message(
        crate::native::transport::cell_context_message(
            &lash_vocabulary::identity::TurnId::from("turn"),
            format!("context-{}", step.id),
            step.id.clone(),
            vec![
                lash_core::Part::tool_call(
                    "p0".into(),
                    serde_json::json!({"code":step.code}).to_string(),
                    lash_core::ToolCallId::fixture(&format!("call-{}", step.id)),
                    format!("provider-{}", step.id),
                    "execute_code".into(),
                    None,
                ),
                lash_core::Part::prose("commentary".into(), "Running the program.".into(), None),
            ],
        ),
    ))
}
