use std::collections::{BTreeMap, BTreeSet};

use lash_core::{
    Message, MessageRole, PartKind, RuntimeExecutionContext, facade_support::ChronologicalPayload,
};
use lash_rlm_types::{RlmAttachmentRef, RlmHistoryItem, RlmHistoryRole, RlmProtocolEvent};

use super::bindings::RlmProjectedBindings;

/// The name of the binding a cell reads the session's history through.
/// Reserved: no host binding may take it.
pub const HISTORY_PROJECTION: &str = "history";

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

/// Read the RLM event a session-history protocol event carries: `None` for
/// another plugin's event, typed corruption for an unstamped, foreign-format or
/// undecodable payload. The inverse of [`rlm_protocol_event`].
pub fn decode_rlm_protocol_event(
    event: &lash_core::ProtocolEvent,
) -> Result<Option<RlmProtocolEvent>, lash_core::StoredDataCorruption> {
    if event.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Ok(None);
    }
    let corrupt = |message: String| lash_core::StoredDataCorruption {
        record_kind: "RLM protocol event".into(),
        message,
    };
    #[derive(serde::Deserialize)]
    struct Stamp {
        format: u32,
    }
    let stamp: Stamp = serde_json::from_value(event.payload.clone())
        .map_err(|error| corrupt(error.to_string()))?;
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
    let envelope: RlmEventEnvelope = serde_json::from_value(event.payload.clone())
        .map_err(|error| corrupt(error.to_string()))?;
    Ok(Some(envelope.event))
}

#[derive(Clone, Debug)]
pub struct RlmHistoryProjection {
    history: Vec<RlmHistoryItem>,
    chronological_indices: BTreeMap<usize, usize>,
    suppressed_chronological_indices: BTreeSet<usize>,
}

impl RlmHistoryProjection {
    pub fn from_chronological(
        projection: &lash_core::facade_support::ChronologicalProjection,
    ) -> Result<Self, lash_core::StoredDataCorruption> {
        Self::from_entries(projection.entries())
    }

    /// The history of a transcript's entries; of a prefix of them, the
    /// history the transcript had when it was that long.
    pub(crate) fn from_entries(
        entries: &[lash_core::facade_support::ChronologicalEntry],
    ) -> Result<Self, lash_core::StoredDataCorruption> {
        let suppressed_chronological_indices = completed_turn_internal_indices(entries)?;
        let mut history = Vec::with_capacity(entries.len());
        let mut chronological_indices = BTreeMap::new();
        for entry in entries {
            if suppressed_chronological_indices.contains(&entry.index) {
                continue;
            }
            let item = match &entry.payload {
                ChronologicalPayload::Message(message) => history_item_from_message(message),
                ChronologicalPayload::ProtocolEvent(event) => {
                    match decode_rlm_protocol_event(event)? {
                        Some(RlmProtocolEvent::RlmAssistantContent(content)) => {
                            Some(RlmHistoryItem::Message {
                                id: content.id,
                                role: RlmHistoryRole::Assistant,
                                content: content.prose,
                                attachments: Vec::new(),
                            })
                        }
                        Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                            Some(history_item_from_lash_vm_step(&step))
                        }
                        _ => None,
                    }
                }
            };
            if let Some(item) = item {
                chronological_indices.insert(entry.index, history.len());
                history.push(item);
            }
        }
        Ok(Self {
            history,
            chronological_indices,
            suppressed_chronological_indices,
        })
    }

    /// Return the compact semantic `history[N]` index for a retained source
    /// entry. Protocol-internal entries suppressed by completed-turn
    /// precedence do not consume an index.
    pub(crate) fn projected_index_for_chronological(&self, index: usize) -> Option<usize> {
        self.chronological_indices.get(&index).copied()
    }

    pub(crate) fn suppresses_chronological(&self, index: usize) -> bool {
        self.suppressed_chronological_indices.contains(&index)
    }

    pub fn history(&self) -> &[RlmHistoryItem] {
        self.history.as_slice()
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    pub fn item(&self, index: usize) -> Option<RlmHistoryItem> {
        self.history.get(index).cloned()
    }

    pub fn value(&self) -> serde_json::Value {
        serde_json::to_value(&self.history).unwrap_or_else(|_| serde_json::Value::Array(vec![]))
    }
}

/// The relationship is derived from event provenance: protocol entries and a later assistant
/// message before the next user/event turn boundary share one completed turn.
/// Content is never compared.
/// Intermediate trajectory entries remain available, while their prose is represented by the
/// canonical transcript.
/// A terminal step with an archive stays addressable; its printed values
/// are independent of the canonical assistant answer. Without a committed
/// message, every terminal step remains unchanged.
fn completed_turn_internal_indices(
    entries: &[lash_core::facade_support::ChronologicalEntry],
) -> Result<BTreeSet<usize>, lash_core::StoredDataCorruption> {
    let mut suppressed = BTreeSet::new();
    let mut assistant_content_indices = Vec::new();
    let mut terminal_step = None;

    for entry in entries {
        match &entry.payload {
            ChronologicalPayload::Message(message) => match message.role {
                // A refused call's results are protocol output inside the
                // turn, not the next turn's input.
                MessageRole::User
                    if crate::native::transport::is_exchange_message(
                        message.origin.as_ref(),
                        &message.parts,
                    ) => {}
                MessageRole::User | MessageRole::Event => {
                    assistant_content_indices.clear();
                    terminal_step = None;
                }
                MessageRole::Assistant => {
                    if is_rlm_protocol_output(message.origin.as_ref()) {
                        assistant_content_indices.push(entry.index);
                    } else if history_item_from_message(message).is_some() {
                        if let Some(step_index) = terminal_step.take() {
                            suppressed.insert(step_index);
                        }
                        suppressed.extend(assistant_content_indices.drain(..));
                    }
                }
                MessageRole::System => {}
            },
            ChronologicalPayload::ProtocolEvent(event) => match decode_rlm_protocol_event(event)? {
                Some(RlmProtocolEvent::RlmAssistantContent(_)) => {
                    assistant_content_indices.push(entry.index);
                }
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                    terminal_step = step
                        .result
                        .finish()
                        .is_some()
                        .then_some(entry.index)
                        .filter(|_| step.prints_retained.is_none());
                }
                _ => {}
            },
        }
    }

    Ok(suppressed)
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

pub fn rlm_history_projection(
    projection: &lash_core::facade_support::ChronologicalProjection,
) -> Result<RlmHistoryProjection, lash_core::StoredDataCorruption> {
    RlmHistoryProjection::from_chronological(projection)
}

/// Why a cell has no host bindings to start with.
pub(crate) enum HostBindingsError {
    /// The step that records them failed.
    Journal(lash_core::RuntimeEffectControllerError),
    Refused(String),
}

/// Whether `source` names `name` as a whole word.
fn mentions(source: &str, name: &str) -> bool {
    let part = |character: char| character == '_' || character.is_alphanumeric();
    source.match_indices(name).any(|(at, _)| {
        !source[..at].chars().next_back().is_some_and(part)
            && !source[at + name.len()..].chars().next().is_some_and(part)
    })
}

/// The read-only bindings the cell running `source` starts with: the
/// session's, as the cell records them, and `history`, the session's
/// transcript as the cell's run started from it. `history` is bound only
/// for a cell whose source names it: a run's transcript is fixed, so the
/// value is the same on every drive of the cell.
pub(crate) async fn cell_host_bindings(
    ctx: &RuntimeExecutionContext<'_>,
    session_bindings: RlmProjectedBindings,
    source: &str,
) -> Result<BTreeMap<String, serde_json::Value>, HostBindingsError> {
    let recorded = match ctx
        .parent_invocation()
        .and_then(lash_core::RuntimeInvocation::effect_address)
    {
        Some(address) => session_bindings
            .journaled(ctx, &address.replay_key)
            .await
            .map_err(HostBindingsError::Journal)?,
        None => session_bindings,
    };
    let mut bindings = recorded.into_values();
    if bindings.contains_key(HISTORY_PROJECTION) {
        return Err(HostBindingsError::Refused(format!(
            "`{HISTORY_PROJECTION}` is reserved as an RLM built-in binding"
        )));
    }
    if mentions(source, HISTORY_PROJECTION) {
        let history = RlmHistoryProjection::from_chronological(&ctx.chronological_projection())
            .map_err(|error| HostBindingsError::Refused(error.to_string()))?;
        bindings.insert(HISTORY_PROJECTION.to_owned(), history.value());
    }
    Ok(bindings)
}

fn history_item_from_message(message: &Message) -> Option<RlmHistoryItem> {
    // A native provider exchange is replayed as its call/result pair and is
    // the cell's own step in `history`, never a message beside it.
    if crate::native::transport::is_exchange_message(message.origin.as_ref(), &message.parts) {
        return None;
    }
    let content = message_history_text(message);
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
        id: message.id.clone(),
        role: history_role(message.role),
        content,
        attachments,
    })
}

fn history_item_from_lash_vm_step(entry: &lash_core::CellRecord) -> RlmHistoryItem {
    RlmHistoryItem::from_cell_record(entry)
}

fn message_history_text(message: &Message) -> String {
    let chunks = message
        .parts
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::CellRecord;

    #[test]
    fn corrupt_rlm_history_refuses_projection_and_transcript() {
        use lash_core::transcript::TranscriptDecoderPlugin as _;
        let foreign = lash_core::ProtocolEvent {
            plugin_id: "foreign".into(),
            payload: serde_json::json!(null),
        };
        assert!(
            decode_rlm_protocol_event(&foreign)
                .expect("foreign event")
                .is_none()
        );
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"format": u32::MAX}),
            serde_json::json!({"format": RLM_PROTOCOL_EVENT_VERSION, "event": null}),
        ] {
            let event = lash_core::ProtocolEvent {
                plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.into(),
                payload,
            };
            let error = decode_rlm_protocol_event(&event).expect_err("our corrupt event refuses");
            assert_eq!(error.record_kind, "RLM protocol event");
            let events = [lash_core::SessionHistoryRecord::Protocol(event.clone())];
            let projection = lash_core::facade_support::ChronologicalProjection::from_turn_view(
                &events,
                &Default::default(),
            );
            assert!(rlm_history_projection(&projection).is_err());
            assert!(
                crate::projection::transcript::RlmTranscriptDecoder
                    .decode_event(&event)
                    .is_err()
            );
        }
    }

    fn message(id: &str, role: MessageRole, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: lash_core::facade_support::shared_parts(vec![lash_core::Part::text(
                format!("{id}.p0"),
                text.to_string(),
                None,
            )]),
            origin: None,
            reply_marker: None,
        }
    }

    #[test]
    fn completed_turn_projection_keeps_only_transcript_and_compacts_indices() {
        let terminal = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "terminal".to_string(),
            protocol_iteration: 1,
            code: "finish { answer: 42 }".to_string(),
            prints: vec!["terminal output".to_string().into()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Finished(serde_json::json!({ "answer": 42 }).into()),
        };
        let retained = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "retained".to_string(),
            protocol_iteration: 0,
            code: "print \"next\"".to_string(),
            prints: vec!["next".to_string().into()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Completed,
        };
        let events = [
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u1",
                    MessageRole::User,
                    "first",
                )),
            ),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmAssistantContent(lash_rlm_types::RlmAssistantContent {
                    id: "terminal-content".to_string(),
                    reasoning: String::new(),
                    prose: "terminal prose".to_string(),
                }),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(terminal)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "a1",
                    MessageRole::Assistant,
                    "committed answer",
                )),
            ),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u2",
                    MessageRole::User,
                    "second",
                )),
            ),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(retained)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological).expect("valid history fixture");

        assert_eq!(projection.len(), 4);
        assert!(projection.suppresses_chronological(1));
        assert!(projection.suppresses_chronological(2));
        assert_eq!(projection.projected_index_for_chronological(3), Some(1));
        assert_eq!(projection.projected_index_for_chronological(5), Some(3));
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::Message { content, .. } if content == "committed answer"
        ));
        assert!(matches!(
            &projection.history()[3],
            RlmHistoryItem::LashVmStep { id, .. } if id == "retained"
        ));
    }

    #[test]
    fn completed_turn_projection_keeps_intermediate_steps_without_duplicate_prose() {
        let intermediate = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "intermediate".to_string(),
            protocol_iteration: 0,
            code: "missing_name".to_string(),
            prints: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Program,
                "unknown name",
            )),
        };
        let terminal = CellRecord {
            language: "typescript".to_string(),
            prints_retained: None,
            id: "terminal".to_string(),
            protocol_iteration: 1,
            code: "finish \"done\"".to_string(),
            prints: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            result: lash_core::CellOutcome::Finished(serde_json::json!("done").into()),
        };
        let events = [
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u1",
                    MessageRole::User,
                    "start",
                )),
            ),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmAssistantContent(lash_rlm_types::RlmAssistantContent {
                    id: "intermediate-content".to_string(),
                    reasoning: String::new(),
                    prose: "surviving prose".to_string(),
                }),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(intermediate)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(Box::new(terminal)),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            )),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "a1",
                    MessageRole::Assistant,
                    "surviving prose\n\ndone",
                )),
            ),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological).expect("valid history fixture");

        assert!(projection.suppresses_chronological(1));
        assert!(!projection.suppresses_chronological(2));
        assert!(projection.suppresses_chronological(3));
        assert_eq!(projection.len(), 3);
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::LashVmStep { id, outcome, .. }
                if id == "intermediate"
                    && outcome.error.as_ref().map(|error| error.message.as_str()) == Some("unknown name")
        ));
        assert!(matches!(
            &projection.history()[2],
            RlmHistoryItem::Message { content, .. }
                if content == "surviving prose\n\ndone"
        ));
    }

    #[test]
    fn prose_only_completion_suppresses_internal_assistant_record() {
        let mut internal = message("internal", MessageRole::Assistant, "natural completion");
        internal.origin = Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        });
        let events = [
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "u1",
                    MessageRole::User,
                    "answer naturally",
                )),
            ),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(internal),
            ),
            lash_core::SessionHistoryRecord::Conversation(
                lash_core::facade_support::ConversationRecord::from_message(message(
                    "a1",
                    MessageRole::Assistant,
                    "natural completion",
                )),
            ),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological).expect("valid history fixture");

        assert!(projection.suppresses_chronological(1));
        assert_eq!(projection.len(), 2);
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::Message { content, .. } if content == "natural completion"
        ));
    }
}
