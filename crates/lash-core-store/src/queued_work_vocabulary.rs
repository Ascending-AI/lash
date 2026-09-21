//! Durable queued-work vocabulary.
//!
//! The queue's durable rows, their claim and completion payloads and the
//! session-command family that rides in them. The runtime's queue driver
//! stays in `lash-core`; only the data it persists lives here.

use crate::{ProcessId, ProcessWakeDelivery, QueuedWorkClass, SessionId, TurnCause, TurnInput};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionCommand {
    /// Apply durable session-policy intent at the command drain. Consecutive
    /// config patches may share one head commit, but every enclosing queued
    /// batch remains present in the atomic completion.
    ApplyConfigPatch {
        patch: Box<super::ApplyConfigPatch>,
    },
    // No generation guard: the command drains asynchronously, so any
    // generation observed at enqueue time may legitimately have advanced by
    // drain time, and the refresh recomputes the surface from live sources
    // regardless — a guard could only fail spuriously.
    RefreshToolCatalog {
        reason: String,
    },
}
impl SessionCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ApplyConfigPatch { .. } => "apply_config_patch",
            Self::RefreshToolCatalog { .. } => "refresh_tool_catalog",
        }
    }

    pub fn source_key(&self, idempotency_key: impl AsRef<str>) -> String {
        format!("command:{}:{}", self.kind(), idempotency_key.as_ref())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCommandReceipt {
    pub session_id: SessionId,
    pub batch_id: crate::BatchId,
    pub source_key: String,
}
impl std::fmt::Display for SessionCommandReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "batch `{}` for session `{}`",
            self.batch_id, self.session_id
        )
    }
}
/// The semantically distinct outcomes of config-command submission. Rejection
/// happens before durable queue acceptance; `Durable` means the command's
/// completion and session head committed together. `Pending` preserves an
/// accepted receipt when the configured settlement deadline expires, while
/// `Cancelled` reports a queued command withdrawn before that commit.
#[derive(Clone, Debug)]
pub enum SessionCommandSettlement {
    Rejected(crate::RuntimeError),
    Durable(SessionCommandReceipt),
    Pending(SessionCommandReceipt),
    Cancelled(SessionCommandReceipt),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPolicy {
    EarliestSafeBoundary,
    AfterCurrentTurnCommit,
}
impl DeliveryPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EarliestSafeBoundary => "earliest_safe_boundary",
            Self::AfterCurrentTurnCommit => "after_current_turn_commit",
        }
    }

    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "earliest_safe_boundary" => Some(Self::EarliestSafeBoundary),
            "after_current_turn_commit" => Some(Self::AfterCurrentTurnCommit),
            _ => None,
        }
    }
}
/// Semantic kind of one queued-work row.
///
/// Control rows are always claimed alone even when a producer assigns a merge
/// key accidentally.
///
/// This is also the durable ingress-family discriminator. [`Self::Control`]
/// holds exactly when the row's payloads are session commands, because
/// [`QueuedWorkBatchDraft::new`] derives the kind from the payloads and no
/// setter exists to break the correspondence. Store ordering projections
/// therefore compare `work_kind` with [`Self::Control`]'s stable value for the
/// session-command family rather than hydrating payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkKind {
    Turn,
    Control,
}
impl QueuedWorkKind {
    /// Project the ingress family to its work class.
    pub fn work_class(self) -> QueuedWorkClass {
        match self {
            Self::Control => QueuedWorkClass::SessionCommand,
            Self::Turn => QueuedWorkClass::TurnWork,
        }
    }

    /// Reports whether rows of this kind may join an adjacent compatible turn claim.
    ///
    /// Only [`Self::Turn`] is batchable. Control rows remain single-row claims
    /// even when they carry the same merge key as neighboring work.
    pub fn is_batchable(self) -> bool {
        matches!(self, Self::Turn)
    }

    /// Store implementations must preserve these spellings so rows remain
    /// readable across runtime restarts and backend implementations.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Control => "control",
        }
    }

    /// The accepted values are exactly `"turn"` and `"control"`; an
    /// unrecognized value returns `None` so a store can reject incompatible
    /// durable data instead of assigning unsafe batching semantics.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "turn" => Some(Self::Turn),
            "control" => Some(Self::Control),
            _ => None,
        }
    }
}
/// Producer-stamped execution authority for queued work.
///
/// Both fields are opaque to Lash. Equality is the batching contract: rows
/// with different principals or different elevation overrides never share a
/// turn. Keeping this separate from `merge_key` prevents a grouping label from
/// becoming an authorization encoding.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedWorkAuthority {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation: Option<String>,
}
impl QueuedWorkAuthority {
    /// Stamps queued work with an opaque principal and no elevation override.
    ///
    /// Lash compares the complete authority value when forming a batch, so
    /// work created for a different principal cannot share the resulting turn.
    pub fn new(principal: impl Into<String>) -> Self {
        Self {
            principal: Some(principal.into()),
            elevation: None,
        }
    }

    /// Adds or replaces the opaque elevation override used by the work.
    ///
    /// Elevation participates in the same equality gate as the principal;
    /// rows with different overrides are never coalesced into one turn.
    pub fn with_elevation(mut self, elevation: impl Into<String>) -> Self {
        self.elevation = Some(elevation.into());
        self
    }
}
/// Complete claim-time bounds and drain policy passed to durable store
/// implementations.
#[derive(Clone, Debug)]
pub struct QueuedWorkClaimPolicy {
    pub max_context_tokens: usize,
    pub action_token_reserve: usize,
    pub max_rows: usize,
    pub max_pending_age_ms: u64,
    pub drain_policy: std::sync::Arc<dyn crate::QueuedDrainPolicy>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedWorkPayload {
    ProcessWake {
        wake: Box<ProcessWakeDelivery>,
    },
    AgentFrameTask {
        frame_id: crate::FrameNodeId,
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    },
    SessionCommand {
        command: Box<SessionCommand>,
    },
}
impl QueuedWorkPayload {
    pub fn process_wake(wake: ProcessWakeDelivery) -> Self {
        Self::ProcessWake {
            wake: Box::new(wake),
        }
    }

    pub fn session_command(command: SessionCommand) -> Self {
        Self::SessionCommand {
            command: Box::new(command),
        }
    }

    pub fn agent_frame_task(
        frame_id: crate::FrameNodeId,
        task: impl Into<String>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    ) -> Self {
        Self::AgentFrameTask {
            frame_id,
            task: task.into(),
            protocol_turn_options,
        }
    }

    pub fn work_class(&self) -> QueuedWorkClass {
        match self {
            Self::SessionCommand { .. } => QueuedWorkClass::SessionCommand,
            Self::ProcessWake { .. } | Self::AgentFrameTask { .. } => QueuedWorkClass::TurnWork,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkItem {
    pub item_id: String,
    pub payload: QueuedWorkPayload,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkBatch {
    pub batch_id: crate::BatchId,
    pub session_id: SessionId,
    pub enqueue_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub delivery_policy: DeliveryPolicy,
    pub kind: QueuedWorkKind,
    pub authority: QueuedWorkAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
    pub available_at_ms: u64,
    pub enqueued_at_ms: u64,
    pub items: Vec<QueuedWorkItem>,
}
impl QueuedWorkBatch {
    pub fn validate_payload_family(&self) -> Result<(), crate::StoreError> {
        validate_payload_family(self.kind, self.items.iter().map(|item| &item.payload)).map_err(
            |message| crate::StoreError::StoredDataCorrupt {
                record_kind: "QueuedWorkBatch",
                message,
            },
        )
    }

    pub fn work_class(&self) -> QueuedWorkClass {
        self.kind.work_class()
    }

    pub fn is_session_command_work(&self) -> bool {
        self.work_class() == QueuedWorkClass::SessionCommand
    }

    pub fn is_turn_work(&self) -> bool {
        self.work_class() == QueuedWorkClass::TurnWork
    }
}
/// Receiver-side result of an idempotent queued-work enqueue.
#[derive(Clone, Debug)]
pub enum QueuedWorkEnqueueOutcome {
    Inserted(QueuedWorkBatch),
    Existing(QueuedWorkBatch),
}
impl QueuedWorkEnqueueOutcome {
    pub fn batch(&self) -> &QueuedWorkBatch {
        match self {
            Self::Inserted(batch) | Self::Existing(batch) => batch,
        }
    }

    pub fn into_batch(self) -> QueuedWorkBatch {
        match self {
            Self::Inserted(batch) | Self::Existing(batch) => batch,
        }
    }

    pub fn process_wake_was_absorbed(&self) -> bool {
        matches!(self, Self::Existing(_))
    }
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(try_from = "QueuedWorkDraftWire")]
pub struct QueuedWorkBatchDraft {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// Structural producer identity for a process wake.
    ///
    /// Stores use this tuple for the receiver allocation-floor fence. It
    /// deliberately duplicates the human-readable source key so
    /// correctness never depends on parsing that string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_wake_source: Option<ProcessWakeSource>,
    pub delivery_policy: DeliveryPolicy,
    pub authority: QueuedWorkAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
    pub available_at_ms: u64,
    pub payloads: QueuedWorkBatchPayloads,
}
impl QueuedWorkBatchDraft {
    pub fn new(
        session_id: impl Into<SessionId>,
        delivery_policy: DeliveryPolicy,
        payloads: impl Into<QueuedWorkBatchPayloads>,
    ) -> Self {
        let payloads = payloads.into();
        Self {
            session_id: session_id.into(),
            source_key: None,
            process_wake_source: None,
            delivery_policy,
            authority: QueuedWorkAuthority::default(),
            merge_key: None,
            available_at_ms: 0,
            payloads,
        }
    }

    pub fn with_source_key(mut self, source_key: impl Into<String>) -> Self {
        self.source_key = Some(source_key.into());
        self
    }

    pub fn with_process_wake_source(
        mut self,
        process_id: impl Into<ProcessId>,
        sequence: u64,
    ) -> Self {
        self.process_wake_source = Some(ProcessWakeSource {
            process_id: process_id.into(),
            sequence,
        });
        self
    }

    pub fn with_available_at_ms(mut self, available_at_ms: u64) -> Self {
        self.available_at_ms = available_at_ms;
        self
    }

    /// There is deliberately no setter: the kind is a function of the payloads,
    /// so a producer cannot assert [`QueuedWorkKind::Control`] over turn work.
    pub fn kind(&self) -> QueuedWorkKind {
        self.payloads.kind()
    }

    pub fn with_authority(mut self, authority: QueuedWorkAuthority) -> Self {
        self.authority = authority;
        self
    }

    /// Assign a non-empty producer-selected merge key.
    ///
    /// # Panics
    ///
    /// Panics when the supplied key is empty.
    pub fn with_merge_key(mut self, merge_key: impl Into<String>) -> Self {
        let merge_key = merge_key.into();
        assert!(
            !merge_key.is_empty(),
            "queued-work merge key must be non-empty"
        );
        self.merge_key = Some(merge_key);
        self
    }

    pub fn work_class(&self) -> QueuedWorkClass {
        self.kind().work_class()
    }

    pub fn validate_process_wake_source(&self) -> Result<(), String> {
        let mut payloads = self.payloads.iter();
        match (
            self.process_wake_source.as_ref(),
            payloads.next(),
            payloads.next(),
        ) {
            (
                Some(source),
                Some(QueuedWorkPayload::ProcessWake { wake }),
                None,
            ) if wake.target_session_id == self.session_id
                && wake.process_id == source.process_id
                && wake.sequence == source.sequence
                && source.sequence <= i64::MAX as u64
                && self.source_key.as_deref()
                    == Some(process_wake_source_key(&source.process_id, source.sequence).as_str()) =>
            {
                Ok(())
            }
            (None, _, _)
                if !self.payloads
                    .iter()
                    .any(|payload| matches!(payload, QueuedWorkPayload::ProcessWake { .. })) =>
            {
                Ok(())
            }
            _ => Err(
                "process-wake queued work requires one matching payload, structural source tuple, signed-64-bit sequence, target session, and source key"
                    .to_string(),
            ),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessWakeSource {
    pub process_id: ProcessId,
    pub sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkClaimBoundary {
    ActiveTurnCheckpoint,
    Idle,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkCompletionData {
    pub batch_ids: Vec<crate::BatchId>,
}
/// A shared work completion carrying settled queued-work batch identities.
pub type QueuedWorkCompletion = crate::WorkCompletion<QueuedWorkCompletionData>;
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkClaimData {
    pub batches: Vec<QueuedWorkBatch>,
    /// Interrupted predecessor identity a store implementor must restore if
    /// this successor claim is abandoned. This is claim-control metadata, not
    /// durable protocol payload, so it is deliberately omitted from serde.
    #[serde(skip)]
    pub abandon_restore_claim_id: Option<String>,
    /// Interrupted predecessor token paired with `abandon_restore_claim_id`.
    #[serde(skip)]
    pub abandon_restore_claim_token: Option<Box<str>>,
}
/// A shared work claim carrying queued-work batches.
pub type QueuedWorkClaim = crate::WorkClaim<QueuedWorkClaimData>;
impl crate::WorkClaim<QueuedWorkClaimData> {
    pub fn completion(&self) -> QueuedWorkCompletion {
        QueuedWorkCompletion {
            session_id: self.session_id.clone(),
            claim_id: self.claim_id.clone(),
            lease_token: self.lease_token.clone(),
            data: QueuedWorkCompletionData {
                batch_ids: self
                    .batches
                    .iter()
                    .map(|batch| batch.batch_id.clone())
                    .collect(),
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.batches.iter().all(|batch| batch.items.is_empty())
    }

    /// Materializes checkpoint input from a claim for runtime and conformance-suite implementors.
    pub fn materialize_queued_checkpoint_work(&self) -> QueuedCheckpointWork {
        let mut turn_causes = Vec::new();
        for batch in &self.batches {
            for item in &batch.items {
                match &item.payload {
                    QueuedWorkPayload::ProcessWake { wake } => {
                        turn_causes.push(crate::process_wake_turn_cause(wake));
                    }
                    QueuedWorkPayload::AgentFrameTask { .. } => {}
                    QueuedWorkPayload::SessionCommand { .. } => {}
                }
            }
        }
        QueuedCheckpointWork { turn_causes }
    }

    /// Extracts the sole exclusive session command for queued-work driver implementors.
    pub fn exclusive_session_command(&self) -> Option<(&QueuedWorkBatch, &SessionCommand)> {
        if self.batches.len() != 1 {
            return None;
        }
        let batch = self.batches.first()?;
        if batch.kind != QueuedWorkKind::Control || batch.items.len() != 1 {
            return None;
        }
        let item = batch.items.first()?;
        match &item.payload {
            QueuedWorkPayload::SessionCommand { command } => Some((batch, command.as_ref())),
            _ => None,
        }
    }

    /// Extract every independently receipted command in a coalesced
    /// session-command claim. Every batch remains exactly one control item;
    /// only the enclosing commit is shared.
    pub fn session_commands(&self) -> Option<Vec<(&QueuedWorkBatch, &SessionCommand)>> {
        let mut commands = Vec::with_capacity(self.batches.len());
        for batch in &self.batches {
            if batch.kind != QueuedWorkKind::Control || batch.items.len() != 1 {
                return None;
            }
            let item = batch.items.first()?;
            let QueuedWorkPayload::SessionCommand { command } = &item.payload else {
                return None;
            };
            commands.push((batch, command.as_ref()));
        }
        (!commands.is_empty()).then_some(commands)
    }

    /// Materializes turn-producing input from a claim for queued-work driver implementors.
    pub fn materialize_queued_turn_work(&self) -> QueuedTurnWork {
        let checkpoint = self.materialize_queued_checkpoint_work();
        let mut input_items = Vec::new();
        let mut selected_turn_options = None;
        for batch in &self.batches {
            for item in &batch.items {
                if let QueuedWorkPayload::AgentFrameTask {
                    task,
                    protocol_turn_options: task_options,
                    ..
                } = &item.payload
                {
                    input_items.push(crate::InputItem::text(task.clone()));
                    // A producer choosing one merge key asserts that these
                    // events may share a turn. Preserve every task in order;
                    // the last event retains the former option precedence.
                    selected_turn_options = task_options.clone();
                }
            }
        }
        let mut input = TurnInput::items(input_items);
        input.protocol_turn_options = selected_turn_options;
        QueuedTurnWork {
            input,
            turn_causes: checkpoint.turn_causes,
        }
    }
}
impl From<SessionCommand> for SessionCommandPayload {
    fn from(command: SessionCommand) -> Self {
        Self(QueuedWorkPayload::session_command(command))
    }
}
impl From<SessionCommand> for QueuedWorkBatchPayloads {
    fn from(command: SessionCommand) -> Self {
        Self::SessionCommand(command.into())
    }
}
// Keep the established draft encoding while deriving kind from the typed body.
impl serde::Serialize for QueuedWorkBatchDraft {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct(
            "QueuedWorkBatchDraft",
            6 + usize::from(self.source_key.is_some())
                + usize::from(self.process_wake_source.is_some())
                + usize::from(self.merge_key.is_some()),
        )?;
        state.serialize_field("session_id", &self.session_id)?;
        if let Some(source) = &self.source_key {
            state.serialize_field("source_key", source)?;
        }
        if let Some(source) = &self.process_wake_source {
            state.serialize_field("process_wake_source", source)?;
        }
        state.serialize_field("delivery_policy", &self.delivery_policy)?;
        state.serialize_field("kind", &self.kind())?;
        state.serialize_field("authority", &self.authority)?;
        if let Some(key) = &self.merge_key {
            state.serialize_field("merge_key", key)?;
        }
        state.serialize_field("available_at_ms", &self.available_at_ms)?;
        state.serialize_field("payloads", &self.payloads)?;
        state.end()
    }
}
impl TryFrom<QueuedWorkDraftWire> for QueuedWorkBatchDraft {
    type Error = String;
    fn try_from(wire: QueuedWorkDraftWire) -> Result<Self, Self::Error> {
        if wire.kind != wire.payloads.kind() {
            return Err("queued-work kind contradicts its payload family".into());
        }
        Ok(Self {
            session_id: wire.session_id,
            source_key: wire.source_key,
            process_wake_source: wire.process_wake_source,
            delivery_policy: wire.delivery_policy,
            authority: wire.authority,
            merge_key: wire.merge_key,
            available_at_ms: wire.available_at_ms,
            payloads: wire.payloads,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueuedCheckpointWork {
    pub turn_causes: Vec<TurnCause>,
}
/// One command or a nonempty sequence of turn work, in the existing item order.
#[derive(Clone, Debug)]
pub enum QueuedWorkBatchPayloads {
    /// Exactly one session command.
    SessionCommand(SessionCommandPayload),
    /// A nonempty sequence whose item order is preserved.
    TurnWork {
        /// The required first item.
        first: TurnWorkPayload,
        /// Remaining items in delivery order.
        rest: Vec<TurnWorkPayload>,
    },
}
impl From<TurnWorkPayload> for QueuedWorkBatchPayloads {
    fn from(first: TurnWorkPayload) -> Self {
        Self::TurnWork {
            first,
            rest: Vec::new(),
        }
    }
}
impl QueuedWorkBatchPayloads {
    pub fn kind(&self) -> QueuedWorkKind {
        match self {
            Self::SessionCommand(_) => QueuedWorkKind::Control,
            Self::TurnWork { .. } => QueuedWorkKind::Turn,
        }
    }

    /// Visit the durable items in delivery order.
    pub fn iter(&self) -> impl Iterator<Item = &QueuedWorkPayload> {
        let (first, rest): (&QueuedWorkPayload, &[TurnWorkPayload]) = match self {
            Self::SessionCommand(command) => (&command.0, &[]),
            Self::TurnWork { first, rest } => (&first.0, rest),
        };
        std::iter::once(first).chain(rest.iter().map(|payload| &payload.0))
    }
}
impl IntoIterator for QueuedWorkBatchPayloads {
    type Item = QueuedWorkPayload;
    type IntoIter = std::vec::IntoIter<QueuedWorkPayload>;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::SessionCommand(command) => vec![command.0],
            Self::TurnWork { first, rest } => std::iter::once(first.0)
                .chain(rest.into_iter().map(|payload| payload.0))
                .collect(),
        }
        .into_iter()
    }
}
impl serde::Serialize for QueuedWorkBatchPayloads {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}
impl<'de> serde::Deserialize<'de> for QueuedWorkBatchPayloads {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let payloads = <Vec<QueuedWorkPayload> as serde::Deserialize>::deserialize(deserializer)?;
        let kind = match payloads.first() {
            Some(QueuedWorkPayload::SessionCommand { .. }) => QueuedWorkKind::Control,
            Some(_) => QueuedWorkKind::Turn,
            None => {
                return Err(serde::de::Error::custom(
                    "queued work requires at least one payload",
                ));
            }
        };
        validate_payload_family(kind, payloads.iter()).map_err(serde::de::Error::custom)?;
        let mut payloads = payloads.into_iter();
        let first = payloads
            .next()
            .ok_or_else(|| serde::de::Error::custom("queued work requires at least one payload"))?;
        Ok(match first {
            first @ QueuedWorkPayload::SessionCommand { .. } => {
                Self::SessionCommand(SessionCommandPayload(first))
            }
            first => Self::TurnWork {
                first: TurnWorkPayload(first),
                rest: payloads.map(TurnWorkPayload).collect(),
            },
        })
    }
}
#[derive(serde::Deserialize)]
struct QueuedWorkDraftWire {
    session_id: SessionId,
    source_key: Option<String>,
    process_wake_source: Option<ProcessWakeSource>,
    delivery_policy: DeliveryPolicy,
    kind: QueuedWorkKind,
    authority: QueuedWorkAuthority,
    merge_key: Option<String>,
    available_at_ms: u64,
    payloads: QueuedWorkBatchPayloads,
}

#[derive(Clone, Debug)]
pub struct QueuedTurnWork {
    pub input: TurnInput,
    pub turn_causes: Vec<TurnCause>,
}
pub fn process_wake_source_key(process_id: &ProcessId, sequence: u64) -> String {
    format!("process:{process_id}:event:{sequence}:wake")
}

/// Constant producer-selected merge key for process wakes.
///
/// The key says only that wake rows are eligible to share a turn. Work kind,
/// delivery boundary, authority, elevation, row count, age, and rendered size
/// remain independent claim gates.
pub const PROCESS_WAKE_MERGE_KEY: &str = "lash.process_wake";

pub fn process_wake_batch_draft(wake: ProcessWakeDelivery) -> QueuedWorkBatchDraft {
    process_wake_batch_draft_with_delivery_policy(wake, DeliveryPolicy::EarliestSafeBoundary)
}

/// Draft a process wake using the host-selected delivery boundary.
///
/// Delivery timing is independent of merge eligibility: it remains a selector
/// compatibility gate and is never encoded into the merge key.
pub fn process_wake_batch_draft_with_delivery_policy(
    wake: ProcessWakeDelivery,
    delivery_policy: DeliveryPolicy,
) -> QueuedWorkBatchDraft {
    let source_key = process_wake_source_key(&wake.process_id, wake.sequence);
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    let authority = wake.authority.clone();
    QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        delivery_policy,
        crate::TurnWorkPayload::process_wake(wake),
    )
    .with_source_key(source_key)
    .with_process_wake_source(process_id, sequence)
    .with_authority(authority)
    .with_merge_key(PROCESS_WAKE_MERGE_KEY)
}
/// A turn-work item; session commands cannot be constructed through this type.
#[derive(Clone, Debug)]
pub struct TurnWorkPayload(QueuedWorkPayload);
impl TurnWorkPayload {
    /// Wrap one durable process wake as turn work.
    pub fn process_wake(wake: ProcessWakeDelivery) -> Self {
        Self(QueuedWorkPayload::process_wake(wake))
    }

    pub fn agent_frame_task(
        frame_id: crate::FrameNodeId,
        task: impl Into<String>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    ) -> Self {
        Self(QueuedWorkPayload::agent_frame_task(
            frame_id,
            task,
            protocol_turn_options,
        ))
    }
}
/// Exactly one session command, validated by construction.
#[derive(Clone, Debug)]
pub struct SessionCommandPayload(QueuedWorkPayload);

fn validate_payload_family<'a>(
    kind: QueuedWorkKind,
    mut payloads: impl Iterator<Item = &'a QueuedWorkPayload>,
) -> Result<(), String> {
    let first = payloads
        .next()
        .ok_or_else(|| "queued work requires at least one payload".to_string())?;
    match kind {
        QueuedWorkKind::Control
            if matches!(first, QueuedWorkPayload::SessionCommand { .. })
                && payloads.next().is_none() =>
        {
            Ok(())
        }
        QueuedWorkKind::Turn
            if !matches!(first, QueuedWorkPayload::SessionCommand { .. })
                && payloads.all(|payload| {
                    !matches!(payload, QueuedWorkPayload::SessionCommand { .. })
                }) =>
        {
            Ok(())
        }
        _ => Err("queued-work kind contradicts its payload family".into()),
    }
}
