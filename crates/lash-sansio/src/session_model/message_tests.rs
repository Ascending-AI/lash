/// Commit-identity tripwire: `Part` is `#[non_exhaustive]`, so downstream
/// exhaustive destructures (e.g. lash-core's commit-identity hasher) are
/// forced to carry `..` and no longer break the compile when a field is
/// added. This in-crate exhaustive destructure restores that tripwire:
/// adding a `Part` field fails here, forcing a deliberate decision about
/// commit-identity inclusion and constructor coverage.
#[test]
fn part_field_additions_trip_this_exhaustive_destructure() {
    let part = super::Part::text("m0.p0".into(), "x".into(), None);
    let super::Part {
        id: _,
        kind: _,
        content: _,
        attachment: _,
        tool_call_id: _,
        tool_name: _,
        tool_replay: _,
        prune_state: _,
        reasoning_meta: _,
        response_meta: _,
    } = part;
}

use super::*;
use crate::AttachmentRef;
use crate::llm::types::ProviderRouteIdentity;

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
    }
}

#[test]
fn a_shared_base_witnesses_the_preserved_prefix_and_names_the_delta() {
    let base = Arc::new(vec![witness_message("m0", "one")]);
    let current = MessageSequence::from_base(Arc::clone(&base));
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
    let current = MessageSequence::from_base(Arc::new(vec![witness_message("m0", "one")]));
    let next = MessageSequence::from_base(Arc::new(vec![witness_message("m0", "one")]));

    assert!(current.preserved_extension_delta(&next).is_none());
}

#[test]
fn a_rebuilt_owned_sequence_drops_the_witness() {
    let base = Arc::new(vec![witness_message("m0", "one")]);
    let current = MessageSequence::from_base(Arc::clone(&base));
    let mut rewritten = MessageSequence::from_base(base);
    rewritten.replace(vec![witness_message("m0", "rewritten")]);

    assert!(current.preserved_extension_delta(&rewritten).is_none());
}

#[test]
fn a_diverged_delta_does_not_witness_and_a_shorter_one_does_not_either() {
    let base = Arc::new(vec![witness_message("m0", "one")]);
    let mut current = MessageSequence::from_base(Arc::clone(&base));
    current.push(witness_message("m1", "two"));

    let mut diverged = MessageSequence::from_base(Arc::clone(&base));
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
fn replay_carrying_constructors_preserve_provider_metadata() {
    let tool_replay = ProviderReplayMeta {
        item_id: Some("call-item".to_string()),
        opaque: Some("opaque-call-state".to_string()),
        origin: Some(ProviderRouteIdentity::new(
            "provider-a",
            "route-a",
            "model-a",
        )),
    };
    let tool_call = Part::tool_call(
        "m0.p0".to_string(),
        r#"{"path":"README.md"}"#.to_string(),
        "call-1".to_string(),
        "read_file".to_string(),
        Some(tool_replay.clone()),
    );

    let response_meta = ResponseTextMeta {
        id: Some("msg-item".to_string()),
        provider_payload: Some("opaque-message-state".to_string()),
        ..ResponseTextMeta::default()
    };
    let prose = Part::prose(
        "m0.p1".to_string(),
        "done".to_string(),
        Some(response_meta.clone()),
    );

    let reasoning_meta = ProviderReasoningReplay {
        item_id: Some("reasoning-item".to_string()),
        encrypted_content: Some("ciphertext".to_string()),
        ..ProviderReasoningReplay::default()
    };
    let reasoning = Part::reasoning(
        "m0.p2".to_string(),
        "summary".to_string(),
        Some(reasoning_meta.clone()),
    );

    assert_eq!(tool_call.tool_replay, Some(tool_replay));
    assert_eq!(prose.response_meta, Some(response_meta));
    assert_eq!(reasoning.reasoning_meta, Some(reasoning_meta));
}

#[test]
fn render_transcript_prompt_orders_turns_oldest_first() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "first")].into(),
            origin: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![part(PartKind::Prose, "reply one")].into(),
            origin: None,
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "second")].into(),
            origin: None,
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
        },
        Message {
            id: "m3".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "second")].into(),
            origin: None,
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
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m2.p0".to_string(),
                r#"{"path":"README.md"}"#.to_string(),
                "tc1".to_string(),
                "read_file".to_string(),
                None,
            )]
            .into(),
            origin: None,
        },
        Message {
            id: "m3".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "m3.p0".to_string(),
                "ok".to_string(),
                "tc1".to_string(),
                "read_file".to_string(),
            )]
            .into(),
            origin: None,
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
        LlmContentBlock::Attachment { attachment_idx: 0 }
    ));
    assert_eq!(rendered.attachments.len(), 1);
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
                "ask_1".to_string(),
                "ask".to_string(),
                None,
            )]
            .into(),
            origin: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "m1.p0".to_string(),
                String::new(),
                "ask_1".to_string(),
                "ask".to_string(),
            )]
            .into(),
            origin: None,
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
    }];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);
    assert!(text.contains("[Attachment]"));
    assert_eq!(rendered.attachments.len(), 1);
}

#[test]
fn render_transcript_prompt_omits_missing_assistant_placeholder_for_current_turn() {
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "first")].into(),
            origin: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![part(PartKind::Prose, "reply one")].into(),
            origin: None,
        },
        Message {
            id: "m2".to_string(),
            role: MessageRole::User,
            parts: vec![part(PartKind::Text, "second")].into(),
            origin: None,
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
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::tool_call(
                "m1.p0".to_string(),
                r#"{"cmd":"date"}"#.to_string(),
                "tc1".to_string(),
                "exec_command".to_string(),
                None,
            )]
            .into(),
            origin: None,
        },
    ];

    let rendered = render_transcript_prompt(&msgs);
    let text = block_text(&rendered.messages[0], 0);

    assert!(text.contains(r#"exec_command({"cmd":"date"})"#));
}

#[test]
fn render_transcript_prompt_omits_runtime_notes_section() {
    let msgs = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![part(PartKind::Text, "hi")].into(),
        origin: None,
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
                "tc1".to_string(),
                "read_file".to_string(),
                None,
            )]
            .into(),
            origin: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::tool_result(
                "m1.p0".to_string(),
                "ok".to_string(),
                "tc1".to_string(),
                "read_file".to_string(),
            )]
            .into(),
            origin: None,
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
    }];

    // JSON round-trip preserves the reasoning part — the snapshot
    // layer must not silently drop it, otherwise replays would lose
    // the trace.
    let serialized = serde_json::to_string(&msgs).expect("serialize messages");
    let deserialized: Vec<Message> =
        serde_json::from_str(&serialized).expect("deserialize messages");
    assert_eq!(deserialized[0].parts.len(), 2);
    assert!(matches!(deserialized[0].parts[0].kind, PartKind::Reasoning));
    assert_eq!(
        deserialized[0].parts[0].content,
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
            "tc1".to_string(),
            "read_file".to_string(),
            None,
        )]
        .into(),
        origin: None,
    }];

    assert!(!messages_are_prompt_resume_safe(&msgs));
}

// ─── Reasoning-part roundtrip (fix 1.3b) ──────────────────────────
//
// Provider reasoning items can carry replay metadata that the adapter
// re-emits on the next turn. The session-model layer stores these parts
// so they survive resume/snapshot and flows them through as
// `kind == "reasoning"` LlmMessages.

fn reasoning_part_fixture(encrypted: Option<&str>) -> Part {
    Part::reasoning(
        "m0.p0".to_string(),
        "Thinking.".to_string(),
        encrypted.map(|encrypted| ProviderReasoningReplay {
            item_id: Some("rs_xyz".to_string()),
            summary: vec!["Thinking.".to_string()],
            encrypted_content: Some(encrypted.to_string()),
            signature: None,
            redacted: false,
            origin: None,
        }),
    )
}

#[test]
fn reasoning_part_roundtrips_through_snapshot_serde() {
    let msgs = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::Assistant,
        parts: vec![reasoning_part_fixture(Some("CIPHER=="))].into(),
        origin: None,
    }];
    let serialized = serde_json::to_string(&msgs).expect("serialize");
    let deserialized: Vec<Message> = serde_json::from_str(&serialized).expect("deserialize");
    assert_eq!(deserialized[0].parts.len(), 1);
    let part = &deserialized[0].parts[0];
    assert!(matches!(part.kind, PartKind::Reasoning));
    let meta = part.reasoning_meta.as_ref().expect("meta survives");
    assert_eq!(meta.item_id.as_deref(), Some("rs_xyz"));
    assert_eq!(meta.summary, vec!["Thinking.".to_string()]);
    assert_eq!(meta.encrypted_content.as_deref(), Some("CIPHER=="));
}

#[test]
fn message_sequence_serializes_as_flat_message_array() {
    // The custom `MessageSequence` serde must produce exactly the same wire
    // form as a plain `Vec<Message>`. This is the invariant that lets
    // `Effect` be serialized directly in a turn checkpoint instead of
    // round-tripping through a parallel `Vec<Message>` twin — so existing
    // persisted checkpoints stay byte-compatible.
    let msgs = vec![
        Message {
            id: "m0".to_string(),
            role: MessageRole::Assistant,
            parts: vec![reasoning_part_fixture(None)].into(),
            origin: None,
        },
        Message {
            id: "m1".to_string(),
            role: MessageRole::Assistant,
            parts: vec![reasoning_part_fixture(Some("CIPHER=="))].into(),
            origin: None,
        },
    ];
    // Build via base+delta so the materialization path is exercised, not
    // just the trivial owned case.
    let sequence = MessageSequence::from_base_and_delta(
        Arc::new(vec![msgs[0].clone()]),
        vec![msgs[1].clone()],
    );

    assert_eq!(
        serde_json::to_value(&sequence).expect("serialize sequence"),
        serde_json::to_value(&msgs).expect("serialize vec"),
        "MessageSequence must serialize identically to Vec<Message>"
    );

    let decoded: MessageSequence = serde_json::from_value(serde_json::to_value(&sequence).unwrap())
        .expect("deserialize sequence");
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded.as_slice()[1].id, "m1");
}

#[test]
fn reasoning_parts_never_flow_to_rendered_prompt_as_text() {
    // Whether or not the reasoning item carries an encrypted blob,
    // it must NEVER be flattened into assistant text content.
    // Without an encrypted blob the adapter also drops it entirely
    // (no point re-feeding a display-only summary).
    let display_only = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::Assistant,
        parts: vec![reasoning_part_fixture(None)].into(),
        origin: None,
    }];
    let rendered = render_structured_prompt(&display_only);
    assert!(
        rendered.messages.is_empty(),
        "display-only reasoning must not reach the prompt"
    );

    // With encrypted content, a single Reasoning block is emitted
    // that adapters can re-emit via their native reasoning channel.
    let replayable = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::Assistant,
        parts: vec![reasoning_part_fixture(Some("CIPHER=="))].into(),
        origin: None,
    }];
    let rendered = render_structured_prompt(&replayable);
    assert_eq!(rendered.messages.len(), 1);
    match &rendered.messages[0].blocks[0] {
        LlmContentBlock::Reasoning { replay, .. } => {
            let replay = replay.as_ref().expect("reasoning replay");
            assert_eq!(replay.encrypted_content.as_deref(), Some("CIPHER=="));
            assert_eq!(replay.item_id.as_deref(), Some("rs_xyz"));
            assert_eq!(replay.summary, vec!["Thinking.".to_string()]);
        }
        other => panic!("expected Reasoning block, got {other:?}"),
    }
    // Sanity: transcript rendering never includes reasoning text.
    let transcript = render_transcript_prompt(&replayable);
    let transcript_text = block_text(&transcript.messages[0], 0);
    assert!(!transcript_text.contains("Thinking."));
    assert!(!transcript_text.contains("CIPHER=="));
}

#[test]
fn turn_input_origin_wire_shape_is_tagged_and_omits_an_absent_input_id() {
    let direct = Message {
        id: "m_turn_t1_input".to_string(),
        role: MessageRole::User,
        parts: vec![part(PartKind::Text, "hello")].into(),
        origin: Some(MessageOrigin::TurnInput {
            turn_id: "t1".to_string(),
            input_id: None,
        }),
    };
    assert_eq!(
        serde_json::to_value(&direct.origin).expect("serialize direct origin"),
        serde_json::json!({ "kind": "turn_input", "turn_id": "t1" }),
        "an absent input id must not appear on the wire"
    );

    let ingress = Message {
        id: "m_ingress_in-7".to_string(),
        role: MessageRole::User,
        parts: vec![part(PartKind::Text, "follow up")].into(),
        origin: Some(MessageOrigin::TurnInput {
            turn_id: "t1".to_string(),
            input_id: Some("in-7".to_string()),
        }),
    };
    assert_eq!(
        serde_json::to_value(&ingress.origin).expect("serialize ingress origin"),
        serde_json::json!({
            "kind": "turn_input",
            "turn_id": "t1",
            "input_id": "in-7",
        })
    );

    let msgs = vec![direct, ingress];
    let decoded: Vec<Message> =
        serde_json::from_str(&serde_json::to_string(&msgs).expect("serialize"))
            .expect("deserialize");
    assert_eq!(
        decoded[0].origin,
        Some(MessageOrigin::TurnInput {
            turn_id: "t1".to_string(),
            input_id: None,
        })
    );
    assert_eq!(
        decoded[1].origin,
        Some(MessageOrigin::TurnInput {
            turn_id: "t1".to_string(),
            input_id: Some("in-7".to_string()),
        })
    );
}

#[test]
fn turn_output_origin_wire_shape_preserves_typed_source() {
    let origin = MessageOrigin::TurnOutput {
        turn_id: "queued-drain-1".to_string(),
        source: TurnOutputSource::Plugin {
            plugin_id: "lash.rlm".to_string(),
        },
    };
    assert_eq!(
        serde_json::to_value(&origin).expect("serialize turn output origin"),
        serde_json::json!({
            "kind": "turn_output",
            "turn_id": "queued-drain-1",
            "source": { "kind": "plugin", "plugin_id": "lash.rlm" },
        })
    );
    assert_eq!(
        serde_json::from_value::<MessageOrigin>(
            serde_json::to_value(origin).expect("serialize turn output origin")
        )
        .expect("deserialize turn output origin"),
        MessageOrigin::TurnOutput {
            turn_id: "queued-drain-1".to_string(),
            source: TurnOutputSource::Plugin {
                plugin_id: "lash.rlm".to_string(),
            },
        }
    );
}

#[test]
fn message_origins_written_before_turn_input_provenance_still_deserialize() {
    // Snapshots written before FIG-972 have no turn-input origin: a user
    // message carried no origin at all, and plugin/process origins are
    // unchanged. All three shapes must still round-trip.
    let legacy = r#"[
        {
            "id":"m_turn_old_input","role":"User",
            "parts":[{"id":"m_turn_old_input.p0","kind":"Text","content":"hi","prune_state":"Intact"}]
        },
        {
            "id":"m1","role":"System",
            "parts":[{"id":"m1.p0","kind":"Text","content":"note","prune_state":"Intact"}],
            "origin":{"kind":"plugin","plugin_id":"compactor"}
        },
        {
            "id":"m2","role":"Event",
            "parts":[{"id":"m2.p0","kind":"Text","content":"woke","prune_state":"Intact"}],
            "origin":{"kind":"process","process_id":"p1","event_type":"finished","sequence":3}
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
            process_id: "p1".to_string(),
            event_type: "finished".to_string(),
            sequence: 3,
            wake_id: None,
            caused_by: None,
        })
    );
}

#[test]
fn reasoning_parts_are_zero_for_prune_accounting() {
    // The rolling-history plugin's prune logic is driven by
    // `prompt_char_count`. Reasoning parts are not user-visible,
    // so they must not count against the prompt budget.
    let part = reasoning_part_fixture(Some("X=="));
    assert_eq!(part.prompt_char_count(), 0);
}
