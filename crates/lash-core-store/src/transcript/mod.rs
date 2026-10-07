//! The canonical rendering of retained committed nodes. Live observations are
//! provisional; only these rows describe committed history.

use crate::{
    InputId, Message, MessageOrigin, MessageRole, NodeId, PartKind, ProtocolEvent,
    SessionHistoryRecord, SessionNodePayload, SessionNodeRecord, TurnId,
};
use lash_sansio::core_support::PartCoreSupport;
use std::sync::Arc;

/// Snapshot-local order. It is deliberately neither serializable nor a cursor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowOrdinal(usize);
impl std::fmt::Debug for RowOrdinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RowOrdinal")
    }
}

/// Opaque row identity. Equality and wire transport do not expose its spelling.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct RowId(NodeId);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TranscriptRowKind {
    User,
    AssistantReply,
    Reasoning,
    ToolCall,
    CodeBlock,
    Attachment,
    Event,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SuppressionReason {
    NonTranscriptNodeKind,
    ProtocolInternal,
    SupersededByCommittedReply,
    NoCommittedReply,
    EmptyContent,
    UnrecognizedProtocolEvent,
}

#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RowProvenance {
    pub turn_id: Option<TurnId>,
    pub input_id: Option<InputId>,
    pub plugin_id: Option<String>,
    pub is_turn_reply: bool,
}

/// Display-only tool facts carried by committed nodes, without execution state.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RowTool {
    pub operation: String,
    pub status: String,
}

/// Protocol-neutral display content. Reasoning and attachments can accompany a
/// reply in its one node-backed row; they do not create extra identities.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RowContent {
    pub text: String,
    pub reasoning: Vec<String>,
    pub attachments: Vec<crate::AttachmentRef>,
    pub language: Option<String>,
    pub code: Option<String>,
    pub output: Option<String>,
    pub success: Option<bool>,
    pub error: Option<String>,
    pub tools: Vec<RowTool>,
    pub tools_omitted: usize,
}

/// Transportable display data. It has no snapshot ordinal or resume position.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct TranscriptRowRecord {
    pub row_id: RowId,
    pub kind: TranscriptRowKind,
    pub provenance: RowProvenance,
    pub content: RowContent,
    pub timestamp: crate::session_graph::NodeTimestamp,
    pub suppressed: Option<SuppressionReason>,
}

#[derive(Clone, Debug)]
pub struct TranscriptRow {
    ordinal: RowOrdinal,
    record: TranscriptRowRecord,
}
impl TranscriptRow {
    pub fn ordinal(&self) -> RowOrdinal {
        self.ordinal
    }
    pub fn row_id(&self) -> &RowId {
        &self.record.row_id
    }
    pub fn record(&self) -> &TranscriptRowRecord {
        &self.record
    }
    pub fn into_record(self) -> TranscriptRowRecord {
        self.record
    }
}

/// The answer of a protocol's pure row projection. Core owns identities,
/// timestamps and reply provenance; extensions only supply display content.
#[derive(Clone, Debug)]
pub enum TranscriptProjectionOutcome {
    Render {
        kind: TranscriptRowKind,
        content: Box<RowContent>,
    },
    Suppress(SuppressionReason),
}

/// Render-side protocol extension, usable without restoring a live session.
pub trait TranscriptRowProjectorPlugin: Send + Sync {
    fn project_message(&self, _message: &Message) -> Option<TranscriptProjectionOutcome> {
        None
    }
    fn project_event(
        &self,
        event: &ProtocolEvent,
    ) -> Result<Option<TranscriptProjectionOutcome>, crate::runtime_error::StoredDataCorruption>;
}

#[derive(Clone, Default)]
pub struct TranscriptProjectionOptions {
    projectors: Vec<Arc<dyn TranscriptRowProjectorPlugin>>,
}
impl std::fmt::Debug for TranscriptProjectionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptProjectionOptions")
            .field("projectors", &self.projectors.len())
            .finish()
    }
}
impl TranscriptProjectionOptions {
    pub fn with_projector(mut self, projector: Arc<dyn TranscriptRowProjectorPlugin>) -> Self {
        self.projectors.push(projector);
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct TranscriptProjection {
    rows: Vec<TranscriptRow>,
}
impl TranscriptProjection {
    pub fn from_read_state(
        view: &crate::SessionReadView,
        options: &TranscriptProjectionOptions,
    ) -> Result<Self, crate::runtime_error::StoredDataCorruption> {
        use crate::session_graph::facade_ops::SessionGraphFacadeOps;
        Self::from_records(view.session_graph().active_path_nodes(), options)
    }
    /// Fold retained committed records in source order, across frame boundaries.
    pub fn from_records<'a>(
        records: impl IntoIterator<Item = &'a SessionNodeRecord>,
        options: &TranscriptProjectionOptions,
    ) -> Result<Self, crate::runtime_error::StoredDataCorruption> {
        let mut current_turn = None;
        let rows = records
            .into_iter()
            .enumerate()
            .map(|(index, node)| {
                let mut provenance = RowProvenance::default();
                let disposition = match &node.payload {
                    SessionNodePayload::FrameOpen { .. } | SessionNodePayload::Plugin { .. } => {
                        TranscriptProjectionOutcome::Suppress(
                            SuppressionReason::NonTranscriptNodeKind,
                        )
                    }
                    SessionNodePayload::Event {
                        event: SessionHistoryRecord::Protocol(event),
                    } => {
                        provenance.turn_id = current_turn.clone();
                        provenance.plugin_id = Some(event.plugin_id.clone());
                        let mut projected = None;
                        for projector in &options.projectors {
                            if let Some(outcome) = projector.project_event(event)? {
                                projected = Some(outcome);
                                break;
                            }
                        }
                        projected.unwrap_or(TranscriptProjectionOutcome::Suppress(
                            SuppressionReason::UnrecognizedProtocolEvent,
                        ))
                    }
                    SessionNodePayload::Event {
                        event: SessionHistoryRecord::Conversation(record),
                    } => {
                        let message = record.to_message();
                        match message.origin.as_ref() {
                            Some(MessageOrigin::TurnInput { turn_id, input_id }) => {
                                provenance.turn_id = Some(turn_id.clone());
                                provenance.input_id = input_id.clone();
                            }
                            Some(MessageOrigin::TurnOutput { turn_id, source }) => {
                                provenance.turn_id = Some(turn_id.clone());
                                if let crate::TurnOutputSource::Plugin { plugin_id } = source {
                                    provenance.plugin_id = Some(plugin_id.clone());
                                }
                            }
                            Some(MessageOrigin::Plugin { plugin_id, .. }) => {
                                provenance.plugin_id = Some(plugin_id.clone())
                            }
                            _ => {}
                        }
                        if let Some(marker) = &message.reply_marker {
                            provenance.turn_id = Some(marker.turn_id().clone());
                            provenance.is_turn_reply = true;
                        }
                        if provenance.turn_id.is_some() {
                            current_turn = provenance.turn_id.clone();
                        }
                        if message.reply_marker.is_some() {
                            project_message(&message)
                        } else {
                            options
                                .projectors
                                .iter()
                                .find_map(|p| p.project_message(&message))
                                .unwrap_or_else(|| project_message(&message))
                        }
                    }
                };
                let disposition = match disposition {
                    TranscriptProjectionOutcome::Render {
                        kind: TranscriptRowKind::AssistantReply,
                        ..
                    } if !provenance.is_turn_reply => {
                        TranscriptProjectionOutcome::Suppress(SuppressionReason::NoCommittedReply)
                    }
                    disposition => disposition,
                };
                let (kind, content, suppressed) = match disposition {
                    TranscriptProjectionOutcome::Render { kind, content } => (kind, *content, None),
                    TranscriptProjectionOutcome::Suppress(reason) => (
                        TranscriptRowKind::Event,
                        RowContent::default(),
                        Some(reason),
                    ),
                };
                Ok(TranscriptRow {
                    ordinal: RowOrdinal(index),
                    record: TranscriptRowRecord {
                        row_id: RowId(node.node_id.clone()),
                        kind,
                        content,
                        provenance,
                        timestamp: node.timestamp,
                        suppressed,
                    },
                })
            })
            .collect::<Result<Vec<_>, crate::runtime_error::StoredDataCorruption>>()?;
        Ok(Self { rows })
    }
    pub fn rows(&self) -> &[TranscriptRow] {
        &self.rows
    }
    pub fn visible(&self) -> impl Iterator<Item = &TranscriptRowRecord> {
        self.rows
            .iter()
            .map(TranscriptRow::record)
            .filter(|r| r.suppressed.is_none())
    }
    pub fn into_records(self) -> Vec<TranscriptRowRecord> {
        self.rows
            .into_iter()
            .map(TranscriptRow::into_record)
            .collect()
    }
    pub fn reply(&self, turn_id: &TurnId) -> Option<&TranscriptRowRecord> {
        self.visible().find(|row| {
            row.provenance.is_turn_reply && row.provenance.turn_id.as_ref() == Some(turn_id)
        })
    }
}

fn project_message(message: &Message) -> TranscriptProjectionOutcome {
    if message.is_transient() || message.role == MessageRole::System {
        return TranscriptProjectionOutcome::Suppress(SuppressionReason::ProtocolInternal);
    }
    let mut content = RowContent::default();
    let mut text = Vec::new();
    for part in message.parts.iter() {
        let rendered = part.render();
        if part.kind() == PartKind::Reasoning {
            if !rendered.trim().is_empty() {
                content.reasoning.push(rendered);
            }
        } else if message
            .reply_marker
            .as_ref()
            .is_none_or(|marker| marker.part_id() == part.id())
        {
            text.push(rendered);
        }
        content.attachments.extend(
            part.attachment_sources()
                .filter_map(|source| source.stored_ref())
                .cloned(),
        );
    }
    content.text = text.join("\n");
    if content.reasoning.is_empty()
        && !message.parts.is_empty()
        && message
            .parts
            .iter()
            .all(|part| part.kind() == PartKind::Reasoning)
    {
        return TranscriptProjectionOutcome::Suppress(SuppressionReason::EmptyContent);
    }
    let kind = if message.reply_marker.is_some() {
        TranscriptRowKind::AssistantReply
    } else if message.role == MessageRole::User {
        TranscriptRowKind::User
    } else if message.role == MessageRole::Event {
        TranscriptRowKind::Event
    } else if message
        .parts
        .iter()
        .any(|part| matches!(part.kind(), PartKind::ToolCall | PartKind::ToolResult))
    {
        TranscriptRowKind::ToolCall
    } else if !content.reasoning.is_empty() {
        TranscriptRowKind::Reasoning
    } else {
        return TranscriptProjectionOutcome::Suppress(SuppressionReason::NoCommittedReply);
    };
    TranscriptProjectionOutcome::Render {
        kind,
        content: Box::new(content),
    }
}

// New variants must acquire a projection or an explicit suppression before
// the vocabulary can grow. These matches compile in the enum's owning crate.
const _: fn(TranscriptRowKind) = |kind| match kind {
    TranscriptRowKind::User
    | TranscriptRowKind::AssistantReply
    | TranscriptRowKind::Reasoning
    | TranscriptRowKind::ToolCall
    | TranscriptRowKind::CodeBlock
    | TranscriptRowKind::Attachment
    | TranscriptRowKind::Event => (),
};
const _: fn(SuppressionReason) = |reason| match reason {
    SuppressionReason::NonTranscriptNodeKind
    | SuppressionReason::ProtocolInternal
    | SuppressionReason::SupersededByCommittedReply
    | SuppressionReason::NoCommittedReply
    | SuppressionReason::EmptyContent
    | SuppressionReason::UnrecognizedProtocolEvent => (),
};
