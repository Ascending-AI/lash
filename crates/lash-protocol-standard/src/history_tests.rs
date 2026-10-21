use super::*;
use lash_protocol_types::ToolCallId;
use lash_protocol_types::{AttachmentId, MediaType, PartAttachment};
fn part(kind: PartKind, content: &str) -> Part {
    match kind {
        PartKind::Text => Part::text("p0".into(), content.into(), None),
        PartKind::Prose => Part::prose("p0".into(), content.into(), None),
        _ => panic!("unsupported test part {kind:?}"),
    }
}

fn test_attachment_ref(byte_len: u64) -> AttachmentRef {
    AttachmentRef {
        id: AttachmentId::parse("3cd0b0330a44b423a583def0f3795fdc194067a489c1f3e1bfe61ba204380423")
            .expect("valid attachment id"),
        media_type: MediaType::parse("image/png").unwrap(),
        byte_len,
        type_metadata: None,
        label: None,
    }
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
    assert_eq!(rendered.attachments().count(), 1);
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
                ToolCallId::fixture("tc1"),
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

/// Mode choice controls wording; core preserves structured history and never
/// reuses text rendered under a different policy object.
#[test]
fn mode_history_policy_selects_text_without_changing_records_or_order() {
    let messages = vec![
        Message {
            id: "note".into(),
            role: MessageRole::System,
            parts: vec![Part::output("note.p0".into(), "first".into())].into(),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "input".into(),
            role: MessageRole::User,
            parts: vec![
                Part::text("input.p0".into(), "second".into(), None),
                attachment_part(b"image"),
            ]
            .into(),
            origin: None,
            reply_marker: None,
        },
    ];
    let stored = serde_json::to_value(&messages).unwrap();
    let cache = lash_protocol_types::BaseRenderCache::new();
    let standard = presentation();
    let rendered = cache.rendered(&messages, &standard);
    assert_eq!(
        block_text(&rendered[0], 0),
        "Runtime note:\n<output>\nfirst\n</output>"
    );
    assert_eq!(block_text(&rendered[1], 0), "second");
    let replacement = HistoryPresentation::new(|_, part| format!("chosen: {}", part.content()));
    let replaced = cache.rendered(&messages, &replacement);
    assert_eq!(block_text(&replaced[0], 0), "chosen: first");
    assert_eq!(block_text(&replaced[1], 0), "chosen: second");
    assert_eq!(replaced[0].role, rendered[0].role);
    assert_eq!(replaced[1].role, rendered[1].role);
    assert_eq!(replaced[1].blocks[1], rendered[1].blocks[1]);
    assert_eq!(cache.rendered(&messages, &standard), rendered);
    assert_eq!(serde_json::to_value(&messages).unwrap(), stored);
}
