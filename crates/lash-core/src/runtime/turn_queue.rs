use super::process::ProcessWakeDelivery;
use crate::store::QueuedWorkClass;
use crate::{PluginMessage, TurnCause, TurnInput};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionCommand {
    // No generation guard: the command drains asynchronously, so any
    // generation observed at enqueue time may legitimately have advanced by
    // drain time, and the refresh recomputes the surface from live sources
    // regardless — a guard could only fail spuriously.
    RefreshToolCatalog { reason: String },
}

impl SessionCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RefreshToolCatalog { .. } => "refresh_tool_catalog",
        }
    }

    pub fn source_key(&self, idempotency_key: impl AsRef<str>) -> String {
        format!("command:{}:{}", self.kind(), idempotency_key.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCommandReceipt {
    pub session_id: String,
    pub batch_id: String,
    pub source_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPolicy {
    EarliestSafeBoundary,
    AfterCurrentTurnCommit,
}

impl DeliveryPolicy {
    /// Exposes the stable snake-case delivery value for queued-work store implementors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EarliestSafeBoundary => "earliest_safe_boundary",
            Self::AfterCurrentTurnCommit => "after_current_turn_commit",
        }
    }

    /// Parses the stable delivery value for queued-work store implementors, returning `None` for an
    /// unknown value.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "earliest_safe_boundary" => Some(Self::EarliestSafeBoundary),
            "after_current_turn_commit" => Some(Self::AfterCurrentTurnCommit),
            _ => None,
        }
    }
}

/// Constant producer-selected merge key for process wakes.
///
/// The key says only that wake rows are eligible to share a turn. Work kind,
/// delivery boundary, authority, elevation, row count, age, and rendered size
/// remain independent claim gates.
pub const PROCESS_WAKE_MERGE_KEY: &str = "lash.process_wake";

/// Semantic kind of one queued-work row.
///
/// Control and cancellation rows are always claimed alone even when a
/// producer assigns a merge key accidentally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkKind {
    Turn,
    Control,
    Cancel,
}

impl QueuedWorkKind {
    pub fn is_batchable(self) -> bool {
        matches!(self, Self::Turn)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Control => "control",
            Self::Cancel => "cancel",
        }
    }

    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "turn" => Some(Self::Turn),
            "control" => Some(Self::Control),
            "cancel" => Some(Self::Cancel),
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
    pub fn new(principal: impl Into<String>) -> Self {
        Self {
            principal: Some(principal.into()),
            elevation: None,
        }
    }

    pub fn with_elevation(mut self, elevation: impl Into<String>) -> Self {
        self.elevation = Some(elevation.into());
        self
    }
}

/// Host policy bounding one automatically selected queued-work claim.
///
/// The action reserve is required and has no Lash default. Row count and
/// maximum pending age retain Lash defaults because a poor choice affects
/// batching efficiency rather than context-window correctness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuedWorkBatchingConfig {
    action_token_reserve: std::num::NonZeroUsize,
    max_rows: std::num::NonZeroUsize,
    max_pending_age: std::time::Duration,
}

impl QueuedWorkBatchingConfig {
    pub const DEFAULT_MAX_ROWS: usize = 64;
    pub const DEFAULT_MAX_PENDING_AGE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Construct batching policy with an explicit non-zero model-action
    /// reserve and defaulted row-count and pending-age bounds.
    ///
    /// # Panics
    ///
    /// Panics when `action_token_reserve` is zero.
    pub const fn new(action_token_reserve: usize) -> Self {
        let Some(action_token_reserve) = std::num::NonZeroUsize::new(action_token_reserve) else {
            panic!("queued-work action token reserve must be non-zero");
        };
        Self {
            action_token_reserve,
            max_rows: std::num::NonZeroUsize::new(Self::DEFAULT_MAX_ROWS)
                .expect("default queued-work row bound is non-zero"),
            max_pending_age: Self::DEFAULT_MAX_PENDING_AGE,
        }
    }

    /// # Panics
    ///
    /// Panics when `max_rows` is zero.
    pub const fn with_max_rows(mut self, max_rows: usize) -> Self {
        let Some(max_rows) = std::num::NonZeroUsize::new(max_rows) else {
            panic!("queued-work max rows must be non-zero");
        };
        self.max_rows = max_rows;
        self
    }

    /// # Panics
    ///
    /// Panics when `max_pending_age` is zero.
    pub const fn with_max_pending_age(mut self, max_pending_age: std::time::Duration) -> Self {
        assert!(
            !max_pending_age.is_zero(),
            "queued-work max pending age must be non-zero"
        );
        self.max_pending_age = max_pending_age;
        self
    }

    pub const fn action_token_reserve(self) -> usize {
        self.action_token_reserve.get()
    }

    pub const fn max_rows(self) -> usize {
        self.max_rows.get()
    }

    pub const fn max_pending_age(self) -> std::time::Duration {
        self.max_pending_age
    }

    pub(crate) fn claim_policy(self, max_context_tokens: usize) -> QueuedWorkClaimPolicy {
        QueuedWorkClaimPolicy {
            max_context_tokens,
            action_token_reserve: self.action_token_reserve(),
            max_rows: self.max_rows(),
            max_pending_age_ms: u64::try_from(self.max_pending_age.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// Complete claim-time bounds passed to durable store implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuedWorkClaimPolicy {
    pub max_context_tokens: usize,
    pub action_token_reserve: usize,
    pub max_rows: usize,
    pub max_pending_age_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedWorkPayload {
    ProcessWake {
        wake: Box<ProcessWakeDelivery>,
    },
    AgentFrameTask {
        frame_id: crate::AgentFrameId,
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
        frame_id: impl Into<crate::AgentFrameId>,
        task: impl Into<String>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    ) -> Self {
        Self::AgentFrameTask {
            frame_id: frame_id.into(),
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
    pub batch_id: String,
    pub session_id: String,
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
    pub fn work_class(&self) -> Option<QueuedWorkClass> {
        work_class_for_payloads(self.items.iter().map(|item| &item.payload))
    }

    pub fn is_session_command_work(&self) -> bool {
        self.work_class() == Some(QueuedWorkClass::SessionCommand)
    }

    pub fn is_turn_work(&self) -> bool {
        self.work_class() == Some(QueuedWorkClass::TurnWork)
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkBatchDraft {
    pub session_id: String,
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
    pub kind: QueuedWorkKind,
    pub authority: QueuedWorkAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
    pub available_at_ms: u64,
    pub payloads: Vec<QueuedWorkPayload>,
}

impl QueuedWorkBatchDraft {
    pub fn new(
        session_id: impl Into<String>,
        delivery_policy: DeliveryPolicy,
        payloads: impl Into<Vec<QueuedWorkPayload>>,
    ) -> Self {
        let payloads = payloads.into();
        let kind =
            if work_class_for_payloads(payloads.iter()) == Some(QueuedWorkClass::SessionCommand) {
                QueuedWorkKind::Control
            } else {
                QueuedWorkKind::Turn
            };
        Self {
            session_id: session_id.into(),
            source_key: None,
            process_wake_source: None,
            delivery_policy,
            kind,
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
        process_id: impl Into<String>,
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

    pub fn with_kind(mut self, kind: QueuedWorkKind) -> Self {
        self.kind = kind;
        self
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

    pub fn work_class(&self) -> Option<QueuedWorkClass> {
        work_class_for_payloads(self.payloads.iter())
    }

    #[doc(hidden)]
    pub fn validate_process_wake_source(&self) -> Result<(), String> {
        match (
            self.process_wake_source.as_ref(),
            self.payloads.as_slice(),
        ) {
            (
                Some(source),
                [QueuedWorkPayload::ProcessWake { wake }],
            ) if wake.target_session_id == self.session_id
                && wake.process_id == source.process_id
                && wake.sequence == source.sequence
                && source.sequence <= i64::MAX as u64
                && self.source_key.as_deref()
                    == Some(process_wake_source_key(&source.process_id, source.sequence).as_str()) =>
            {
                Ok(())
            }
            (None, payloads)
                if !payloads
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
    pub process_id: String,
    pub sequence: u64,
}

fn work_class_for_payloads<'a>(
    payloads: impl IntoIterator<Item = &'a QueuedWorkPayload>,
) -> Option<QueuedWorkClass> {
    let mut payloads = payloads.into_iter();
    let first = payloads.next()?.work_class();
    payloads
        .all(|payload| payload.work_class() == first)
        .then_some(first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkClaimBoundary {
    ActiveTurnCheckpoint,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkCompletionData {
    pub batch_ids: Vec<String>,
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
    pub(crate) abandon_restore_claim_id: Option<String>,
}

/// A shared work claim carrying queued-work batches.
pub type QueuedWorkClaim = crate::WorkClaim<QueuedWorkClaimData>;

impl crate::WorkClaim<QueuedWorkClaimData> {
    /// Builds the settlement receipt a queued-work store implementor passes back after applying
    /// every batch in this claim.
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

    /// Reports whether a queued-work store or conformance-suite implementor received no items.
    pub fn is_empty(&self) -> bool {
        self.batches.iter().all(|batch| batch.items.is_empty())
    }

    /// Materializes checkpoint input from a claim for runtime and conformance-suite implementors.
    pub fn materialize_queued_checkpoint_work(&self) -> QueuedCheckpointWork {
        let messages = Vec::new();
        let transient_messages = Vec::new();
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
        QueuedCheckpointWork {
            messages,
            transient_messages,
            turn_causes,
        }
    }

    /// Materializes checkpoint input through the attachment-aware seam used by runtime and
    /// conformance-suite implementors.
    pub async fn materialize_queued_checkpoint_work_with_attachments(
        &self,
        _attachment_store: &crate::SessionAttachmentStore,
    ) -> Result<QueuedCheckpointWork, String> {
        let messages = Vec::new();
        let transient_messages = Vec::new();
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
        Ok(QueuedCheckpointWork {
            messages,
            transient_messages,
            turn_causes,
        })
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
            messages: checkpoint.messages,
            turn_causes: checkpoint.turn_causes,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueuedCheckpointWork {
    pub messages: Vec<PluginMessage>,
    pub transient_messages: Vec<PluginMessage>,
    pub turn_causes: Vec<TurnCause>,
}

#[derive(Clone, Debug)]
pub struct QueuedTurnWork {
    pub input: TurnInput,
    pub messages: Vec<PluginMessage>,
    pub turn_causes: Vec<TurnCause>,
}

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
        vec![QueuedWorkPayload::process_wake(wake)],
    )
    .with_source_key(source_key)
    .with_process_wake_source(process_id, sequence)
    .with_authority(authority)
    .with_merge_key(PROCESS_WAKE_MERGE_KEY)
}

pub fn process_wake_source_key(process_id: &str, sequence: u64) -> String {
    format!("process:{process_id}:event:{sequence}:wake")
}
