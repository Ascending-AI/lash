use lash_core::facade_support::BorrowedChronologicalMessage;
use lash_core::{MessageRole, PartKind};
use lash_rlm_types::{RlmAttachmentRef, RlmHistoryItem, RlmHistoryRole, RlmProtocolEvent};

/// Version of the RLM payload nested in a session-history protocol event.
/// version_surface = "migrate"
/// format_manifest = "RlmProtocolEvent"
/// version_guard(roots(RlmEventEnvelope))
pub const RLM_PROTOCOL_EVENT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RlmEventEnvelope {
    format: u32,
    event: RlmProtocolEvent,
}

#[expect(
    clippy::expect_used,
    reason = "the typed RLM history envelope serializes"
)]
pub fn rlm_protocol_event(
    event: RlmProtocolEvent,
    schema_version: u32,
) -> lash_core::ProtocolEvent {
    lash_core::ProtocolEvent::typed(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        RlmEventEnvelope {
            format: schema_version,
            event,
        },
    )
    .expect("RLM protocol events serialize")
}

#[cfg(test)]
thread_local! {
    pub(crate) static EVENT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read the RLM event a session-history protocol event carries: `None` for
/// another plugin's event, typed corruption for an unstamped, foreign-format or
/// undecodable payload. The inverse of [`rlm_protocol_event`].
///
/// The event is decoded once per retained payload and shared by every
/// reader of it: the transcript, the history index and a restore read the
/// same decode, and a rewritten payload decodes afresh (FIG-6443).
pub fn decode_rlm_protocol_event(
    event: &lash_core::ProtocolEvent,
) -> Result<Option<std::sync::Arc<RlmProtocolEvent>>, lash_core::StoredDataCorruption> {
    if event.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Ok(None);
    }
    let decoded = event
        .payload
        .derived(|payload| DecodedRlmEvent(decode_rlm_payload(payload).map(std::sync::Arc::new)));
    decoded.0.clone().map(Some)
}

/// What one retained RLM payload decodes to, kept with the payload.
struct DecodedRlmEvent(Result<std::sync::Arc<RlmProtocolEvent>, lash_core::StoredDataCorruption>);

fn decode_rlm_payload(
    payload: &serde_json::Value,
) -> Result<RlmProtocolEvent, lash_core::StoredDataCorruption> {
    #[cfg(test)]
    EVENT_DECODES.with(|count| count.set(count.get() + 1));
    let corrupt = |message: String| lash_core::StoredDataCorruption {
        record_kind: "RLM protocol event".into(),
        message,
    };
    #[derive(serde::Deserialize)]
    struct Stamp {
        format: u32,
    }
    let stamp: Stamp =
        serde::Deserialize::deserialize(payload).map_err(|error| corrupt(error.to_string()))?;
    if !lash_core::store::upcast_chain_covers(
        lash_core::surface_format!(RLM_PROTOCOL_EVENT_VERSION),
        stamp.format,
        RLM_PROTOCOL_EVENT_VERSION,
    ) {
        return Err(corrupt(format!(
            "unsupported format {}, expected {RLM_PROTOCOL_EVENT_VERSION}",
            stamp.format
        )));
    }
    let envelope: RlmEventEnvelope =
        serde::Deserialize::deserialize(payload).map_err(|error| corrupt(error.to_string()))?;
    Ok(envelope.event)
}

/// Whether a message is the RLM protocol's own durable output, judged by its
/// typed origin alone: the one classifier every RLM projection, prompt-side
/// and render-side, asks. A host or another plugin writing on the same
/// channel carries its own provenance and is never classified as internal.
pub fn is_rlm_protocol_output(origin: Option<&lash_core::MessageOrigin>) -> bool {
    match origin {
        Some(lash_core::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        })
        | Some(lash_core::MessageOrigin::TurnOutput {
            source: lash_core::TurnOutputSource::Plugin { plugin_id },
            ..
        }) => plugin_id == crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        _ => false,
    }
}

fn history_item_from_message(message: BorrowedChronologicalMessage<'_>) -> Option<RlmHistoryItem> {
    // A native provider exchange is replayed as its call/result pair and is
    // the cell's own step in `history`, never a message beside it.
    if crate::native::transport::is_exchange_message(message.origin, message.parts) {
        return None;
    }
    let content = message_history_text(message.parts);
    let attachments = message
        .parts
        .iter()
        .flat_map(|part| {
            part.identified_attachments()
                .into_iter()
                .map(|(id, attachment)| RlmAttachmentRef {
                    id,
                    reference: attachment.clone(),
                })
        })
        .collect::<Vec<_>>();
    if content.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(RlmHistoryItem::Message {
        id: message.id.to_owned(),
        role: history_role(message.role),
        content,
        attachments,
    })
}

fn history_item_from_lash_vm_step(entry: &lash_core::CellRecord) -> RlmHistoryItem {
    RlmHistoryItem::from_cell_record(entry)
}

fn message_history_text(parts: &[lash_core::Part]) -> String {
    let chunks = parts
        .iter()
        .filter(|part| matches!(part.kind(), PartKind::Text | PartKind::Prose))
        .filter_map(|part| part.text_content())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    chunks.join("\n\n")
}

fn history_role(role: MessageRole) -> RlmHistoryRole {
    match role {
        MessageRole::User => RlmHistoryRole::User,
        MessageRole::System => RlmHistoryRole::System,
        MessageRole::Assistant => RlmHistoryRole::Assistant,
        MessageRole::Event => RlmHistoryRole::Event,
    }
}

/// Project exactly one indexed entry, preserving the transcript's item shape.
pub(crate) fn history_item(
    payload: lash_core::facade_support::BorrowedChronologicalPayload<'_>,
) -> Result<Option<RlmHistoryItem>, lash_core::StoredDataCorruption> {
    use lash_core::facade_support::BorrowedChronologicalPayload;
    Ok(match payload {
        BorrowedChronologicalPayload::Message(message) => history_item_from_message(message),
        BorrowedChronologicalPayload::ProtocolEvent(event) => {
            match decode_rlm_protocol_event(event)?.as_deref() {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                    Some(history_item_from_lash_vm_step(step))
                }
                _ => None,
            }
        }
    })
}
