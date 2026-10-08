//! HISTORY (FIG-5257): an attachment-omission history policy narrows the
//! request's view only. The stored history keeps every attachment, and the
//! view keeps every message, role, part and tool call/result pairing; a
//! decision naming a part that carries no attachment changes nothing.

use super::*;
use crate::{AttachmentRef, MessageRole, ModelToolReturnPart, ToolCallId};

fn image(id: &str) -> AttachmentRef {
    crate::AttachmentRef {
        id: crate::AttachmentId::parse(
            format!("{:02x}", id.bytes().fold(0u8, u8::wrapping_add)).repeat(32),
        )
        .expect("valid attachment id"),
        media_type: crate::MediaType::parse("image/png").expect("valid media type"),
        byte_len: 3,
        type_metadata: None,
        label: None,
    }
}

fn message(id: &str, role: MessageRole, parts: Vec<Part>) -> Message {
    Message {
        id: id.to_string(),
        role,
        parts: parts.into(),
        origin: None,
        reply_marker: None,
    }
}

fn history() -> Vec<Message> {
    let call = ToolCallId::fixture("look");
    vec![
        message(
            "u1",
            MessageRole::User,
            vec![
                Part::text("u1.p0".into(), "see this".into(), None),
                Part::attachment_part(
                    "u1.p1".into(),
                    String::new(),
                    Some(crate::session_model::message::PartAttachment {
                        reference: image("u1"),
                    }),
                ),
            ],
        ),
        message(
            "a1",
            MessageRole::Assistant,
            vec![Part::tool_call(
                "a1.p0".into(),
                "{}".into(),
                call.clone(),
                "provider-call".into(),
                "look".into(),
                None,
            )],
        ),
        message(
            "t1",
            MessageRole::User,
            vec![Part::tool_result(
                "t1.p0".into(),
                vec![
                    ModelToolReturnPart::text("before"),
                    ModelToolReturnPart::Attachment(image("t1")),
                    ModelToolReturnPart::text("after"),
                ],
                call,
                "look".into(),
            )],
        ),
        message(
            "u2",
            MessageRole::User,
            vec![Part::text("u2.p0".into(), "latest".into(), None)],
        ),
    ]
}

fn encoded(messages: &[Message]) -> serde_json::Value {
    serde_json::to_value(messages).expect("messages encode")
}

fn named(message: &str, part: usize) -> HistoryPartId {
    HistoryPartId {
        message: message.into(),
        part,
    }
}

/// One message's id and role, and each part's kind and tool call identity.
type MessageShape = (String, MessageRole, Vec<(PartKind, Option<String>)>);

/// The shape a request depends on, message by message.
fn shape(messages: &[Message]) -> Vec<MessageShape> {
    messages
        .iter()
        .map(|message| {
            (
                message.id.clone(),
                message.role,
                message
                    .parts
                    .iter()
                    .map(|part| (part.kind(), part.call_id().map(ToString::to_string)))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn omission_changes_neither_stored_history_nor_tool_pairing() {
    let stored = history();
    let mut view = stored.clone();
    let omissions = [
        named("u1", 1),
        named("t1", 0),
        // A text part and a part or message that does not exist: no effect.
        named("u2", 0),
        named("u2", 9),
        named("missing", 0),
    ]
    .into_iter()
    .collect();

    assert_eq!(apply_attachment_omissions(&mut view, &omissions), 2);

    assert_eq!(
        encoded(&stored),
        encoded(&history()),
        "the stored history keeps its attachments"
    );
    assert_eq!(
        shape(&view),
        shape(&stored),
        "the view keeps every message and pairing"
    );
    assert!(view[0].parts[1].attachment().is_none());
    assert_eq!(view[0].parts[1].content(), OMITTED_ATTACHMENT_PLACEHOLDER);
    assert_eq!(view[0].parts[0].content(), "see this");
    assert_eq!(
        view[2].parts[0].tool_result_content(),
        Some(
            &[
                ModelToolReturnPart::text("before"),
                ModelToolReturnPart::text(OMITTED_ATTACHMENT_PLACEHOLDER),
                ModelToolReturnPart::text("after"),
            ][..]
        )
    );
    assert_eq!(
        encoded(&view[1..2]),
        encoded(&stored[1..2]),
        "the tool call is unchanged"
    );
    assert_eq!(
        encoded(&view[3..]),
        encoded(&stored[3..]),
        "a named text part is unchanged"
    );
}
