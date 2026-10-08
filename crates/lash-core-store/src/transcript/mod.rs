//! The typed, decoded history of retained committed nodes (ADR 0129). Live
//! observations are provisional; only these entries describe committed
//! history. Lash decodes facts here: roles, content blocks, tool calls and
//! results, code cells and their outcomes, the sealed reply and omission
//! counts. It renders nothing: hosts own every presentation decision.

use crate::{
    AttachmentRef, InputId, Message, MessageOrigin, MessageRole, NodeId, Part, ProtocolEvent,
    SessionHistoryRecord, SessionNodePayload, SessionNodeRecord, TurnId,
};
use lash_sansio::tool_output::ModelToolReturnPart;
use lash_sansio::{CellFailure, ExecutedCall, OutputValue, RetainedOutput, ToolCallId};
use std::sync::Arc;

/// Opaque entry identity. Equality and transport do not expose its spelling.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EntryId(NodeId);

/// Why a committed node yields no entry content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntryProvenance {
    pub turn_id: Option<TurnId>,
    pub input_id: Option<InputId>,
    pub plugin_id: Option<String>,
    /// The entry is its turn's sealed reply: the runtime minted its marker.
    pub is_turn_reply: bool,
}

/// Who a committed message speaks for. A provider replays tool results under
/// the user role; the decoder classifies them as [`TranscriptRole::Tool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TranscriptRole {
    User,
    Assistant,
    Tool,
    Event,
}

/// One content block of a committed message, in part order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TranscriptBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    /// An attachment part: its stored reference, when it has one, and the
    /// text the part carries in its place.
    Attachment {
        attachment: Option<AttachmentRef>,
        text: String,
    },
    ToolCall {
        call_id: ToolCallId,
        tool_name: String,
        arguments: String,
    },
    ToolResult {
        call_id: ToolCallId,
        tool_name: String,
        content: Vec<ToolResultBlock>,
    },
    Code {
        code: String,
    },
    CodeOutput {
        text: String,
    },
    CodeError {
        text: String,
    },
}

/// One block of a tool result, in order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolResultBlock {
    Text { text: String },
    Attachment { attachment: Option<AttachmentRef> },
    Retained { output: RetainedOutput },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub blocks: Vec<TranscriptBlock>,
}

/// One code cell a protocol executed, with its typed outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptCell {
    pub language: String,
    pub code: String,
    /// Inline prints, in order; empty when `prints_retained` holds them.
    pub prints: Vec<CellPrint>,
    /// One archive of every print, when they exceeded the inline limit.
    pub prints_retained: Option<RetainedOutput>,
    pub result: CellResult,
    /// The dispatches the cell executed. An entry's `call_id` is the id of
    /// the host tool call's own record; `None` for a dispatch lash handled
    /// with no host tool call.
    pub calls: Vec<ExecutedCall>,
    /// Calls the cell made beyond the recorded `calls`.
    pub calls_omitted: usize,
    pub images: Vec<AttachmentRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CellPrint {
    pub text: String,
    pub value: serde_json::Value,
}

/// What a cell resolved to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CellResult {
    /// The cell ran to its end without a terminal value.
    Completed,
    Failed(CellFailure),
    /// The cell finished its turn with this value.
    Finished(TerminalValue),
}

/// A cell's terminal value: inline, or retained with its witness.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TerminalValue {
    Inline(serde_json::Value),
    Retained(RetainedOutput),
}

impl From<OutputValue> for TerminalValue {
    fn from(value: OutputValue) -> Self {
        match value {
            OutputValue::Inline(value) => Self::Inline(value),
            OutputValue::Retained(value) => Self::Retained(value),
        }
    }
}

/// What a committed node decodes to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum TranscriptItem {
    Message(TranscriptMessage),
    Cell(Box<TranscriptCell>),
    Suppressed(SuppressionReason),
}

/// One retained committed node, decoded. Transportable; its order is its
/// position in the transcript, which is not a cursor.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptEntry {
    pub entry_id: EntryId,
    pub provenance: EntryProvenance,
    pub timestamp: crate::session_graph::NodeTimestamp,
    pub item: TranscriptItem,
}

impl TranscriptEntry {
    pub fn is_suppressed(&self) -> bool {
        matches!(self.item, TranscriptItem::Suppressed(_))
    }
}

/// A protocol's decoder of the stored shapes it owns, usable without
/// restoring a live session. Core owns identities, timestamps and reply
/// provenance; a decoder supplies only the decoded item.
pub trait TranscriptDecoderPlugin: Send + Sync {
    fn decode_message(&self, _message: &Message) -> Option<TranscriptItem> {
        None
    }
    fn decode_event(
        &self,
        event: &ProtocolEvent,
    ) -> Result<Option<TranscriptItem>, crate::runtime_error::StoredDataCorruption>;
}

#[derive(Clone, Default)]
pub struct TranscriptDecoders {
    decoders: Vec<Arc<dyn TranscriptDecoderPlugin>>,
}
impl std::fmt::Debug for TranscriptDecoders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptDecoders")
            .field("decoders", &self.decoders.len())
            .finish()
    }
}
impl TranscriptDecoders {
    pub fn with_decoder(mut self, decoder: Arc<dyn TranscriptDecoderPlugin>) -> Self {
        self.decoders.push(decoder);
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionTranscript {
    entries: Vec<TranscriptEntry>,
}
impl SessionTranscript {
    pub fn from_read_state(
        view: &crate::SessionReadView,
        decoders: &TranscriptDecoders,
    ) -> Result<Self, crate::runtime_error::StoredDataCorruption> {
        use crate::session_graph::facade_ops::SessionGraphFacadeOps;
        Self::from_records(view.session_graph().active_path_nodes(), decoders)
    }
    /// Decode retained committed records in source order, across frame
    /// boundaries.
    pub fn from_records<'a>(
        records: impl IntoIterator<Item = &'a SessionNodeRecord>,
        decoders: &TranscriptDecoders,
    ) -> Result<Self, crate::runtime_error::StoredDataCorruption> {
        Self::fold(records, decoders, None)
    }

    /// Fold `records` with `current_turn` as the turn a protocol event
    /// belongs to until a message names one.
    fn fold<'a>(
        records: impl IntoIterator<Item = &'a SessionNodeRecord>,
        decoders: &TranscriptDecoders,
        mut current_turn: Option<TurnId>,
    ) -> Result<Self, crate::runtime_error::StoredDataCorruption> {
        let entries = records
            .into_iter()
            .map(|node| {
                let mut provenance = EntryProvenance::default();
                let item = match &node.payload {
                    SessionNodePayload::FrameOpen { .. } | SessionNodePayload::Plugin { .. } => {
                        TranscriptItem::Suppressed(SuppressionReason::NonTranscriptNodeKind)
                    }
                    SessionNodePayload::Event {
                        event: SessionHistoryRecord::Protocol(event),
                    } => {
                        provenance.turn_id = current_turn.clone();
                        provenance.plugin_id = Some(event.plugin_id.clone());
                        let mut decoded = None;
                        for decoder in &decoders.decoders {
                            if let Some(item) = decoder.decode_event(event)? {
                                decoded = Some(item);
                                break;
                            }
                        }
                        decoded.unwrap_or(TranscriptItem::Suppressed(
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
                            decode_message(&message)
                        } else {
                            decoders
                                .decoders
                                .iter()
                                .find_map(|decoder| decoder.decode_message(&message))
                                .unwrap_or_else(|| decode_message(&message))
                        }
                    }
                };
                Ok(TranscriptEntry {
                    entry_id: EntryId(node.node_id.clone()),
                    provenance,
                    timestamp: node.timestamp,
                    item,
                })
            })
            .collect::<Result<Vec<_>, crate::runtime_error::StoredDataCorruption>>()?;
        Ok(Self { entries })
    }
    /// Every entry, suppressed ones included, in source order.
    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }
    pub fn visible(&self) -> impl Iterator<Item = &TranscriptEntry> {
        self.entries.iter().filter(|entry| !entry.is_suppressed())
    }
    pub fn into_entries(self) -> Vec<TranscriptEntry> {
        self.entries
    }
    pub fn reply(&self, turn_id: &TurnId) -> Option<&TranscriptEntry> {
        self.visible().find(|entry| {
            entry.provenance.is_turn_reply && entry.provenance.turn_id.as_ref() == Some(turn_id)
        })
    }
}

/// One committed turn of a session: its entries decode exactly the nodes its
/// commit appended, every input it admitted among them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommittedTurn {
    pub turn_id: TurnId,
    pub entries: Vec<TranscriptEntry>,
    pub committed_at_ms: u64,
    pub outcome: crate::store::TurnCommitOutcome,
}

/// One page of a session's committed turns, oldest first by commit, read
/// after a [`CommittedTurnCursor`](crate::store::CommittedTurnCursor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedTurnsPage {
    pub turns: Vec<CommittedTurn>,
    /// Persist it only after applying every turn of the page; the next read
    /// continues from it. A page shorter than its limit has read to the head.
    pub next: crate::store::CommittedTurnCursor,
}

impl CommittedTurnsPage {
    /// Decode each turn's appended nodes. A protocol event belongs to its
    /// turn even before the turn's first message.
    pub fn decode(
        page: crate::store::CommittedTurnNodesPage,
        decoders: &TranscriptDecoders,
    ) -> Result<Self, crate::runtime_error::StoredDataCorruption> {
        let turns = page
            .turns
            .into_iter()
            .map(|turn| {
                let entries = SessionTranscript::fold(
                    turn.nodes.iter(),
                    decoders,
                    Some(turn.turn_id.clone()),
                )?
                .into_entries();
                Ok(CommittedTurn {
                    turn_id: turn.turn_id,
                    entries,
                    committed_at_ms: turn.committed_at_ms,
                    outcome: turn.outcome,
                })
            })
            .collect::<Result<_, crate::runtime_error::StoredDataCorruption>>()?;
        Ok(Self {
            turns,
            next: page.next,
        })
    }
}

/// Decode a committed message. A reply keeps only its marked part among its
/// prose, with its reasoning and attachments.
fn decode_message(message: &Message) -> TranscriptItem {
    if message.is_transient() || message.role == MessageRole::System {
        return TranscriptItem::Suppressed(SuppressionReason::ProtocolInternal);
    }
    let marked = message.reply_marker.as_ref().map(|marker| marker.part_id());
    let blocks = message
        .parts
        .iter()
        .filter_map(|part| decode_part(part, marked))
        .collect::<Vec<_>>();
    let has = |test: fn(&TranscriptBlock) -> bool| blocks.iter().any(test);
    if blocks.is_empty() && !message.parts.is_empty() {
        return TranscriptItem::Suppressed(SuppressionReason::EmptyContent);
    }
    let role = if message.reply_marker.is_some() {
        TranscriptRole::Assistant
    } else if has(|block| matches!(block, TranscriptBlock::ToolResult { .. })) {
        TranscriptRole::Tool
    } else if has(|block| matches!(block, TranscriptBlock::ToolCall { .. })) {
        TranscriptRole::Assistant
    } else if message.role == MessageRole::User {
        TranscriptRole::User
    } else if message.role == MessageRole::Event {
        TranscriptRole::Event
    } else if blocks
        .iter()
        .all(|block| matches!(block, TranscriptBlock::Reasoning { .. }))
        && !blocks.is_empty()
    {
        TranscriptRole::Assistant
    } else {
        // Assistant prose that is not its turn's reply: the reply supersedes it.
        return TranscriptItem::Suppressed(SuppressionReason::NoCommittedReply);
    };
    TranscriptItem::Message(TranscriptMessage { role, blocks })
}

/// The part kinds, folded exhaustively inside their owning crate.
enum PartClass {
    Text,
    Attachment,
    Code,
    Output,
    Error,
    ToolCall,
    ToolResult,
    Reasoning,
}

fn decode_part(part: &Part, marked: Option<&str>) -> Option<TranscriptBlock> {
    let class = lash_sansio::core_support::fold_part_kind(
        part.kind(),
        [
            PartClass::Text,
            PartClass::Attachment,
            PartClass::Code,
            PartClass::Output,
            PartClass::Error,
            PartClass::Text,
            PartClass::ToolCall,
            PartClass::ToolResult,
            PartClass::Reasoning,
        ],
    );
    let text = || part.text_content().unwrap_or_default().to_string();
    let tool_name = || part.tool_name().unwrap_or_default().to_string();
    Some(match class {
        PartClass::Reasoning => {
            if text().trim().is_empty() {
                return None;
            }
            TranscriptBlock::Reasoning { text: text() }
        }
        PartClass::Text => {
            if marked.is_some_and(|marked| marked != part.id()) {
                return None;
            }
            TranscriptBlock::Text { text: text() }
        }
        PartClass::Attachment => TranscriptBlock::Attachment {
            attachment: part
                .attachment()
                .map(|attachment| attachment.reference.clone()),
            text: text(),
        },
        PartClass::ToolCall => TranscriptBlock::ToolCall {
            call_id: part.call_id()?.clone(),
            tool_name: tool_name(),
            arguments: text(),
        },
        PartClass::ToolResult => TranscriptBlock::ToolResult {
            call_id: part.call_id()?.clone(),
            tool_name: tool_name(),
            content: part
                .tool_result_content()
                .unwrap_or_default()
                .iter()
                .map(|block| match block {
                    ModelToolReturnPart::Text { text } => {
                        ToolResultBlock::Text { text: text.clone() }
                    }
                    ModelToolReturnPart::Attachment(reference) => ToolResultBlock::Attachment {
                        attachment: Some(reference.clone()),
                    },
                    ModelToolReturnPart::Retained(output) => ToolResultBlock::Retained {
                        output: output.clone(),
                    },
                })
                .collect(),
        },
        PartClass::Code => TranscriptBlock::Code { code: text() },
        PartClass::Output => TranscriptBlock::CodeOutput { text: text() },
        PartClass::Error => TranscriptBlock::CodeError { text: text() },
    })
}

// New variants must acquire a decoding or an explicit suppression before the
// vocabulary can grow. These matches compile in the enums' owning crate.
const _: fn(SuppressionReason) = |reason| match reason {
    SuppressionReason::NonTranscriptNodeKind
    | SuppressionReason::ProtocolInternal
    | SuppressionReason::SupersededByCommittedReply
    | SuppressionReason::NoCommittedReply
    | SuppressionReason::EmptyContent
    | SuppressionReason::UnrecognizedProtocolEvent => (),
};
const _: fn(TranscriptRole) = |role| match role {
    TranscriptRole::User
    | TranscriptRole::Assistant
    | TranscriptRole::Tool
    | TranscriptRole::Event => (),
};
