use crate::{CheckpointKind, PluginMessage, TurnCause, TurnInput};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum TurnInputIngress {
    ActiveTurn {
        turn_id: crate::TurnId,
        #[serde(default)]
        min_boundary: TurnInputCheckpointBoundary,
    },
    NextTurn,
}

impl TurnInputIngress {
    /// Routes an input to an active turn at or after the named checkpoint boundary for turn-input
    /// store implementors.
    pub fn active_turn(
        turn_id: impl Into<crate::TurnId>,
        min_boundary: TurnInputCheckpointBoundary,
    ) -> Self {
        Self::ActiveTurn {
            turn_id: turn_id.into(),
            min_boundary,
        }
    }

    /// Routes an input to the next idle turn for turn-input store implementors; it is never
    /// admitted at an active-turn checkpoint.
    pub fn next_turn() -> Self {
        Self::NextTurn
    }

    /// Exposes the target turn ID to turn-input store implementors for active-turn ingress,
    /// returning `None` for next-turn ingress.
    pub fn active_turn_id(&self) -> Option<&str> {
        match self {
            Self::ActiveTurn { turn_id, .. } => Some(turn_id.as_str()),
            Self::NextTurn => None,
        }
    }

    /// Lets turn-input store implementors admit only active-turn ingress whose minimum boundary has
    /// been reached; next-turn ingress never enters a running turn.
    pub fn admits_checkpoint(&self, checkpoint: CheckpointKind) -> bool {
        match self {
            Self::ActiveTurn { min_boundary, .. } => min_boundary.admits(checkpoint),
            Self::NextTurn => false,
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputCheckpointBoundary {
    #[default]
    AfterWork,
    BeforeCompletion,
}

impl TurnInputCheckpointBoundary {
    /// Treats `AfterWork` as admitting both checkpoints and `BeforeCompletion` as admitting only
    /// that final checkpoint for turn-input store implementors.
    pub fn admits(self, checkpoint: CheckpointKind) -> bool {
        match self {
            Self::AfterWork => true,
            Self::BeforeCompletion => checkpoint == CheckpointKind::BeforeCompletion,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputState {
    PendingActive,
    DeferredNextTurn,
    Accepted,
    Cancelled,
    Completed,
}

impl TurnInputState {
    /// Exposes the stable snake-case lifecycle value for turn-input store implementors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingActive => "pending_active",
            Self::DeferredNextTurn => "deferred_next_turn",
            Self::Accepted => "accepted",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
        }
    }

    /// Parses the stable snake-case lifecycle value for store implementors, returning `None` for an
    /// unknown value instead of inventing a state.
    pub fn from_wire_str(value: &str) -> Option<Self> {
        match value {
            "pending_active" => Some(Self::PendingActive),
            "deferred_next_turn" => Some(Self::DeferredNextTurn),
            "accepted" => Some(Self::Accepted),
            "cancelled" => Some(Self::Cancelled),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    /// Lets store, effect-host, and protocol implementors test whether this `TurnInputState` is
    /// next turn pending while materializing, executing, or persisting a session turn.
    pub fn is_next_turn_pending(self) -> bool {
        matches!(self, Self::DeferredNextTurn)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputDraft {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub ingress: TurnInputIngress,
    pub input: TurnInput,
}

impl PendingTurnInputDraft {
    /// Constructs a `PendingTurnInputDraft` for store and durable-substrate implementors while
    /// claiming and settling durable turn inputs.
    pub fn new(session_id: impl Into<String>, ingress: TurnInputIngress, input: TurnInput) -> Self {
        Self {
            session_id: session_id.into(),
            input_id: None,
            source_key: None,
            ingress,
            input,
        }
    }

    /// Sets the input id carried by a `PendingTurnInputDraft` for store and durable-substrate
    /// implementors while claiming and settling durable turn inputs.
    pub fn with_input_id(mut self, input_id: impl Into<String>) -> Self {
        self.input_id = Some(input_id.into());
        self
    }

    /// Sets the source key carried by a `PendingTurnInputDraft` for store and durable-substrate
    /// implementors while claiming and settling durable turn inputs.
    pub fn with_source_key(mut self, source_key: impl Into<String>) -> Self {
        self.source_key = Some(source_key.into());
        self
    }

    /// Compares ingress and canonical JSON input for turn-input store implementors enforcing
    /// source-key idempotency; generated input IDs and mutable lifecycle fields are deliberately
    /// ignored.
    pub fn submitted_content_matches(
        &self,
        existing: &PendingTurnInput,
    ) -> Result<bool, serde_json::Error> {
        Ok(self.ingress == existing.ingress
            && serde_json::to_value(&self.input)? == serde_json::to_value(&existing.input)?)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInput {
    pub input_id: String,
    pub session_id: String,
    pub enqueue_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub ingress: TurnInputIngress,
    pub state: TurnInputState,
    pub enqueued_at_ms: u64,
    pub input: TurnInput,
}

/// Durable acceptance evidence returned to an ingress caller.
///
/// The receipt deliberately carries only stable routing and idempotency
/// identity. Queue dispatch and the pending row's mutable lifecycle state are
/// observed separately through the pending-input reconciliation surface.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputAcceptanceReceipt {
    pub input_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub ingress: TurnInputIngress,
}

/// Durable evidence that an admitted input became canonical conversation input.
///
/// This is the application stage between [`TurnInputAcceptanceReceipt`]
/// (admission) and the terminal turn commit (settlement). It deliberately
/// carries identity only: hosts correlate an input to its canonical turn and
/// committed message without parsing or retaining display text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputApplication {
    pub input_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub turn_id: crate::TurnId,
    pub committed_message_id: String,
    /// Present for active-turn checkpoint application and absent when the
    /// input formed the initial canonical input of an idle queued turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointKind>,
}

impl From<&PendingTurnInput> for TurnInputAcceptanceReceipt {
    fn from(input: &PendingTurnInput) -> Self {
        Self {
            input_id: input.input_id.clone(),
            session_id: input.session_id.clone(),
            source_key: input.source_key.clone(),
            ingress: input.ingress.clone(),
        }
    }
}

impl PendingTurnInput {
    /// Returns the source key when present and otherwise the durable input ID for store
    /// implementors producing a stable reconciliation key.
    pub fn source_or_id(&self) -> &str {
        self.source_key.as_deref().unwrap_or(&self.input_id)
    }

    /// Exposes accepted input to store and durable-substrate implementors while claiming and
    /// settling durable turn inputs. Returns `None` when no accepted input is present.
    pub fn accepted_input(&self) -> Option<crate::AcceptedInjectedTurnInput> {
        plugin_message_from_turn_input(&self.input).map(|message| {
            crate::AcceptedInjectedTurnInput {
                id: self
                    .source_key
                    .as_deref()
                    .map(source_key_display_id)
                    .or_else(|| Some(self.input_id.clone())),
                message,
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PendingTurnInputCancelTarget {
    InputId(String),
    SourceKey(String),
}

impl PendingTurnInputCancelTarget {
    /// Targets one durable input ID for turn-input store implementors performing cancellation.
    pub fn input_id(input_id: impl Into<String>) -> Self {
        Self::InputId(input_id.into())
    }

    /// Targets the input admitted under a source idempotency key for turn-input store implementors
    /// performing cancellation.
    pub fn source_key(source_key: impl Into<String>) -> Self {
        Self::SourceKey(source_key.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputClaimDiagnostics {
    pub state: TurnInputState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<crate::LeaseOwnerIdentity>,
    /// The session-execution-lease generation the live claim pins, when a claim
    /// holds the row. `None` when the row carries no claim (ADR 0029).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_session_lease_generation: Option<u64>,
    pub claim_fencing_token: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum PendingTurnInputCancelOutcome {
    Cancelled(PendingTurnInput),
    AlreadyClaimed {
        input: PendingTurnInput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<PendingTurnInputClaimDiagnostics>,
    },
    AlreadyCompleted(PendingTurnInput),
    AlreadyCancelled(PendingTurnInput),
    NotFound,
}

impl PendingTurnInputCancelOutcome {
    /// Reports success to turn-input store implementors only for the transition performed by this
    /// cancellation attempt, not for an already-cancelled row.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    /// Returns the durable input for every found cancellation outcome and `None` only when the
    /// target was absent.
    pub fn input(&self) -> Option<&PendingTurnInput> {
        match self {
            Self::Cancelled(input)
            | Self::AlreadyClaimed { input, .. }
            | Self::AlreadyCompleted(input)
            | Self::AlreadyCancelled(input) => Some(input),
            Self::NotFound => None,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputCancelResult {
    pub target: PendingTurnInputCancelTarget,
    pub outcome: PendingTurnInputCancelOutcome,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum PendingTurnInputSuffixCancelOutcome {
    AnchorNotFound {
        anchor: PendingTurnInputCancelTarget,
    },
    Outcomes {
        anchor: PendingTurnInputCancelTarget,
        outcomes: Vec<PendingTurnInputCancelOutcome>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnInputClaimMode {
    ActiveTurn {
        turn_id: crate::TurnId,
        checkpoint: CheckpointKind,
    },
    NextTurn,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputCompletionData {
    pub input_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}

/// A shared work completion carrying settled turn-input identities and
/// application evidence.
pub type TurnInputCompletion = crate::WorkCompletion<TurnInputCompletionData>;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnInputClaimData {
    pub mode: TurnInputClaimMode,
    pub inputs: Vec<PendingTurnInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}

/// A shared work claim carrying pending turn-input material.
pub type TurnInputClaim = crate::WorkClaim<TurnInputClaimData>;

impl crate::WorkClaim<TurnInputClaimData> {
    /// Exposes completion to store and durable-substrate implementors while claiming and settling
    /// durable queued work.
    pub fn completion(&self) -> TurnInputCompletion {
        TurnInputCompletion {
            session_id: self.session_id.clone(),
            claim_id: self.claim_id.clone(),
            lease_token: self.lease_token.clone(),
            data: TurnInputCompletionData {
                input_ids: self
                    .inputs
                    .iter()
                    .map(|input| input.input_id.clone())
                    .collect(),
                applications: self.applications.clone(),
            },
        }
    }

    /// Updates initial turn application state for store and durable-substrate implementors while
    /// claiming and settling durable queued work.
    pub fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) {
        self.applications = self
            .inputs
            .iter()
            .filter(|input| {
                input.input.items.iter().any(|item| match item {
                    crate::InputItem::Text { text } => !text.is_empty(),
                    crate::InputItem::Attachment { .. } => true,
                })
            })
            .map(|input| TurnInputApplication {
                input_id: input.input_id.clone(),
                source_key: input.source_key.clone(),
                turn_id: turn_id.clone(),
                committed_message_id: committed_message_id.to_string(),
                checkpoint: None,
            })
            .collect();
    }

    /// Records application evidence only for claimed inputs whose deterministic ingress message IDs
    /// appear in the committed checkpoint messages.
    pub fn record_checkpoint_applications(
        &mut self,
        turn_id: &crate::TurnId,
        checkpoint: CheckpointKind,
        committed_messages: &[crate::Message],
    ) {
        let committed_message_ids = committed_messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let recorded = self
            .inputs
            .iter()
            .filter_map(|input| {
                let committed_message_id = ingress_message_id(&input.input_id);
                committed_message_ids
                    .contains(committed_message_id.as_str())
                    .then(|| TurnInputApplication {
                        input_id: input.input_id.clone(),
                        source_key: input.source_key.clone(),
                        turn_id: turn_id.clone(),
                        committed_message_id,
                        checkpoint: Some(checkpoint),
                    })
            })
            .collect::<Vec<_>>();
        self.applications.retain(|application| {
            !recorded
                .iter()
                .any(|replacement| replacement.input_id == application.input_id)
        });
        self.applications.extend(recorded);
    }

    /// Exposes accepted turn inputs to store and durable-substrate implementors while claiming and
    /// settling durable queued work.
    pub fn accepted_turn_inputs(&self) -> Vec<crate::AcceptedInjectedTurnInput> {
        self.inputs
            .iter()
            .filter_map(PendingTurnInput::accepted_input)
            .collect()
    }

    /// Materializes claimed inputs in claim order for turn-input store implementors, resolving
    /// attachments and omitting inputs that produce no committed message.
    pub async fn materialize_checkpoint_turn_input(
        &self,
        attachment_store: &crate::SessionAttachmentStore,
        attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
    ) -> Result<QueuedCheckpointTurnInput, String> {
        let mut messages = Vec::new();
        for input in &self.inputs {
            if let Some(message) = committed_message_from_pending_input(
                input,
                attachment_store,
                attachment_source_policy,
            )
            .await?
            {
                messages.push(message);
            }
        }
        Ok(QueuedCheckpointTurnInput {
            messages,
            turn_causes: Vec::new(),
        })
    }

    /// Materializes for turn data for store and durable-substrate implementors while claiming and
    /// settling durable queued work.
    pub fn materialize_turn_input(&self) -> TurnInput {
        let mut input_items = Vec::new();
        let mut protocol_turn_options = None;
        let mut trace_turn_id = None;
        for pending in &self.inputs {
            input_items.extend(pending.input.items.clone());
            if protocol_turn_options.is_none() {
                protocol_turn_options = pending.input.protocol_turn_options.clone();
            }
            if trace_turn_id.is_none() {
                trace_turn_id = pending.input.trace_turn_id.clone();
            }
        }
        TurnInput {
            items: input_items,
            protocol_turn_options,
            trace_turn_id,
            protocol_extension: None,
            turn_context: crate::TurnContext::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct QueuedCheckpointTurnInput {
    pub messages: Vec<crate::Message>,
    pub turn_causes: Vec<TurnCause>,
}

pub(crate) fn source_key_display_id(source: &str) -> String {
    source
        .strip_prefix("host:")
        .or_else(|| source.strip_prefix("injection:"))
        .unwrap_or(source)
        .to_string()
}

pub(crate) fn plugin_message_from_turn_input(input: &TurnInput) -> Option<PluginMessage> {
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for item in &input.items {
        match item {
            crate::InputItem::Text { text: item_text } if !item_text.is_empty() => {
                text.push(item_text.clone());
            }
            crate::InputItem::Text { .. } => {}
            crate::InputItem::Attachment { source } => attachments.push(source.clone()),
        }
    }
    if text.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(PluginMessage {
        id: None,
        role: crate::MessageRole::User,
        content: text.join("\n"),
        origin: None,
        parts: Vec::new(),
        attachments,
    })
}

async fn committed_message_from_pending_input(
    pending: &PendingTurnInput,
    attachment_store: &crate::SessionAttachmentStore,
    attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<Option<crate::Message>, String> {
    let normalized = super::io::normalize_input_items(
        &pending.input.items,
        attachment_store,
        attachment_source_policy,
    )
    .await?;
    let message_id = ingress_message_id(&pending.input_id);
    let mut parts = Vec::new();
    for item in normalized {
        match item {
            super::NormalizedItem::Text(text) if !text.is_empty() => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part {
                    id: part_id,
                    kind: crate::PartKind::Text,
                    content: text,
                    attachment: None,
                    tool_call_id: None,
                    tool_name: None,
                    tool_replay: None,
                    prune_state: crate::PruneState::Intact,
                    reasoning_meta: None,
                    response_meta: None,
                });
            }
            super::NormalizedItem::Text(_) => {}
            super::NormalizedItem::Attachment(source) => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part {
                    id: part_id,
                    kind: crate::PartKind::Attachment,
                    content: String::new(),
                    attachment: Some(crate::session_model::message::PartAttachment { source }),
                    tool_call_id: None,
                    tool_name: None,
                    tool_replay: None,
                    prune_state: crate::PruneState::Intact,
                    reasoning_meta: None,
                    response_meta: None,
                });
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::Message {
        id: message_id,
        role: crate::MessageRole::User,
        origin: None,
        parts: crate::shared_parts(parts),
    }))
}

pub(crate) fn ingress_message_id(input_id: &str) -> String {
    format!("m_ingress_{input_id}")
}
