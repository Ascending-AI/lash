use super::*;
use crate::AttachmentRef;

fn part(kind: PartKind, content: &str) -> Part {
    Part::base("p0".to_string(), kind, content.to_string())
}

fn test_attachment_ref(byte_len: u64) -> AttachmentRef {
    AttachmentRef {
        id: crate::AttachmentId::parse("att-test").expect("valid attachment id"),
        media_type: crate::MediaType::parse("image/png").unwrap(),
        byte_len,
        type_metadata: None,
        label: None,
    }
}

fn witness_message(id: &str, text: &str) -> Message {
    Message {
        id: id.to_string(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), text.to_string(), None)]),
        origin: None,
        reply_marker: None,
    }
}

#[test]
fn a_result_appended_to_cached_history_keeps_its_original_provider_correlation() {
    let call_id = crate::ToolCallId::fixture("deferred-call");
    let call = Message {
        id: "call".into(),
        role: MessageRole::Assistant,
        parts: shared_parts(vec![Part::tool_call(
            "call.p0".into(),
            "{}".into(),
            call_id.clone(),
            "provider-call".into(),
            "lookup".into(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    };
    let cache = Arc::new(BaseRenderCache::new());
    let mut sequence = MessageSequence::from_base(vec![call].into()).with_base_render_cache(cache);
    sequence.render_prompt();
    sequence.push(Message {
        id: "result".into(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::tool_result(
            "result.p0".into(),
            vec![ModelToolReturnPart::text("resolved")],
            call_id,
            "lookup".into(),
            crate::ToolCallStatus::Success,
            None,
        )]),
        origin: None,
        reply_marker: None,
    });
    let rendered = sequence.render_prompt();
    assert_eq!(
        rendered.messages,
        render_prompt(sequence.as_slice()).messages
    );
    assert!(rendered.messages.iter().flat_map(|message| message.blocks.iter()).any(|block| {
        matches!(block, LlmContentBlock::ToolResult { call_id, .. } if call_id == "provider-call")
    }));
}

#[test]
fn a_shared_base_witnesses_the_preserved_prefix_and_names_the_delta() {
    let base = AppendVec::from(vec![witness_message("m0", "one")]);
    let current = MessageSequence::from_base(base.clone());
    let mut next = MessageSequence::from_base(base);
    next.push(witness_message("m1", "two"));

    let delta = current
        .preserved_extension_delta(&next)
        .expect("a rope over the same base preserves its prefix by identity");
    assert_eq!(
        delta.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["m1"]
    );
}

#[test]
fn an_equal_but_separately_built_base_does_not_witness() {
    let current = MessageSequence::from_base(AppendVec::from(vec![witness_message("m0", "one")]));
    let next = MessageSequence::from_base(AppendVec::from(vec![witness_message("m0", "one")]));

    assert!(current.preserved_extension_delta(&next).is_none());
}

#[test]
fn a_rebuilt_owned_sequence_drops_the_witness() {
    let base = AppendVec::from(vec![witness_message("m0", "one")]);
    let current = MessageSequence::from_base(base.clone());
    let mut rewritten = MessageSequence::from_base(base);
    rewritten.replace(vec![witness_message("m0", "rewritten")]);

    assert!(current.preserved_extension_delta(&rewritten).is_none());
}

#[test]
fn a_diverged_delta_does_not_witness_and_a_shorter_one_does_not_either() {
    let base = AppendVec::from(vec![witness_message("m0", "one")]);
    let mut current = MessageSequence::from_base(base.clone());
    current.push(witness_message("m1", "two"));

    let mut diverged = MessageSequence::from_base(base.clone());
    diverged.push(witness_message("m1", "changed"));
    assert!(current.preserved_extension_delta(&diverged).is_none());

    let shorter = MessageSequence::from_base(base);
    assert!(current.preserved_extension_delta(&shorter).is_none());
}

#[test]
fn content_equality_answers_what_comparing_serialized_messages_answered() {
    let left = witness_message("m0", "one");
    let mut right = left.clone();
    assert!(message_content_equal(&left, &right));

    right.parts = shared_parts((*left.parts).clone());
    assert!(
        !Arc::ptr_eq(&left.parts, &right.parts),
        "the structural comparison is the one under test"
    );
    assert!(message_content_equal(&left, &right));

    let changed = witness_message("m0", "two");
    assert!(!message_content_equal(&left, &changed));
    assert_eq!(
        message_content_equal(&left, &changed),
        serde_json::to_value(&left).unwrap() == serde_json::to_value(&changed).unwrap()
    );
}

fn attachment_part(bytes: &[u8]) -> Part {
    Part::attachment_part(
        "p0".to_string(),
        String::new(),
        Some(PartAttachment {
            source: AttachmentSource::stored(test_attachment_ref(bytes.len() as u64)),
        }),
    )
}

#[test]
fn render_transcript_prompt_orders_turns_oldest_first() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "first")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![part(PartKind::Prose, "reply one")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "second")].into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);

    assert!(text.contains("=== Turn 1 ===\nUser:\nfirst"));
    assert!(text.contains("Assistant (Lash, continuing this transcript):\nreply one"));
    assert!(text.contains("=== Turn 2 ===\nUser:\nsecond"));
}

fn block_text(msg: &LlmMessage, idx: usize) -> &str {
    match msg.blocks.get(idx) {
        Some(LlmContentBlock::Text { text, .. }) => text.as_ref(),
        Some(other) => panic!("expected Text block, got {other:?}"),
        None => panic!("missing block at index {idx}"),
    }
}

#[test]
fn render_prompt_repl_preserves_message_boundaries() {
    let msgs = vec![
        Message {
            id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "first")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::Assistant,
            parts: vec![
                part(PartKind::Prose, "reply one"),
                part(PartKind::Code, "x = 1"),
            ]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m3".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "second")].into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_prompt(&msgs);
    assert_eq!(rendered.messages.len(), 3);
    assert_eq!(block_text(&rendered.messages[0], 0), "first");
    assert!(block_text(&rendered.messages[1], 0).contains("reply one"));
    assert_eq!(block_text(&rendered.messages[1], 1), "x = 1");
    assert_eq!(block_text(&rendered.messages[2], 0), "second");
}

#[test]
fn render_structured_prompt_preserves_tool_protocol_and_user_images() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::System,
            parts: vec![part(PartKind::Text, "note")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![
                part(PartKind::Text, "show this"),
                attachment_part(&[1, 2, 3]),
            ]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m2.p0".to_string(),
                r#"{"path":"README.md"}"#.to_string(),
                crate::ToolCallId::fixture("tc1"),
                "tc1".to_string(),
                "read_file".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m3".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "m3.p0".to_string(),
                vec![crate::ModelToolReturnPart::text("ok")],
                crate::ToolCallId::fixture("tc1"),
                "read_file".to_string(),
                crate::ToolCallStatus::Success,
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_structured_prompt(&msgs);
    assert_eq!(rendered.messages.len(), 4);
    assert_eq!(rendered.messages[0].role, LlmRole::System);
    assert_eq!(block_text(&rendered.messages[0], 0), "Runtime note:\nnote");
    // User message has text + image blocks bundled together.
    assert_eq!(rendered.messages[1].role, LlmRole::User);
    assert!(matches!(
        rendered.messages[1].blocks[0],
        LlmContentBlock::Text { .. }
    ));
    assert!(matches!(
        rendered.messages[1].blocks[1],
        LlmContentBlock::Attachment { .. }
    ));
    assert_eq!(rendered.attachments().len(), 1);
    assert!(matches!(
        rendered.messages[2].blocks[0],
        LlmContentBlock::ToolCall { .. }
    ));
    assert!(matches!(
        rendered.messages[3].blocks[0],
        LlmContentBlock::ToolResult { .. }
    ));
}

#[test]
fn render_structured_prompt_preserves_empty_tool_results() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m0.p0".to_string(),
                r#"{"question":"Pick one"}"#.to_string(),
                crate::ToolCallId::fixture("ask_1"),
                "ask_1".to_string(),
                "ask".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "m1.p0".to_string(),
                Vec::new(),
                crate::ToolCallId::fixture("ask_1"),
                "ask".to_string(),
                crate::ToolCallStatus::Success,
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_structured_prompt(&msgs);
    assert_eq!(rendered.messages.len(), 2);
    match &rendered.messages[0].blocks[0] {
        LlmContentBlock::ToolCall {
            call_id, tool_name, ..
        } => {
            assert_eq!(call_id, "ask_1");
            assert_eq!(tool_name, "ask");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
    match &rendered.messages[1].blocks[0] {
        LlmContentBlock::ToolResult {
            call_id, content, ..
        } => {
            assert_eq!(call_id, "ask_1");
            assert!(content.is_empty());
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn render_transcript_prompt_collects_attachments() {
    let msgs = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![attachment_part(&[9, 8, 7])].into(),
        origin: None,
        reply_marker: None,
    }];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);
    assert!(text.contains("[Attachment]"));
    assert_eq!(rendered.attachments().len(), 1);
}

#[test]
fn render_transcript_prompt_omits_missing_assistant_placeholder_for_current_turn() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "first")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![part(PartKind::Prose, "reply one")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "second")].into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);

    assert!(text.contains("=== Turn 2 ===\nUser:\nsecond"));
    assert!(!text.contains("=== Turn 2 ===\nUser:\nsecond\n\nAssistant (Lash, continuing this transcript):\n[No assistant content recorded]"));
}

#[test]
fn render_transcript_prompt_preserves_tool_name_for_assistant_tool_calls() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "what time is it")].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m1.p0".to_string(),
                r#"{"timezone":"UTC"}"#.to_string(),
                crate::ToolCallId::fixture("tc1"),
                "tc1".to_string(),
                "get_time".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);

    assert!(text.contains(r#"get_time({"timezone":"UTC"})"#));
}

#[test]
fn render_transcript_prompt_omits_runtime_notes_section() {
    let msgs = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![part(PartKind::Text, "hi")].into(),
        origin: None,
        reply_marker: None,
    }];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);
    assert!(!text.contains("Runtime Notes:"));
}

#[test]
fn prompt_resume_safety_accepts_completed_tool_history() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m0.p0".to_string(),
                r#"{"path":"README.md"}"#.to_string(),
                crate::ToolCallId::fixture("tc1"),
                "tc1".to_string(),
                "read_file".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "m1.p0".to_string(),
                vec![crate::ModelToolReturnPart::text("ok")],
                crate::ToolCallId::fixture("tc1"),
                "read_file".to_string(),
                crate::ToolCallStatus::Success,
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    ];

    assert!(messages_are_prompt_resume_safe(&msgs));
}

#[test]
fn reasoning_parts_survive_snapshot_but_never_reach_the_model() {
    let reasoning_part = Part::reasoning(
        "m1.p0".to_string(),
        "Thinking about how to answer.".to_string(),
        None,
    );

    let msgs = vec![Message {
        id: "m1".to_string(),
        role: MessageRole::Assistant,
        parts: vec![
            reasoning_part.clone(),
            part(PartKind::Prose, "Here is the answer."),
        ]
        .into(),
        origin: None,
        reply_marker: None,
    }];

    // JSON round-trip preserves the reasoning part — the snapshot
    // layer must not silently drop it, otherwise replays would lose
    // the trace.
    let serialized = serde_json::to_string(&msgs).expect("serialize messages");
    let deserialized: Vec<Message> =
        serde_json::from_str(&serialized).expect("deserialize messages");
    assert_eq!(deserialized[0].parts.len(), 2);
    assert!(matches!(
        deserialized[0].parts[0].kind(),
        PartKind::Reasoning
    ));
    assert_eq!(
        deserialized[0].parts[0].content(),
        "Thinking about how to answer."
    );

    // But the rendered LLM prompt must NOT include the reasoning
    // content in any assistant TEXT block — reasoning travels as its
    // own block kind so adapters that don't understand it can drop
    // without corrupting the visible transcript.
    let rendered = render_structured_prompt(&msgs);
    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].role, LlmRole::Assistant);
    // Without `reasoning_meta`, the reasoning part is dropped entirely,
    // so the assistant turn contains only the prose block.
    assert_eq!(rendered.messages[0].blocks.len(), 1);
    assert!(matches!(
        &rendered.messages[0].blocks[0],
        LlmContentBlock::Text { text, .. } if text.as_ref() == "Here is the answer."
    ));

    // When the assistant message consists solely of a display-only
    // reasoning part (no encrypted payload), no message is sent at
    // all.
    let reasoning_only = vec![Message {
        id: "m2".to_string(),
        role: MessageRole::Assistant,
        parts: vec![reasoning_part].into(),
        origin: None,
        reply_marker: None,
    }];
    let rendered_only = render_structured_prompt(&reasoning_only);
    assert!(rendered_only.messages.is_empty());
}

#[test]
fn prompt_resume_safety_rejects_unmatched_tool_calls() {
    let msgs = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::Assistant,
        parts: vec![Part::tool_call(
            "m0.p0".to_string(),
            r#"{"path":"README.md"}"#.to_string(),
            crate::ToolCallId::fixture("tc1"),
            "tc1".to_string(),
            "read_file".to_string(),
            None,
        )]
        .into(),
        origin: None,
        reply_marker: None,
    }];

    assert!(!messages_are_prompt_resume_safe(&msgs));
}

// ─── Reasoning-part roundtrip (fix 1.3b) ──────────────────────────
//
// Provider reasoning items can carry replay metadata that the adapter
// re-emits on the next turn. The session-model layer stores these parts
// so they survive resume/snapshot and flows them through as
// `kind == "reasoning"` LlmMessages.

#[test]
fn fig1123_only_committed_turn_inputs_start_genuine_user_segments() {
    let messages = vec![
        Message {
            id: "genuine".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "genuine")].into(),
            origin: Some(MessageOrigin::TurnInput {
                turn_id: TurnId::from("turn"),
                input_id: None,
            }),
            reply_marker: None,
        },
        Message {
            id: "call".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "call.p0".to_string(),
                "{}".to_string(),
                crate::ToolCallId::fixture("synthetic"),
                "synthetic".to_string(),
                "tool".to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "synthetic".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "synthetic.p0".to_string(),
                vec![crate::ModelToolReturnPart::text("synthetic")],
                crate::ToolCallId::fixture("synthetic"),
                "tool".to_string(),
                crate::ToolCallStatus::Success,
                None,
            )]
            .into(),
            origin: Some(MessageOrigin::Plugin {
                plugin_id: "plugin".to_string(),
                transient: false,
            }),
            reply_marker: None,
        },
    ];

    let rendered = render_prompt(&messages);

    assert!(rendered.messages[0].starts_user_segment);
    assert!(!rendered.messages[2].starts_user_segment);
}

#[test]
fn message_origins_written_before_turn_input_provenance_still_deserialize() {
    // Snapshots written before FIG-972 have no turn-input origin: a user
    // message carried no origin at all, and plugin/process origins are
    // unchanged. All three shapes must still round-trip.
    let legacy = r#"[
        {
            "id":"m_turn_old_input","role":"User",
            "parts":[{"id":"m_turn_old_input.p0","kind":"Text","content":"hi"}]
        },
        {
            "id":"m1","role":"System",
            "parts":[{"id":"m1.p0","kind":"Text","content":"note"}],
            "origin":{"kind":"plugin","plugin_id":"compactor"}
        },
        {
            "id":"m2","role":"Event",
            "parts":[{"id":"m2.p0","kind":"Text","content":"woke"}],
            "origin":{"kind":"process","process_id":"p_00000000000070008000000000000001","event_type":"finished","sequence":3}
        }
    ]"#;
    let msgs: Vec<Message> = serde_json::from_str(legacy).expect("legacy snapshot");
    assert_eq!(msgs[0].origin, None);
    assert_eq!(
        msgs[1].origin,
        Some(MessageOrigin::Plugin {
            plugin_id: "compactor".to_string(),
            transient: false,
        })
    );
    assert_eq!(
        msgs[2].origin,
        Some(MessageOrigin::Process {
            process_id: ProcessId::from_minted(0x0000_0000_0000_7000_8000_0000_0000_0000 | 1),
            event_type: "finished".to_string(),
            sequence: 3,
            wake_id: None,
            caused_by: None,
        })
    );
}

// ─── Part enum serde (FIG-3305) ─────────────────────────────────────
//
// `Part` is now an internally-tagged enum whose variants own only their
// kind's fields. The durable JSON is unchanged — the same flat shape
// every stored message already uses — and the compatibility reader
// rejects pairings the constructors cannot produce.

#[test]
fn legacy_flat_json_pairs_rejected_when_the_kind_cannot_carry_the_field() {
    // A Text part with a call id was representable in the old flat
    // struct; the enum reader refuses it with a typed error.
    let bad = format!(
        r#"{{"id":"m.p0","kind":"Text","content":"x","call_id":"{}"}}"#,
        crate::ToolCallId::fixture("call-1")
    );
    let err = serde_json::from_str::<Part>(&bad).expect_err("invalid pairing must fail");
    assert!(
        err.to_string().contains("call_id"),
        "typed error names the offending field: {err}"
    );

    // A tool result missing its call pair is likewise unrepresentable.
    let missing = r#"{"id":"m.p0","kind":"ToolResult","blocks":[]}"#;
    let err = serde_json::from_str::<Part>(missing).expect_err("missing call pair must fail");
    assert!(
        err.to_string().contains("call_id"),
        "typed error names the missing field: {err}"
    );

    // A tool result's content is its ordered blocks: the retired text-only
    // shape (a `content` string, no `blocks`) is refused, not coerced.
    let text_only = r#"{"id":"m.p0","kind":"ToolResult","content":"x","tool_call_id":"call-1","tool_name":"lookup"}"#;
    let err = serde_json::from_str::<Part>(text_only).expect_err("text-only result must fail");
    assert!(
        err.to_string().contains("content"),
        "typed error names the retired field: {err}"
    );
    let no_blocks =
        r#"{"id":"m.p0","kind":"ToolResult","tool_call_id":"call-1","tool_name":"lookup"}"#;
    let err = serde_json::from_str::<Part>(no_blocks).expect_err("missing blocks must fail");
    assert!(err.to_string().contains("blocks"), "{err}");
    let blocks_on_text = r#"{"id":"m.p0","kind":"Text","content":"x","blocks":[]}"#;
    serde_json::from_str::<Part>(blocks_on_text).expect_err("blocks belong to tool results");

    // An attachment answers no call: the retired tool-result attachment
    // pairing is rejected.
    let lone = r#"{"id":"m.p0","kind":"Attachment","content":"","tool_call_id":"call-1"}"#;
    serde_json::from_str::<Part>(lone).expect_err("attachment tool_call_id must fail");
}

#[test]
fn tool_result_attachments_are_counted_and_distinctly_identified() {
    let first = AttachmentSource::stored(test_attachment_ref(1));
    let second = AttachmentSource::stored(test_attachment_ref(2));
    let result = Part::tool_result(
        "m1.p0".into(),
        vec![
            crate::ModelToolReturnPart::Attachment(first.clone()),
            crate::ModelToolReturnPart::text("between"),
            crate::ModelToolReturnPart::Attachment(second.clone()),
        ],
        crate::ToolCallId::fixture("call-1"),
        "shot".into(),
        crate::ToolCallStatus::Success,
        None,
    );
    assert_eq!(
        result.identified_attachment_sources(),
        vec![
            ("m1.p0#1".to_string(), &first),
            ("m1.p0#2".to_string(), &second),
        ]
    );
    let msgs = vec![
        Message {
            id: "m0".into(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m0.p0".into(),
                "{}".into(),
                crate::ToolCallId::fixture("call-1"),
                "call-1".into(),
                "shot".into(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m1".into(),
            role: MessageRole::User,
            parts: vec![result].into(),
            origin: None,
            reply_marker: None,
        },
    ];
    assert_eq!(render_prompt(&msgs).attachments(), vec![&first, &second]);
}
