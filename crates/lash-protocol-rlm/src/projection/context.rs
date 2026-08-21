use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use lash_core::{
    AttachmentSource, Message, MessageRole, PartKind, RuntimeExecutionContext,
    facade_support::ChronologicalPayload,
};
use lash_rlm_types::{
    RlmAttachmentRef, RlmHistoryItem, RlmHistoryRole, RlmImageRef, RlmProtocolEvent,
    RlmTrajectoryEntry,
};
use lashlang::{
    ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, State as FlowState, Value as FlowValue,
};

use super::bindings::{
    ProjectionResolver, RLM_TURN_INPUT_PLUGIN_ID, RlmProjectedBindings, RlmProjectionExtension,
};
use super::transport::json_to_flow_value;

pub fn rlm_protocol_event(event: RlmProtocolEvent) -> lash_core::ProtocolEvent {
    lash_core::ProtocolEvent::typed(crate::plugin::RLM_PROTOCOL_PLUGIN_ID, event)
        .expect("RLM protocol events serialize")
}

pub fn decode_rlm_protocol_event(event: &lash_core::ProtocolEvent) -> Option<RlmProtocolEvent> {
    event
        .decode(crate::plugin::RLM_PROTOCOL_PLUGIN_ID)
        .ok()
        .flatten()
}

#[derive(Clone, Debug)]
pub struct RlmHistoryProjection {
    history: Vec<RlmHistoryItem>,
    chronological_indices: HashMap<usize, usize>,
    suppressed_chronological_indices: HashSet<usize>,
}

impl RlmHistoryProjection {
    pub fn from_chronological(
        projection: &lash_core::facade_support::ChronologicalProjection,
    ) -> Self {
        let suppressed_chronological_indices =
            completed_turn_internal_indices(projection.entries());
        let mut history = Vec::with_capacity(projection.entries().len());
        let mut chronological_indices = HashMap::with_capacity(projection.entries().len());
        for entry in projection.entries() {
            if suppressed_chronological_indices.contains(&entry.index) {
                continue;
            }
            let item = match &entry.payload {
                ChronologicalPayload::Message(message) => history_item_from_message(message),
                ChronologicalPayload::ProtocolEvent(event) => {
                    match decode_rlm_protocol_event(event) {
                        Some(RlmProtocolEvent::RlmAssistantContent(content)) => {
                            Some(RlmHistoryItem::Message {
                                id: content.id,
                                role: RlmHistoryRole::Assistant,
                                content: content.prose,
                                attachments: Vec::new(),
                            })
                        }
                        Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                            Some(history_item_from_lashlang_step(&step))
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
        Self {
            history,
            chronological_indices,
            suppressed_chronological_indices,
        }
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

/// Find protocol prose and terminal mechanics superseded by a committed
/// assistant transcript from the same turn. The relationship is derived from
/// event provenance: protocol entries and a later assistant message before the
/// next user/event turn boundary share one completed turn. Content is never
/// compared. Intermediate trajectory entries remain available, while their
/// prose is represented by the canonical transcript. A terminal step with no
/// committed message remains unchanged.
fn completed_turn_internal_indices(
    entries: &[lash_core::facade_support::ChronologicalEntry],
) -> HashSet<usize> {
    let mut suppressed = HashSet::new();
    let mut assistant_content_indices = Vec::new();
    let mut terminal_step = None;

    for entry in entries {
        match &entry.payload {
            ChronologicalPayload::Message(message) => match message.role {
                MessageRole::User | MessageRole::Event => {
                    assistant_content_indices.clear();
                    terminal_step = None;
                }
                MessageRole::Assistant => {
                    if is_internal_rlm_assistant(message) {
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
            ChronologicalPayload::ProtocolEvent(event) => match decode_rlm_protocol_event(event) {
                Some(RlmProtocolEvent::RlmAssistantContent(_)) => {
                    assistant_content_indices.push(entry.index);
                }
                Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                    terminal_step = step.final_output.is_some().then_some(entry.index);
                }
                _ => {}
            },
        }
    }

    suppressed
}

fn is_internal_rlm_assistant(message: &Message) -> bool {
    matches!(
        message.origin.as_ref(),
        Some(lash_core::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        }) if plugin_id == crate::plugin::RLM_PROTOCOL_PLUGIN_ID
    )
}

pub fn rlm_history_projection(
    projection: &lash_core::facade_support::ChronologicalProjection,
) -> RlmHistoryProjection {
    RlmHistoryProjection::from_chronological(projection)
}

pub(crate) async fn projected_bindings(
    ctx: &RuntimeExecutionContext<'_>,
    session_bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
) -> Result<ProjectedBindings, String> {
    let mut bindings = ProjectedBindings::new();
    bindings
        .try_insert(
            "history",
            ProjectedValue::custom(
                "history",
                Arc::new(HistoryProjectedValue {
                    projection: Arc::new(rlm_history_projection(
                        ctx.chronological_projection().as_ref(),
                    )),
                }),
            ),
        )
        .map_err(|err| format!("`{}` is reserved as an RLM built-in binding", err.name()))?;
    insert_projected_bindings(
        &mut bindings,
        session_bindings,
        Arc::clone(&projection_resolver),
    )
    .await?;
    if let Some(extension) = ctx
        .turn_context()
        .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
    {
        insert_projected_bindings(
            &mut bindings,
            extension.bindings.clone(),
            projection_resolver,
        )
        .await?;
    }
    Ok(bindings)
}

async fn insert_projected_bindings(
    target: &mut ProjectedBindings,
    bindings: RlmProjectedBindings,
    projection_resolver: Arc<dyn ProjectionResolver>,
) -> Result<(), String> {
    let host_bindings = bindings
        .into_projected_bindings(projection_resolver)
        .await
        .map_err(|err| err.to_string())?;
    for name in host_bindings.names().collect::<Vec<_>>() {
        let value = host_bindings
            .get(&name)
            .expect("name came from projected bindings");
        target.try_insert(name, value).map_err(|err| {
            format!(
                "`{}` is already bound as an RLM projected binding",
                err.name()
            )
        })?;
    }
    Ok(())
}

struct HistoryProjectedValue {
    projection: Arc<RlmHistoryProjection>,
}

impl ProjectedHostDescriptor for HistoryProjectedValue {
    fn type_name(&self) -> &str {
        "list"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, ProjectedReadResponse> {
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Len => ProjectedReadResponse::Len(self.projection.len()),
                ProjectedReadRequest::Index(index) => {
                    let Ok(Some(index)) = projected_index(&index, self.projection.len()) else {
                        return ProjectedReadResponse::Missing;
                    };
                    self.projection
                        .item(index)
                        .and_then(|item| serde_json::to_value(item).ok())
                        .map(json_to_flow_value)
                        .map(ProjectedReadResponse::Value)
                        .unwrap_or(ProjectedReadResponse::Missing)
                }
                ProjectedReadRequest::Render => ProjectedReadResponse::Text(
                    serde_json::to_string(self.projection.history())
                        .unwrap_or_else(|_| "[]".to_string()),
                ),
                ProjectedReadRequest::Materialize => {
                    ProjectedReadResponse::Value(json_to_flow_value(self.projection.value()))
                }
                _ => ProjectedReadResponse::Missing,
            }
        })
    }
}

pub(crate) fn projected_index(index: &FlowValue, len: usize) -> Result<Option<usize>, ()> {
    let FlowValue::Number(index) = index else {
        return Err(());
    };
    if !index.is_finite() || index.fract() != 0.0 {
        return Err(());
    }
    let len = len as isize;
    let index = *index as isize;
    let normalized = if index < 0 { len + index } else { index };
    if normalized < 0 || normalized >= len {
        return Ok(None);
    }
    Ok(Some(normalized as usize))
}

pub(crate) fn prune_reserved_projected_bindings(rlm: &mut FlowState) {
    prune_protected_bindings(rlm, &BTreeSet::new());
}

pub(crate) fn prune_protected_bindings(rlm: &mut FlowState, protected_names: &BTreeSet<String>) {
    prune_projected_binding_names(
        rlm,
        std::iter::once("history").chain(protected_names.iter().map(String::as_str)),
    );
}

/// Removes the named bindings in one transaction.
///
/// Pruning is one heap copy and one collection for the whole set rather than
/// one of each per name.
pub(crate) fn prune_projected_binding_names<'a>(
    rlm: &mut FlowState,
    names: impl IntoIterator<Item = &'a str>,
) {
    rlm.patch_globals(names.into_iter().map(|name| lashlang::GlobalPatch::Remove {
        name: name.to_string(),
    }))
    .expect("removing bindings cannot exceed the heap bound");
}

fn history_item_from_message(message: &Message) -> Option<RlmHistoryItem> {
    let content = message_history_text(message);
    let attachments = message
        .parts
        .iter()
        .filter_map(|part| {
            let attachment = part.attachment.as_ref()?;
            let (media_type, label, source, reference) = attachment_summary(&attachment.source);
            Some(RlmAttachmentRef {
                id: part.id.clone(),
                media_type,
                label,
                source,
                reference,
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

fn history_item_from_lashlang_step(entry: &RlmTrajectoryEntry) -> RlmHistoryItem {
    RlmHistoryItem::from_trajectory_entry(entry, entry.images.iter().map(image_ref).collect())
}

fn message_history_text(message: &Message) -> String {
    let chunks = message
        .parts
        .iter()
        .filter(|part| matches!(part.kind, PartKind::Text | PartKind::Prose))
        .map(|part| part.content.trim())
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

fn image_ref(image: &lash_core::AttachmentRef) -> RlmImageRef {
    let (width, height) = match image.type_metadata.as_ref() {
        Some(lash_core::AttachmentTypeMetadata::Image { width, height }) => (*width, *height),
        None => (None, None),
    };
    RlmImageRef {
        id: image.id.to_string(),
        media_type: image.media_type.clone(),
        width,
        height,
        bytes: image.byte_len as usize,
        label: image.label.clone(),
    }
}

fn attachment_summary(
    source: &AttachmentSource,
) -> (Option<lash_core::MediaType>, Option<String>, String, String) {
    match source {
        AttachmentSource::Inline { media_type, .. } => (
            Some(media_type.clone()),
            None,
            "inline".to_string(),
            "transient".to_string(),
        ),
        AttachmentSource::Stored { attachment_ref } => (
            Some(attachment_ref.media_type.clone()),
            attachment_ref.label.clone(),
            "stored".to_string(),
            attachment_ref.id.to_string(),
        ),
        AttachmentSource::ExternalUrl { media_type, url } => (
            Some(media_type.clone()),
            None,
            "external_url".to_string(),
            url.clone(),
        ),
        AttachmentSource::ProviderFile { id, .. } => {
            (None, None, "provider_file".to_string(), id.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    fn step_projection(output: &str) -> lash_core::facade_support::ChronologicalProjection {
        let entry = RlmTrajectoryEntry {
            id: "lashlang_step_0".to_string(),
            protocol_iteration: 0,
            code: "print big".to_string(),
            output: vec![output.to_string()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: None,
            final_output: None,
        };
        let events = [lash_core::SessionHistoryRecord::Protocol(
            rlm_protocol_event(RlmProtocolEvent::RlmTrajectoryEntry(entry)),
        )];
        lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        )
    }

    async fn read_index(value: &HistoryProjectedValue, index: i64) -> FlowValue {
        match value
            .read_one(ProjectedReadRequest::Index(FlowValue::Number(index as f64)))
            .await
        {
            ProjectedReadResponse::Value(value) => value,
            other => panic!("expected indexed value, got {other:?}"),
        }
    }

    // The history renderer advertises a `full: history[N].output[M]` reference
    // for truncated outputs (see `truncated_ref` in driver/history.rs). This
    // proves the contract resolves: indexing the real `history` projection
    // hands back the FULL, untruncated value the prompt only previewed.
    #[tokio::test]
    async fn history_step_output_resolves_full_untruncated_value() {
        let full = "X".repeat(50_000);
        let projection = step_projection(&full);
        let value = HistoryProjectedValue {
            projection: Arc::new(rlm_history_projection(&projection)),
        };

        // history[0] -> the serialized lashlang execution record.
        let FlowValue::Record(step) = read_index(&value, 0).await else {
            panic!("history[0] should be a record");
        };
        // history[0].output -> the list of per-print outputs.
        let Some(FlowValue::List(outputs)) = step.get("output") else {
            panic!("step record should carry an `output` list, got {step:?}");
        };
        // history[0].output[0] -> the full untruncated string.
        let Some(FlowValue::String(text)) = outputs.first() else {
            panic!("output[0] should be a string");
        };
        assert_eq!(
            text.as_str(),
            full.as_str(),
            "re-fetched value must be the full untruncated output"
        );
    }

    #[test]
    fn completed_turn_projection_keeps_only_transcript_and_compacts_indices() {
        let terminal = RlmTrajectoryEntry {
            id: "terminal".to_string(),
            protocol_iteration: 1,
            code: "finish { answer: 42 }".to_string(),
            output: vec!["terminal output".to_string()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: None,
            final_output: Some(serde_json::json!({ "answer": 42 })),
        };
        let retained = RlmTrajectoryEntry {
            id: "retained".to_string(),
            protocol_iteration: 0,
            code: "print \"next\"".to_string(),
            output: vec!["next".to_string()],
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: None,
            final_output: None,
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
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(terminal),
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
                RlmProtocolEvent::RlmTrajectoryEntry(retained),
            )),
        ];
        let chronological = lash_core::facade_support::ChronologicalProjection::from_turn_view(
            &events,
            &lash_core::facade_support::MessageSequence::default(),
        );
        let projection = rlm_history_projection(&chronological);

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
            RlmHistoryItem::LashlangStep { id, .. } if id == "retained"
        ));
    }

    #[test]
    fn completed_turn_projection_keeps_intermediate_steps_without_duplicate_prose() {
        let intermediate = RlmTrajectoryEntry {
            id: "intermediate".to_string(),
            protocol_iteration: 0,
            code: "missing_name".to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: Some("unknown name".to_string()),
            final_output: None,
        };
        let terminal = RlmTrajectoryEntry {
            id: "terminal".to_string(),
            protocol_iteration: 1,
            code: "finish \"done\"".to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            error: None,
            final_output: Some(serde_json::json!("done")),
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
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(intermediate),
            )),
            lash_core::SessionHistoryRecord::Protocol(rlm_protocol_event(
                RlmProtocolEvent::RlmTrajectoryEntry(terminal),
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
        let projection = rlm_history_projection(&chronological);

        assert!(projection.suppresses_chronological(1));
        assert!(!projection.suppresses_chronological(2));
        assert!(projection.suppresses_chronological(3));
        assert_eq!(projection.len(), 3);
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::LashlangStep { id, error, .. }
                if id == "intermediate" && error.as_deref() == Some("unknown name")
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
        let projection = rlm_history_projection(&chronological);

        assert!(projection.suppresses_chronological(1));
        assert_eq!(projection.len(), 2);
        assert!(matches!(
            &projection.history()[1],
            RlmHistoryItem::Message { content, .. } if content == "natural completion"
        ));
    }
}
