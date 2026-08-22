use crate::{CheckpointKind, PluginMessage, TurnCause, TurnInput};
use sha2::{Digest, Sha256};

/// Mint a newly created pending turn-input ID from explicit deterministic facts.
///
/// The stable format is `ti:<sha256-hex>`, where the digest input remains the
/// FIG-886 continuity seed
/// `{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}`. Callers must supply a
/// `(now_epoch_ms, nonce)` pair unique per `(session_id, source_key)` across
/// every process writing the store; existing persisted IDs are never rewritten.
#[doc(hidden)]
#[must_use]
pub fn derive_pending_turn_input_id(
    session_id: &str,
    source_key: Option<&str>,
    now_epoch_ms: u64,
    nonce: u64,
) -> String {
    format!(
        "ti:{:x}",
        Sha256::digest(format!("{session_id}:{source_key:?}:{now_epoch_ms}:{nonce}").as_bytes())
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

/// Generates the boundary enumeration and its wire spelling from one variant list.
///
/// The generated `as_wire_str` match is exhaustive, so a new
/// [`TurnInputCheckpointBoundary`] variant fails to compile until it is added here, and adding it
/// here necessarily extends `ALL`. That is what keeps the SQL admission list
/// ([`crate::store_backend_support::admitted_min_boundary_sql`]) complete: the list cannot silently
/// omit a boundary the type can hold.
macro_rules! turn_input_checkpoint_boundary_wire {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        impl TurnInputCheckpointBoundary {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The stable snake-case value persisted in `ingress_json.min_boundary`.
            pub(crate) fn as_wire_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

turn_input_checkpoint_boundary_wire!(
    AfterWork => "after_work",
    BeforeCompletion => "before_completion",
);

/// Generates the checkpoint enumeration a claim can name, from one variant list.
///
/// The generated match is exhaustive, so a new [`CheckpointKind`] variant fails to compile until it
/// is added here, which keeps `CLAIM_CHECKPOINTS` complete for the tests that sweep every
/// checkpoint.
macro_rules! turn_input_claim_checkpoints {
    ($($variant:ident),+ $(,)?) => {
        #[cfg(test)]
        pub(crate) const CLAIM_CHECKPOINTS: &[CheckpointKind] = &[$(CheckpointKind::$variant),+];

        /// Compile-time guard only: the exhaustive match below is what fails on a new variant.
        #[cfg(test)]
        #[allow(dead_code)]
        fn assert_claim_checkpoints_are_exhaustive(checkpoint: CheckpointKind) {
            match checkpoint {
                $(CheckpointKind::$variant => ()),+
            }
        }
    };
}

turn_input_claim_checkpoints!(AfterWork, BeforeCompletion);

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
    pub input_ids: Vec<String>,
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
    pub session_id: String,
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

/// Turn-input rows a turn accepted itself and drives without a claim.
///
/// The unclaimed half of a turn's drive: the rows exist durably before the
/// turn executes, exactly as a claimed row does, but no session-execution lease
/// fences them. Their settlement is decided by the head CAS alone
/// ([ADR 0069 §5](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UnclaimedTurnInputs {
    pub session_id: String,
    pub inputs: Vec<PendingTurnInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}

impl UnclaimedTurnInputs {
    /// Exposes settlement to store and durable-substrate implementors driving
    /// rows they accepted themselves.
    pub fn completion(&self) -> TurnInputCompletion {
        TurnInputCompletion {
            session_id: self.session_id.clone(),
            claim: None,
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

    /// Records the initial application evidence for rows driven without a
    /// claim, matching [`TurnInputClaim::record_initial_turn_application`].
    pub fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) {
        self.applications = initial_turn_applications(&self.inputs, turn_id, committed_message_id);
    }
}

/// The turn-input rows one turn is driving, with the authority it will settle
/// them under.
///
/// Exactly two regimes, one authority: a claimed drive settles under its
/// generation-fenced claim, an unclaimed drive settles under the head CAS
/// alone. Everything between acceptance and settlement — materialization,
/// application evidence, the committed message's provenance — is identical, so
/// the runtime carries both through one list rather than two.
#[derive(Clone, Debug)]
pub(crate) enum TurnInputDrive {
    Claimed(TurnInputClaim),
    Unclaimed(UnclaimedTurnInputs),
}

impl TurnInputDrive {
    pub(crate) fn inputs(&self) -> &[PendingTurnInput] {
        match self {
            Self::Claimed(claim) => &claim.inputs,
            Self::Unclaimed(unclaimed) => &unclaimed.inputs,
        }
    }

    pub(crate) fn applications(&self) -> &[TurnInputApplication] {
        match self {
            Self::Claimed(claim) => &claim.applications,
            Self::Unclaimed(unclaimed) => &unclaimed.applications,
        }
    }

    pub(crate) fn completion(&self) -> TurnInputCompletion {
        match self {
            Self::Claimed(claim) => claim.completion(),
            Self::Unclaimed(unclaimed) => unclaimed.completion(),
        }
    }

    /// The lease generation this drive is fenced by, or `None` when it holds no
    /// claim. Only a claimed drive can be superseded by a later generation, so
    /// only a claimed drive can have its settlement dropped and retried.
    pub(crate) fn claim_generation(&self) -> Option<(String, u64)> {
        match self {
            Self::Claimed(claim) => Some((claim.claim_id.clone(), claim.session_lease_generation)),
            Self::Unclaimed(_) => None,
        }
    }

    pub(crate) fn as_claim(&self) -> Option<&TurnInputClaim> {
        match self {
            Self::Claimed(claim) => Some(claim),
            Self::Unclaimed(_) => None,
        }
    }

    pub(crate) fn materialize_turn_input(&self) -> TurnInput {
        match self {
            Self::Claimed(claim) => claim.materialize_turn_input(),
            Self::Unclaimed(unclaimed) => materialize_turn_input(&unclaimed.inputs),
        }
    }

    pub(crate) fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) {
        match self {
            Self::Claimed(claim) => {
                claim.record_initial_turn_application(turn_id, committed_message_id);
            }
            Self::Unclaimed(unclaimed) => {
                unclaimed.record_initial_turn_application(turn_id, committed_message_id);
            }
        }
    }
}

fn initial_turn_applications(
    inputs: &[PendingTurnInput],
    turn_id: &crate::TurnId,
    committed_message_id: &str,
) -> Vec<TurnInputApplication> {
    inputs
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
        .collect()
}

fn materialize_turn_input(inputs: &[PendingTurnInput]) -> TurnInput {
    let mut input_items = Vec::new();
    let mut protocol_turn_options = None;
    let mut trace_turn_id = None;
    for pending in inputs {
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
    turn_id: &crate::TurnId,
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
                parts.push(crate::Part::text(part_id, text, None));
            }
            super::NormalizedItem::Text(_) => {}
            super::NormalizedItem::Attachment(source) => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part::attachment_part(
                    part_id,
                    String::new(),
                    Some(crate::session_model::message::PartAttachment { source }),
                ));
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::Message {
        id: message_id,
        role: crate::MessageRole::User,
        // Same typed provenance the turn's opening input carries: the absorbing
        // turn plus the durable input this message came from (FIG-972).
        origin: Some(crate::MessageOrigin::TurnInput {
            turn_id: turn_id.to_string(),
            input_id: Some(pending.input_id.clone()),
        }),
        parts: crate::shared_parts(parts),
    }))
}

impl crate::TurnInput {
    /// The part of this input a durable acceptance row can carry.
    ///
    /// `protocol_extension` and live `TurnContext` plugin inputs are
    /// process-local handles that no store can hold, so the acceptance commit
    /// records everything else and the caller driving the turn keeps the live
    /// state (ADR 0069). A worker that later recovers the row drives exactly
    /// this projection.
    ///
    /// `trace_turn_id` is dropped for the same reason: it labels one drive
    /// attempt, not the input. A recovered row is driven under the recovering
    /// worker's own execution scope, and a persisted trace id from the
    /// abandoned attempt would collide with it
    /// ([`RuntimeErrorCode::ExecutionScopeTurnIdMismatch`](crate::RuntimeErrorCode::ExecutionScopeTurnIdMismatch)),
    /// making an accepted direct turn unrecoverable — exactly the property
    /// ADR 0069 exists to guarantee.
    #[must_use]
    pub(crate) fn durable_projection(&self) -> Self {
        Self {
            items: self.items.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: crate::TurnContext::default(),
        }
    }
}

pub(crate) fn ingress_message_id(input_id: &str) -> String {
    format!("m_ingress_{input_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_boundary_wire_values_match_the_persisted_ingress_encoding() {
        for boundary in TurnInputCheckpointBoundary::ALL.iter().copied() {
            assert_eq!(
                serde_json::to_value(boundary).expect("serialize boundary"),
                serde_json::Value::String(boundary.as_wire_str().to_string()),
                "the SQL literal a store filters on must equal the persisted wire value"
            );
        }
    }

    #[test]
    fn every_checkpoint_admits_at_least_the_default_boundary() {
        for checkpoint in CLAIM_CHECKPOINTS.iter().copied() {
            let admitted = TurnInputCheckpointBoundary::ALL
                .iter()
                .filter(|boundary| boundary.admits(checkpoint))
                .collect::<Vec<_>>();
            assert!(
                admitted.contains(&&TurnInputCheckpointBoundary::default()),
                "an absent min_boundary reads as the default, which must stay admissible at {checkpoint:?}"
            );
            let predicate =
                crate::store_backend_support::admitted_min_boundary_sql("min_boundary", checkpoint);
            assert!(
                !predicate.contains("IN ()"),
                "an empty admitted set must not emit an `IN ()` syntax error at {checkpoint:?}"
            );
        }
        assert!(
            !TurnInputCheckpointBoundary::BeforeCompletion.admits(CheckpointKind::AfterWork),
            "before-completion ingress must be withheld at the after-work checkpoint"
        );
    }

    #[test]
    fn admitted_min_boundary_sql_spells_the_current_checkpoints_exactly() {
        assert_eq!(
            crate::store_backend_support::admitted_min_boundary_sql(
                "min_boundary",
                CheckpointKind::AfterWork
            ),
            "COALESCE(min_boundary, 'after_work') IN ('after_work')"
        );
        assert_eq!(
            crate::store_backend_support::admitted_min_boundary_sql(
                "min_boundary",
                CheckpointKind::BeforeCompletion
            ),
            "COALESCE(min_boundary, 'after_work') IN ('after_work', 'before_completion')"
        );
    }

    #[test]
    fn pending_turn_input_id_mint_preserves_the_fig_886_format() {
        assert_eq!(
            derive_pending_turn_input_id("session", Some("source"), 123, 7),
            "ti:57f04cf281275e1916bcbbb9b8541466110f642648709b4b45bb3d6bfcf68698"
        );
        assert_ne!(
            derive_pending_turn_input_id("session", Some("source"), 123, 7),
            derive_pending_turn_input_id("session", Some("source"), 123, 8)
        );
    }
}
