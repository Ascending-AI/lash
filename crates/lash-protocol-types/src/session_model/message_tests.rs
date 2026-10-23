use super::*;
use crate::AttachmentRef;

fn part(kind: PartKind, content: &str) -> Part {
    Part::base("p0".to_string(), kind, content.to_string())
}

fn test_attachment_ref(byte_len: u64) -> AttachmentRef {
    AttachmentRef {
        id: crate::AttachmentId::parse(
            "3cd0b0330a44b423a583def0f3795fdc194067a489c1f3e1bfe61ba204380423",
        )
        .expect("valid attachment id"),
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
    sequence.render_prompt(&crate::session_model::message::HistoryPresentation::verbatim());
    sequence.push(Message {
        id: "result".into(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::tool_result(
            "result.p0".into(),
            vec![ModelToolReturnPart::text("resolved")],
            call_id,
            "lookup".into(),
            crate::ToolCallStatus::Success,
        )]),
        origin: None,
        reply_marker: None,
    });
    let rendered =
        sequence.render_prompt(&crate::session_model::message::HistoryPresentation::verbatim());
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
            reference: test_attachment_ref(bytes.len() as u64),
        }),
    )
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
            )]
            .into(),
            origin: None,
            reply_marker: None,
        },
    ];

    let rendered = render_structured_prompt(&msgs);
    assert_eq!(rendered.messages.len(), 4);
    assert_eq!(rendered.messages[0].role, LlmRole::System);
    assert_eq!(block_text(&rendered.messages[0], 0), "note");
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
    assert_eq!(rendered.attachments().count(), 1);
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

/// A cell's assistant context carries the call that ran the cell and no
/// tool result: the cell record answers it (FIG-5527). The same call in a
/// message that names no cell is unanswered.
#[test]
fn prompt_resume_safety_accepts_a_cell_contexts_call_without_a_result() {
    let context = |cell_id: Option<&str>| {
        vec![Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m0.p0".to_string(),
                r#"{"code":"finish(1)"}"#.to_string(),
                crate::ToolCallId::fixture("tc1"),
                "provider-call-1".to_string(),
                "execute_code".to_string(),
                None,
            )]
            .into(),
            origin: Some(MessageOrigin::TurnOutput {
                turn_id: lash_vocabulary::identity::TurnId::from("turn-1"),
                source: crate::TurnOutputSource::Plugin {
                    plugin_id: "rlm_protocol".to_string(),
                },
                cell_id: cell_id.map(str::to_string),
            }),
            reply_marker: None,
        }]
    };
    assert!(messages_are_prompt_resume_safe(&context(Some("cell-1"))));
    assert!(!messages_are_prompt_resume_safe(&context(None)));
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
    // message carried no origin at all, and plugin origins are unchanged.
    // Both shapes must still round-trip.
    let legacy = r#"[
        {
            "id":"m_turn_old_input","role":"User",
            "parts":[{"id":"m_turn_old_input.p0","kind":"Text","content":"hi"}]
        },
        {
            "id":"m1","role":"System",
            "parts":[{"id":"m1.p0","kind":"Text","content":"note"}],
            "origin":{"kind":"plugin","plugin_id":"compactor"}
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
    let first = test_attachment_ref(1);
    let second = test_attachment_ref(2);
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
    );
    assert_eq!(
        result.identified_attachments(),
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
    assert_eq!(
        render_prompt(&msgs).attachments().collect::<Vec<_>>(),
        vec![&first, &second]
    );
}

/// Sharing a sequence retains an immutable snapshot: append, rewrite and
/// replacement affect only the writer, including after materialization.
/// The flat serialized value remains the message list (FIG-5962).
#[test]
fn shared_sequence_mutation_preserves_snapshots_and_wire_values() {
    let first = witness_message("m0", "one");
    let second = witness_message("m1", "two");
    let expected = vec![first.clone(), second.clone()];
    let sequences = [
        MessageSequence::from_owned(expected.clone()),
        MessageSequence::from_base(AppendVec::from(expected.clone())),
        MessageSequence::from_base_and_delta(AppendVec::from(vec![first]), vec![second]),
    ];
    let expected_bytes = serde_json::to_vec(&expected).unwrap();
    for original in sequences {
        let _flattened = original.shared();
        let mut writer = original.clone();
        let retained = writer.clone();
        writer.push(witness_message("m2", "three"));
        assert_eq!(writer.len(), 3);
        assert_eq!(writer.shared().as_slice()[2].id, "m2");
        writer.extend(vec![witness_message("m3", "four")]);
        assert_eq!(writer.len(), 4);
        assert_eq!(writer.shared().as_slice()[3].id, "m3");
        let appended = writer.clone();
        let appended_bytes = serde_json::to_vec(&appended).unwrap();
        writer.make_mut()[0] = witness_message("m0", "rewritten");
        assert_eq!(
            writer.shared().as_slice()[0].parts[0].content(),
            "rewritten"
        );
        writer.replace(vec![witness_message("replacement", "new history")]);
        assert_eq!(writer.len(), 1);
        assert_eq!(writer.shared().as_slice()[0].id, "replacement");
        assert_eq!(serde_json::to_vec(&original).unwrap(), expected_bytes);
        assert_eq!(serde_json::to_vec(&retained).unwrap(), expected_bytes);
        assert_eq!(serde_json::to_vec(&appended).unwrap(), appended_bytes);
    }
}

/// FIG-7225: extending a validated frame and checking boundaries must inspect
/// only its new messages, even when a snapshot still holds the old prefix.
#[test]
fn resume_safety_validation_visits_only_the_appended_messages() {
    let mut base = AppendVec::new();
    let mut cache = Arc::new(BaseRenderCache::new());
    let mut held = Vec::new();
    safety::MESSAGE_VISITS.with(|visits| visits.set(0));
    for turn in 0..128 {
        let mut sequence =
            MessageSequence::from_base(base.clone()).with_base_render_cache(cache.clone());
        sequence.push(witness_message(&format!("u{turn}"), "input"));
        assert_eq!(sequence.prompt_resume_safe_len(), sequence.len());
        assert_eq!(sequence.prompt_resume_safe_len(), sequence.len());
        held.push(sequence.clone());
        let prefix_len = base.len();
        base = sequence.shared();
        cache = Arc::new(BaseRenderCache::extending(cache, prefix_len));
    }
    safety::MESSAGE_VISITS
        .with(|visits| assert!(visits.get() <= 2 * 128, "{} message visits", visits.get()));
    assert_eq!(held[0].prompt_resume_safe_len(), 1);
}

fn safety_call(id: &str) -> Message {
    Message {
        id: format!("call-{id}"),
        role: MessageRole::Assistant,
        parts: shared_parts(vec![Part::tool_call(
            format!("call-{id}.p0"),
            "{}".into(),
            crate::ToolCallId::fixture(id),
            format!("provider-{id}"),
            "tool".into(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    }
}

fn safety_result(id: &str) -> Message {
    Message {
        id: format!("result-{id}"),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::tool_result(
            format!("result-{id}.p0"),
            vec![ModelToolReturnPart::text("ok")],
            crate::ToolCallId::fixture(id),
            "tool".into(),
            crate::ToolCallStatus::Success,
        )]),
        origin: None,
        reply_marker: None,
    }
}

/// FIG-7225: ordered pairing refusals survive memoization, cloning and a
/// restored list. A rewrite must discard any validation of the old list.
#[test]
fn resume_safety_keeps_ordered_refusals_after_rewrites_and_restore() {
    let call = safety_call("a");
    let result = safety_result("a");
    let text = witness_message("text", "hello");
    let mut wrong_call = call.clone();
    wrong_call.role = MessageRole::User;
    let mut wrong_result = result.clone();
    wrong_result.role = MessageRole::Assistant;
    let mut cell = call.clone();
    cell.origin = Some(MessageOrigin::TurnOutput {
        turn_id: lash_vocabulary::identity::TurnId::from("turn"),
        source: crate::TurnOutputSource::Plugin {
            plugin_id: "cell".into(),
        },
        cell_id: Some("cell".into()),
    });
    let unvalidated = MessageSequence::from_owned(vec![call.clone()]);
    let mut appended = unvalidated.clone();
    appended.push(result.clone());
    assert!(appended.is_prompt_resume_safe());
    assert!(!unvalidated.is_prompt_resume_safe());
    let unvalidated = MessageSequence::from_owned(vec![call.clone()]);
    let mut extended = unvalidated.clone();
    extended.extend(vec![result.clone()]);
    assert!(!unvalidated.is_prompt_resume_safe());
    assert!(extended.is_prompt_resume_safe());
    let cache = Arc::new(BaseRenderCache::new());
    let safe =
        MessageSequence::from_base(vec![text.clone()].into()).with_base_render_cache(cache.clone());
    assert!(safe.is_prompt_resume_safe());
    let pending =
        MessageSequence::from_base(vec![call.clone()].into()).with_base_render_cache(cache);
    assert!(
        !pending.is_prompt_resume_safe(),
        "a render cache is not validation of unrelated messages"
    );
    for (messages, expected) in [
        (vec![text.clone(), call.clone()], 1),
        (vec![call.clone(), result.clone()], 2),
        (vec![call.clone(), call.clone(), result.clone()], 0),
        (vec![call.clone(), result.clone(), call.clone()], 2),
        (vec![call.clone(), result.clone(), result.clone()], 2),
        (vec![result.clone(), call.clone()], 0),
        (vec![wrong_call, result.clone()], 0),
        (vec![call.clone(), wrong_result], 0),
        (vec![cell], 1),
        (
            vec![
                call.clone(),
                safety_call("b"),
                safety_result("b"),
                result.clone(),
            ],
            4,
        ),
    ] {
        let mut sequence = MessageSequence::default();
        for message in &messages {
            sequence.push(message.clone());
            // Force the next append to update the existing validated cursor.
            let _ = sequence.prompt_resume_safe_len();
        }
        let snapshot = sequence.clone();
        let restored: MessageSequence =
            serde_json::from_str(&serde_json::to_string(&sequence).unwrap()).unwrap();
        assert_eq!(sequence.prompt_resume_safe_len(), expected);
        assert_eq!(restored.prompt_resume_safe_len(), expected);
        assert_eq!(sequence.is_prompt_resume_safe(), expected == messages.len());
        assert_eq!(prompt_resume_safe_len(&messages), expected);
        assert_eq!(
            messages_are_prompt_resume_safe(&messages),
            expected == messages.len()
        );
        sequence.replace(vec![call.clone()]);
        assert!(!sequence.is_prompt_resume_safe());
        sequence.extend(vec![result.clone()]);
        assert!(sequence.is_prompt_resume_safe());
        sequence.make_mut().swap(0, 1);
        assert_eq!(sequence.prompt_resume_safe_len(), 0);
        assert_eq!(snapshot.prompt_resume_safe_len(), expected);
    }
}

/// FIG-7225 and ownership rule 2: check and append share one lock; racing
/// branches cannot adopt a safe cut for a result with another call identity.
#[test]
fn resume_safety_racing_branches_keep_their_own_call_identity() {
    let mut pending = MessageSequence::from_owned(vec![safety_call("a")]);
    assert_eq!(pending.prompt_resume_safe_len(), 0);
    let barrier = std::sync::Barrier::new(2);
    let (answered, refused) = std::thread::scope(|scope| {
        let mut answered = pending.clone();
        let mut refused = pending.clone();
        let barrier = &barrier;
        let answered = scope.spawn(move || {
            barrier.wait();
            answered.push(safety_result("a"));
            answered.prompt_resume_safe_len()
        });
        let refused = scope.spawn(move || {
            barrier.wait();
            refused.push(safety_result("b"));
            refused.prompt_resume_safe_len()
        });
        (answered.join().unwrap(), refused.join().unwrap())
    });
    assert_eq!(answered, 2);
    assert_eq!(refused, 0);
    assert_eq!(pending.prompt_resume_safe_len(), 0);
    pending.push(safety_result("a"));
    assert!(pending.is_prompt_resume_safe());
}
