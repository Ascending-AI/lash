//! Durable turn-input vocabulary.
//!
//! The pending-input rows a store persists, their claim and completion
//! payloads, and the checkpoint boundary rule stores filter on. The ingress
//! driver that normalizes and applies them stays in `lash-core`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Mint a newly created pending turn-input ID from explicit deterministic facts.
///
/// The stable format is `ti:<blake3-hex>`, where the digest input remains the
/// FIG-886 continuity seed
/// `{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}`. Callers must supply a
/// `(now_epoch_ms, nonce)` pair unique per `(session_id, source_key)` across
/// every process writing the store; existing persisted IDs are never rewritten.
#[must_use]
pub fn derive_pending_turn_input_id(
    session_id: &SessionId,
    source_key: Option<&str>,
    now_epoch_ms: u64,
    nonce: u64,
) -> String {
    format!(
        "ti:{}",
        crate::stable_hash::blake3_hex(
            "lash-turn-input/v2",
            format!("{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}").as_bytes(),
        )
    )
}
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
    /// Derives the only legal initial durable state for this ingress scope.
    #[must_use]
    pub fn initial_state(&self) -> TurnInputState {
        match self {
            Self::ActiveTurn { .. } => TurnInputState::PendingActive,
            Self::NextTurn => TurnInputState::DeferredNextTurn,
        }
    }

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
    pub fn active_turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::ActiveTurn { turn_id, .. } => Some(turn_id),
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
    /// Lets store, effect-host, and protocol implementors test whether this `TurnInputState` is
    /// next turn pending while materializing, executing, or persisting a session turn.
    pub fn is_next_turn_pending(self) -> bool {
        matches!(self, Self::DeferredNextTurn)
    }

    /// Returns whether this state is settled and eligible for tombstone vacuum.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed)
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInputDraft {
    pub session_id: SessionId,
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
    pub fn new(
        session_id: impl Into<SessionId>,
        ingress: TurnInputIngress,
        input: TurnInput,
    ) -> Self {
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
    pub input_id: crate::InputId,
    pub session_id: SessionId,
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
    pub input_id: crate::InputId,
    pub session_id: SessionId,
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
    pub input_id: crate::InputId,
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
    /// Exposes accepted input to store and durable-substrate implementors while claiming and
    /// settling durable turn inputs. Returns `None` when no accepted input is present.
    pub fn accepted_input(&self) -> Option<crate::AcceptedInjectedTurnInput> {
        plugin_message_from_turn_input(&self.input).map(|message| {
            crate::AcceptedInjectedTurnInput {
                id: self
                    .source_key
                    .as_deref()
                    .map(source_key_display_id)
                    .or_else(|| Some(self.input_id.to_string())),
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
pub struct PendingTurnInputCancelReceipt {
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
    pub input_ids: Vec<crate::InputId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}
/// The claim a settling driver held over the rows it is settling.
///
/// Present only in the claimed regime; see [`TurnInputCompletion::claim`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputSettlementClaim {
    pub claim_id: String,
    pub lease_token: String,
}
/// A settlement receipt carrying settled turn-input identities, application
/// evidence, and the authority the settling driver holds over those rows.
///
/// Turn-input settlement has exactly two regimes and one authority. Both are a
/// conditional write decided by the head CAS the runtime commit already
/// performs; `claim` only *strengthens* that write's predicate:
///
/// * `Some(..)` — the driver holds a generation-fenced claim
///   ([ADR 0029](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0029-claims-are-generation-fenced-under-the-session-lease.md)),
///   and the row must still carry that claim id and lease token.
/// * `None` — the driver accepted these rows itself and drove them without the
///   session-execution lane
///   ([ADR 0069 §5](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)),
///   and the row must still be unclaimed and unsettled.
///
/// Every backend verifies that the settlement affected exactly one row and
/// reports a typed supersession error otherwise; zero rows is never silent
/// success.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputCompletion {
    pub session_id: SessionId,
    #[serde(default, flatten)]
    pub claim: Option<TurnInputSettlementClaim>,
    #[serde(flatten)]
    pub data: TurnInputCompletionData,
}
impl TurnInputCompletion {
    /// Exposes the settling claim id to store implementors, or `None` when the
    /// settlement is unclaimed.
    pub fn claim_id(&self) -> Option<&str> {
        self.claim.as_ref().map(|claim| claim.claim_id.as_str())
    }

    /// Exposes the settling lease token to store implementors, or `None` when
    /// the settlement is unclaimed.
    pub fn lease_token(&self) -> Option<&str> {
        self.claim.as_ref().map(|claim| claim.lease_token.as_str())
    }

    /// Names this settlement's rows for diagnostics that must report a
    /// settlement without assuming it had a claim id.
    pub fn settlement_identity(&self) -> String {
        match self.claim.as_ref() {
            Some(claim) => claim.claim_id.clone(),
            None => format!("unclaimed:{}", self.data.input_ids.join(",")),
        }
    }
}
impl std::ops::Deref for TurnInputCompletion {
    type Target = TurnInputCompletionData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}
impl std::ops::DerefMut for TurnInputCompletion {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}
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
            claim: Some(TurnInputSettlementClaim {
                claim_id: self.claim_id.clone(),
                lease_token: self.lease_token.clone(),
            }),
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
        self.applications = initial_turn_applications(&self.inputs, turn_id, committed_message_id);
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
        turn_id: &crate::TurnId,
        attachment_store: &crate::SessionAttachmentStore,
        attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
    ) -> Result<QueuedCheckpointTurnInput, String> {
        let mut messages = Vec::new();
        for input in &self.inputs {
            if let Some(message) = committed_message_from_pending_input(
                input,
                turn_id,
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
        materialize_turn_input(&self.inputs)
    }
}
