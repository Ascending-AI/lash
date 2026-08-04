use super::process::ProcessWakeDelivery;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotPolicy {
    Join,
    Exclusive,
}

impl SlotPolicy {
    /// Exposes the stable snake-case slot value for queued-work store implementors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Exclusive => "exclusive",
        }
    }

    /// Parses the stable slot value for queued-work store implementors, returning `None` for an
    /// unknown value.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "join" => Some(Self::Join),
            "exclusive" => Some(Self::Exclusive),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeKey {
    Never,
    PayloadDefault,
    Group(String),
}

/// A non-empty receiver-side key used only when wake coalescing is enabled.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WakeCoalescingKey {
    /// Use the queued payload's default merge identity.
    PayloadDefault,
    /// Join wakes assigned to one host-defined group.
    Group(String),
}

impl WakeCoalescingKey {
    fn as_queue_merge_key(&self) -> MergeKey {
        match self {
            Self::PayloadDefault => MergeKey::PayloadDefault,
            Self::Group(group) => MergeKey::Group(group.clone()),
        }
    }
}

/// Whether receiver-side wake claims stay separate or coalesce.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WakeTurnMode {
    /// Deliver every durable wake as its own turn claim.
    EachWake { slot: SlotPolicy },
    /// Coalesce adjacent wakes that share the selected merge key.
    Coalesce { key: WakeCoalescingKey },
}

/// Factory-scoped policy for turning durable process wakes into queued turns.
///
/// Producer-side wake deduplication remains keyed by process event identity.
/// This policy controls only receiver-side delivery and drain coalescing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeTurnPolicy {
    delivery: DeliveryPolicy,
    mode: WakeTurnMode,
}

impl WakeTurnPolicy {
    /// Build a policy from an independently selected delivery boundary and a
    /// coherent receiver claim mode.
    pub fn new(delivery: DeliveryPolicy, mode: WakeTurnMode) -> Self {
        Self { delivery, mode }
    }

    /// Deliver every wake as a separate claim.
    pub fn each_wake(delivery: DeliveryPolicy, slot: SlotPolicy) -> Self {
        Self::new(delivery, WakeTurnMode::EachWake { slot })
    }

    /// Coalesce adjacent wakes that share `key`.
    pub fn coalesce(delivery: DeliveryPolicy, key: WakeCoalescingKey) -> Self {
        Self::new(delivery, WakeTurnMode::Coalesce { key })
    }

    /// The turn boundary at which a queued wake becomes eligible.
    pub fn delivery(&self) -> DeliveryPolicy {
        self.delivery
    }

    /// The receiver claim mode.
    pub fn mode(&self) -> &WakeTurnMode {
        &self.mode
    }

    pub(crate) fn queue_slot_policy(&self) -> SlotPolicy {
        match self.mode {
            WakeTurnMode::EachWake { slot } => slot,
            WakeTurnMode::Coalesce { .. } => SlotPolicy::Join,
        }
    }

    pub(crate) fn queue_merge_key(&self) -> MergeKey {
        match &self.mode {
            WakeTurnMode::EachWake { .. } => MergeKey::Never,
            WakeTurnMode::Coalesce { key } => key.as_queue_merge_key(),
        }
    }
}

impl Default for WakeTurnPolicy {
    fn default() -> Self {
        Self::each_wake(DeliveryPolicy::EarliestSafeBoundary, SlotPolicy::Exclusive)
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedWorkClass {
    SessionCommand,
    TurnWork,
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
    pub slot_policy: SlotPolicy,
    pub merge_key: MergeKey,
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
    pub slot_policy: SlotPolicy,
    pub merge_key: MergeKey,
    pub available_at_ms: u64,
    pub payloads: Vec<QueuedWorkPayload>,
}

impl QueuedWorkBatchDraft {
    pub fn new(
        session_id: impl Into<String>,
        delivery_policy: DeliveryPolicy,
        slot_policy: SlotPolicy,
        payloads: impl Into<Vec<QueuedWorkPayload>>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            source_key: None,
            process_wake_source: None,
            delivery_policy,
            slot_policy,
            merge_key: MergeKey::Never,
            available_at_ms: 0,
            payloads: payloads.into(),
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

    pub fn with_merge_key(mut self, merge_key: MergeKey) -> Self {
        self.merge_key = merge_key;
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
pub struct QueuedWorkCompletion {
    pub session_id: String,
    pub claim_id: String,
    pub lease_token: String,
    pub batch_ids: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedWorkClaim {
    pub session_id: String,
    pub claim_id: String,
    pub owner: crate::LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
    /// The session-execution-lease generation this claim pins. It controls when
    /// another generation may re-claim the rows, not settlement authority
    /// before that re-claim (ADR 0029).
    pub session_lease_generation: u64,
    pub batches: Vec<QueuedWorkBatch>,
}

impl QueuedWorkClaim {
    pub fn completion(&self) -> QueuedWorkCompletion {
        QueuedWorkCompletion {
            session_id: self.session_id.clone(),
            claim_id: self.claim_id.clone(),
            lease_token: self.lease_token.clone(),
            batch_ids: self
                .batches
                .iter()
                .map(|batch| batch.batch_id.clone())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.batches.iter().all(|batch| batch.items.is_empty())
    }

    pub fn materialize_for_checkpoint(&self) -> QueuedCheckpointWork {
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

    pub async fn materialize_for_checkpoint_with_attachments(
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

    pub fn exclusive_session_command(&self) -> Option<(&QueuedWorkBatch, &SessionCommand)> {
        if self.batches.len() != 1 {
            return None;
        }
        let batch = self.batches.first()?;
        if batch.slot_policy != SlotPolicy::Exclusive || batch.items.len() != 1 {
            return None;
        }
        let item = batch.items.first()?;
        match &item.payload {
            QueuedWorkPayload::SessionCommand { command } => Some((batch, command.as_ref())),
            _ => None,
        }
    }

    pub fn materialize_for_turn(&self) -> QueuedTurnWork {
        let checkpoint = self.materialize_for_checkpoint();
        let mut input = TurnInput::empty();
        for batch in &self.batches {
            for item in &batch.items {
                if let QueuedWorkPayload::AgentFrameTask {
                    task,
                    protocol_turn_options,
                    ..
                } = &item.payload
                {
                    input = TurnInput::text(task.clone());
                    input.protocol_turn_options = protocol_turn_options.clone();
                }
            }
        }
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
    process_wake_batch_draft_with_policy(wake, &WakeTurnPolicy::default())
}

pub(crate) fn process_wake_batch_draft_with_policy(
    wake: ProcessWakeDelivery,
    policy: &WakeTurnPolicy,
) -> QueuedWorkBatchDraft {
    let source_key = process_wake_source_key(&wake.process_id, wake.sequence);
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        policy.delivery(),
        policy.queue_slot_policy(),
        vec![QueuedWorkPayload::process_wake(wake)],
    )
    .with_source_key(source_key)
    .with_process_wake_source(process_id, sequence)
    .with_merge_key(policy.queue_merge_key())
}

pub fn process_wake_source_key(process_id: &str, sequence: u64) -> String {
    format!("process:{process_id}:event:{sequence}:wake")
}
