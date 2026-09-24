//! Durable vocabulary of the one session ingress (ADR 0101).
//!
//! A session has one durable ingress: one row per admitted item, one order,
//! one claim, one settlement planner and one cancel vocabulary. Kind-specific
//! behaviour is a property of the item's [`IngressKind`], never of a separate
//! table or claim type. This module holds the data the ingress persists; the
//! store seam is [`crate::store::SessionIngressStore`] and the planners that
//! every backend shares live in [`crate::store::session_ingress_plan`].

use crate::{
    CheckpointKind, ProcessId, ProcessWakeDelivery, QueuedWorkAuthority, SessionCommand, SessionId,
    TurnCancelDisposition, TurnCancelMode, TurnId, TurnInput, TurnInputCheckpointBoundary,
};

/// The three kinds of item a session admits.
///
/// Frame handoffs are not ingress items: the pending follow-on lives on the
/// session head (ADR 0101 §3).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum IngressKind {
    /// Host input, admitted through durable acceptance (ADR 0069).
    Input,
    /// A process wake, a copy of the process delivery.
    ProcessWake,
    /// A session command, applied only at turn boundaries.
    SessionCommand,
}

impl IngressKind {
    /// Every kind, in wire-tag order.
    pub const ALL: [Self; 3] = [Self::Input, Self::ProcessWake, Self::SessionCommand];

    /// The persisted `kind` column spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::ProcessWake => "process_wake",
            Self::SessionCommand => "session_command",
        }
    }

    /// Decode a persisted `kind` column; `None` for an unknown spelling.
    #[must_use]
    pub fn from_wire_str(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// The lane an item of this kind is ordered in. Fixed by kind.
    #[must_use]
    pub const fn lane(self) -> IngressLane {
        match self {
            Self::Input | Self::ProcessWake => IngressLane::Turn,
            Self::SessionCommand => IngressLane::Command,
        }
    }
}

/// The two class-level lanes of one ingress table (ADR 0101 §4).
///
/// Order is `(lane, enqueue_seq)`: commands drain at every turn boundary
/// before the turn lane is claimed, and neither lane ever blocks the other.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum IngressLane {
    Command,
    Turn,
}

impl IngressLane {
    /// The persisted `lane` column spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Turn => "turn",
        }
    }

    /// Decode a persisted `lane` column; `None` for an unknown spelling.
    #[must_use]
    pub fn from_wire_str(value: &str) -> Option<Self> {
        [Self::Command, Self::Turn]
            .into_iter()
            .find(|lane| lane.as_str() == value)
    }
}

/// Where an item may be delivered: immutable intent, written once at
/// admission and never rewritten (ADR 0101 §5.1).
///
/// Eligibility is derived, not stored: a `Turn { turn_id: T, .. }` item is
/// deliverable only into T while T runs, from `min_boundary` onward, and once
/// T's final commit is recorded it is treated as [`Delivery::NextTurn`] by
/// rule, at its own position.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Delivery {
    /// Addressed to one turn, deliverable at its checkpoints from
    /// `min_boundary` onward.
    Turn {
        turn_id: TurnId,
        min_boundary: TurnInputCheckpointBoundary,
    },
    /// Deliverable at idle and at any checkpoint of a running turn.
    AnyBoundary,
    /// Deliverable only at idle: the next turn a boundary starts.
    NextTurn,
}

impl Delivery {
    /// The persisted `delivery_scope` column spelling.
    #[must_use]
    pub const fn scope_str(&self) -> &'static str {
        match self {
            Self::Turn { .. } => "turn",
            Self::AnyBoundary => "any_boundary",
            Self::NextTurn => "next_turn",
        }
    }

    /// The addressed turn, for a turn-addressed item.
    #[must_use]
    pub fn addressed_turn(&self) -> Option<&TurnId> {
        match self {
            Self::Turn { turn_id, .. } => Some(turn_id),
            Self::AnyBoundary | Self::NextTurn => None,
        }
    }

    /// The persisted `delivery_min_boundary` column spelling, for a
    /// turn-addressed item.
    #[must_use]
    pub const fn min_boundary_str(&self) -> Option<&'static str> {
        match self {
            Self::Turn { min_boundary, .. } => Some(checkpoint_boundary_str(*min_boundary)),
            Self::AnyBoundary | Self::NextTurn => None,
        }
    }

    /// Rebuild a delivery from its three persisted columns. `None` when the
    /// columns disagree (a turn scope without its turn and boundary, or an
    /// unaddressed scope carrying either).
    #[must_use]
    pub fn from_persisted(
        scope: &str,
        turn_id: Option<&str>,
        min_boundary: Option<&str>,
    ) -> Option<Self> {
        match (scope, turn_id, min_boundary) {
            ("turn", Some(turn_id), Some(min_boundary)) => Some(Self::Turn {
                turn_id: TurnId::from(turn_id.to_string()),
                min_boundary: checkpoint_boundary_from_str(min_boundary)?,
            }),
            ("any_boundary", None, None) => Some(Self::AnyBoundary),
            ("next_turn", None, None) => Some(Self::NextTurn),
            _ => None,
        }
    }
}

const fn checkpoint_boundary_str(boundary: TurnInputCheckpointBoundary) -> &'static str {
    match boundary {
        TurnInputCheckpointBoundary::AfterWork => "after_work",
        TurnInputCheckpointBoundary::BeforeCompletion => "before_completion",
    }
}

fn checkpoint_boundary_from_str(value: &str) -> Option<TurnInputCheckpointBoundary> {
    match value {
        "after_work" => Some(TurnInputCheckpointBoundary::AfterWork),
        "before_completion" => Some(TurnInputCheckpointBoundary::BeforeCompletion),
        _ => None,
    }
}

/// What an item carries. The wake payload stays an opaque copy of the process
/// delivery.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngressPayload {
    Input { input: Box<TurnInput> },
    ProcessWake { wake: Box<ProcessWakeDelivery> },
    SessionCommand { command: SessionCommand },
}

impl IngressPayload {
    /// The kind this payload fixes.
    #[must_use]
    pub const fn kind(&self) -> IngressKind {
        match self {
            Self::Input { .. } => IngressKind::Input,
            Self::ProcessWake { .. } => IngressKind::ProcessWake,
            Self::SessionCommand { .. } => IngressKind::SessionCommand,
        }
    }

    /// The process fact a wake carries: its process and event sequence.
    #[must_use]
    pub fn wake_source(&self) -> Option<(&ProcessId, u64)> {
        match self {
            Self::ProcessWake { wake } => Some((&wake.process_id, wake.sequence)),
            Self::Input { .. } | Self::SessionCommand { .. } => None,
        }
    }

    /// Whether this is an `ApplyConfigPatch` command: adjacent ones coalesce
    /// into one head commit (ADR 0101 §4).
    #[must_use]
    pub fn is_config_patch(&self) -> bool {
        matches!(
            self,
            Self::SessionCommand {
                command: SessionCommand::ApplyConfigPatch { .. }
            }
        )
    }
}

/// One admitted item's identity: kind-derived, unique across the store.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct IngressItemId(String);

impl IngressItemId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for IngressItemId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for IngressItemId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for IngressItemId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Family version of the ingress submission digest
/// ([`IngressItemDraft::submission_digest`]).
///
/// Stored digests are compared for equality against freshly computed ones, so
/// any change to the preimage grammar or to the serde form of a payload it
/// hashes must bump this version together with both SQL store schema
/// versions: a row admitted under the old grammar would otherwise refuse its
/// own identical retry as a conflict.
pub const SESSION_INGRESS_SUBMISSION_FAMILY_VERSION: u8 = 1;

/// The source-key prefix only the command kind may use.
pub const COMMAND_SOURCE_KEY_PREFIX: &str = "command:";
/// The source-key prefix only the wake kind may use, with
/// [`WAKE_SOURCE_KEY_SUFFIX`].
pub const WAKE_SOURCE_KEY_PREFIX: &str = "process:";
/// The source-key suffix only the wake kind may use, with
/// [`WAKE_SOURCE_KEY_PREFIX`].
pub const WAKE_SOURCE_KEY_SUFFIX: &str = ":wake";

/// A submission to the ingress, before admission.
///
/// Built only through the per-kind constructors, so the per-kind invariants
/// of ADR 0101 §1 hold by construction: a wake is `AnyBoundary`, carries its
/// process source key and the process's authority; a command is `NextTurn`
/// under its `command:` source key.
#[derive(Clone, Debug)]
pub struct IngressItemDraft {
    session_id: SessionId,
    item_id: Option<IngressItemId>,
    source_key: Option<String>,
    delivery: Delivery,
    payload: IngressPayload,
    authority: Option<QueuedWorkAuthority>,
    merge_key: Option<String>,
}

impl IngressItemDraft {
    /// Host input for `session_id`, delivered as `delivery`.
    #[must_use]
    pub fn input(session_id: impl Into<SessionId>, delivery: Delivery, input: TurnInput) -> Self {
        Self {
            session_id: session_id.into(),
            item_id: None,
            source_key: None,
            delivery,
            payload: IngressPayload::Input {
                input: Box::new(input),
            },
            authority: None,
            merge_key: None,
        }
    }

    /// One process wake, addressed to the session the delivery targets.
    #[must_use]
    pub fn process_wake(wake: ProcessWakeDelivery) -> Self {
        let source_key =
            crate::queued_work_vocabulary::process_wake_source_key(&wake.process_id, wake.sequence);
        Self {
            session_id: wake.target_session_id.clone(),
            item_id: None,
            source_key: Some(source_key),
            delivery: Delivery::AnyBoundary,
            authority: Some(wake.authority.clone()),
            merge_key: Some(crate::queued_work_vocabulary::PROCESS_WAKE_MERGE_KEY.to_string()),
            payload: IngressPayload::ProcessWake {
                wake: Box::new(wake),
            },
        }
    }

    /// One session command under the host's idempotency key.
    #[must_use]
    pub fn session_command(
        session_id: impl Into<SessionId>,
        command: SessionCommand,
        idempotency_key: impl AsRef<str>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            item_id: None,
            source_key: Some(command.source_key(idempotency_key)),
            delivery: Delivery::NextTurn,
            payload: IngressPayload::SessionCommand { command },
            authority: None,
            merge_key: None,
        }
    }

    /// Provision the input's item id, as a journaled acceptance does
    /// (FIG-3513): re-running the acceptance names the same row. Ignored for
    /// every other kind, whose id is derived from its source key.
    #[must_use]
    pub fn with_item_id(mut self, item_id: impl Into<IngressItemId>) -> Self {
        if self.payload.kind() == IngressKind::Input {
            self.item_id = Some(item_id.into());
        }
        self
    }

    /// Set a host-owned source key on an input. Ignored for every other kind,
    /// whose source key is fixed by its constructor.
    #[must_use]
    pub fn with_source_key(mut self, source_key: impl Into<String>) -> Self {
        if self.payload.kind() == IngressKind::Input {
            self.source_key = Some(source_key.into());
        }
        self
    }

    /// Per-item authority data for the drain policy and traces. Never a claim
    /// gate (ADR 0101 §5).
    #[must_use]
    pub fn with_authority(mut self, authority: QueuedWorkAuthority) -> Self {
        self.authority = Some(authority);
        self
    }

    /// Per-item merge key data for the drain policy and traces. Never a claim
    /// gate (ADR 0101 §5).
    #[must_use]
    pub fn with_merge_key(mut self, merge_key: impl Into<String>) -> Self {
        self.merge_key = Some(merge_key.into());
        self
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[must_use]
    pub fn kind(&self) -> IngressKind {
        self.payload.kind()
    }

    #[must_use]
    pub fn provisioned_item_id(&self) -> Option<&IngressItemId> {
        self.item_id.as_ref()
    }

    #[must_use]
    pub fn source_key(&self) -> Option<&str> {
        self.source_key.as_deref()
    }

    #[must_use]
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    #[must_use]
    pub fn payload(&self) -> &IngressPayload {
        &self.payload
    }

    #[must_use]
    pub fn authority(&self) -> Option<&QueuedWorkAuthority> {
        self.authority.as_ref()
    }

    #[must_use]
    pub fn merge_key(&self) -> Option<&str> {
        self.merge_key.as_deref()
    }

    /// The reserved source-key prefix this draft's kind may not use, if it
    /// uses one (ADR 0101 §8). Only the wake kind may use `process:…:wake`
    /// and only the command kind `command:…`; a host input using either is
    /// refused before it can meet a wake or a command.
    #[must_use]
    pub fn reserved_source_key_violation(&self) -> Option<&str> {
        let source_key = self.source_key.as_deref()?;
        let wake_shaped = source_key.starts_with(WAKE_SOURCE_KEY_PREFIX)
            && source_key.ends_with(WAKE_SOURCE_KEY_SUFFIX);
        let command_shaped = source_key.starts_with(COMMAND_SOURCE_KEY_PREFIX);
        let violates = match self.kind() {
            IngressKind::Input => wake_shaped || command_shaped,
            IngressKind::ProcessWake => !wake_shaped,
            IngressKind::SessionCommand => !command_shaped,
        };
        violates.then_some(source_key)
    }

    /// The item id admission writes: the provisioned one for an input that
    /// carries it, otherwise derived from the kind's facts. `now_epoch_ms` and
    /// `nonce` seed only an input without a provisioned id; the caller must
    /// supply a pair unique per `(session_id, source_key)`.
    #[must_use]
    pub fn admitted_item_id(&self, now_epoch_ms: u64, nonce: u64) -> IngressItemId {
        if let Some(item_id) = &self.item_id {
            return item_id.clone();
        }
        match &self.payload {
            IngressPayload::Input { .. } => {
                IngressItemId(crate::turn_input_vocabulary::derive_pending_turn_input_id(
                    &self.session_id,
                    self.source_key.as_deref(),
                    now_epoch_ms,
                    nonce,
                ))
            }
            IngressPayload::ProcessWake { wake } => IngressItemId(format!(
                "iw:{}",
                crate::stable_hash::blake3_hex(
                    "lash-session-ingress-wake/v1",
                    format!("{}:{}:{}", self.session_id, wake.process_id, wake.sequence).as_bytes(),
                )
            )),
            IngressPayload::SessionCommand { .. } => IngressItemId(format!(
                "ic:{}",
                crate::stable_hash::blake3_hex(
                    "lash-session-ingress-command/v1",
                    format!(
                        "{}:{}",
                        self.session_id,
                        self.source_key.as_deref().unwrap_or_default()
                    )
                    .as_bytes(),
                )
            )),
        }
    }

    /// The immutable submission digest, written once at admission beside the
    /// immutable delivery and compared, alone, by every replay (ADR 0101 §8).
    ///
    /// What it covers is fixed by kind:
    ///
    /// - input: the submitted delivery and the input's canonical JSON;
    /// - process wake: the process fact only — the process, the event
    ///   sequence and the delivery payload — and not the host-configured
    ///   delivery, so a redelivery under another boundary is the same wake;
    /// - session command: the command's canonical JSON.
    ///
    /// Excluded everywhere: the session id and source key (the row is found
    /// by them), the item id, the enqueue time and every lifecycle and claim
    /// field. The preimage is the `lash.session-ingress-submission` identity
    /// family at [`SESSION_INGRESS_SUBMISSION_FAMILY_VERSION`]; the rendered
    /// form is `session-ingress-submission:v<family>:blake3:<hex>`.
    pub fn submission_digest(&self) -> Result<String, serde_json::Error> {
        let mut identity = crate::stable_identity::IdentityEncoder::new(
            "lash.session-ingress-submission",
            SESSION_INGRESS_SUBMISSION_FAMILY_VERSION,
        );
        match &self.payload {
            IngressPayload::Input { input } => {
                identity.tag(1);
                encode_delivery(&mut identity, &self.delivery);
                identity.bytes(&crate::identity_json::payload_leaf(&serde_json::to_value(
                    input,
                )?));
            }
            IngressPayload::ProcessWake { wake } => {
                identity.tag(2);
                identity.string(wake.process_id.as_str());
                identity.u64(wake.sequence);
                identity.bytes(&crate::identity_json::payload_leaf(&serde_json::to_value(
                    wake,
                )?));
            }
            IngressPayload::SessionCommand { command } => {
                identity.tag(3);
                identity.bytes(&crate::identity_json::payload_leaf(&serde_json::to_value(
                    command,
                )?));
            }
        }
        Ok(crate::stable_identity::rendered_hash(
            "session-ingress-submission",
            SESSION_INGRESS_SUBMISSION_FAMILY_VERSION,
            &identity.finish(),
        ))
    }
}

/// Permanent tag registry for the delivery inside the submission preimage:
/// 1 `turn` (followed by the turn id and the boundary), 2 `any_boundary`,
/// 3 `next_turn`. Boundary: 1 `after_work`, 2 `before_completion`. Retired
/// tags remain burned.
fn encode_delivery(identity: &mut crate::stable_identity::IdentityEncoder, delivery: &Delivery) {
    match delivery {
        Delivery::Turn {
            turn_id,
            min_boundary,
        } => {
            identity.tag(1);
            identity.string(turn_id.as_str());
            identity.tag(match min_boundary {
                TurnInputCheckpointBoundary::AfterWork => 1,
                TurnInputCheckpointBoundary::BeforeCompletion => 2,
            });
        }
        Delivery::AnyBoundary => identity.tag(2),
        Delivery::NextTurn => identity.tag(3),
    }
}

/// An item's durable lifecycle state.
///
/// `accepted` is exactly "a claim holds the row": the claim columns are set
/// on an accepted row and on no other. `held` stays a read projection of an
/// accepted row whose claim pins the session's current drive epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressState {
    Open,
    Accepted,
    Completed,
    Cancelled,
}

impl IngressState {
    /// The persisted `state` column spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Decode a persisted `state` column; `None` for an unknown spelling.
    #[must_use]
    pub fn from_wire_str(value: &str) -> Option<Self> {
        [Self::Open, Self::Accepted, Self::Completed, Self::Cancelled]
            .into_iter()
            .find(|state| state.as_str() == value)
    }

    /// Whether the row is a tombstone.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// How a host withdrawal named the item it withdrew.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressWithdrawSelector {
    ItemId,
    SourceKey,
    Suffix,
}

/// Why an item left the ingress without being delivered. Closed (ADR 0101
/// §10): the free-form `TurnCancelRequest.reason` stays host evidence on the
/// request and is never this.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum IngressCancelReason {
    /// A turn cancel's disposition.
    TurnCancelled {
        request_id: String,
        mode: TurnCancelMode,
    },
    /// An explicit host withdrawal.
    HostWithdrawn { selector: IngressWithdrawSelector },
}

/// The closed cause every tombstone carries (ADR 0101 §8).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum IngressTerminalCause {
    /// An input or wake rendered by a committed turn.
    Delivered,
    /// A command applied at a turn boundary.
    Applied,
    /// An `ApplyConfigPatch` refused because the session's config revision had
    /// moved past the one it was written against (ADR 0101 §12).
    StaleConfigRevision { base: u64, head: u64 },
    /// Withdrawn or dropped without delivery.
    Cancelled { reason: IngressCancelReason },
}

impl IngressTerminalCause {
    /// The terminal state this cause puts a row in.
    #[must_use]
    pub const fn state(&self) -> IngressState {
        match self {
            Self::Delivered | Self::Applied | Self::StaleConfigRevision { .. } => {
                IngressState::Completed
            }
            Self::Cancelled { .. } => IngressState::Cancelled,
        }
    }
}

/// One admitted item as stored, open or tombstoned.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IngressItem {
    pub item_id: IngressItemId,
    pub session_id: SessionId,
    /// One per-session order shared by every kind, taken under the session
    /// lock: enqueue order equals per-session commit order.
    pub enqueue_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub delivery: Delivery,
    pub submission_digest: String,
    pub payload: IngressPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<QueuedWorkAuthority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
    pub state: IngressState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_cause: Option<IngressTerminalCause>,
    /// Informational, and for claim-size bounds. Never an order key.
    pub enqueued_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at_ms: Option<u64>,
}

impl IngressItem {
    #[must_use]
    pub fn kind(&self) -> IngressKind {
        self.payload.kind()
    }

    #[must_use]
    pub fn lane(&self) -> IngressLane {
        self.kind().lane()
    }

    /// Reject a decoded row whose lifecycle columns disagree: a terminal row
    /// without its cause and instant, a cause on a live row, or a cause whose
    /// state is not the row's.
    pub fn validate_lifecycle(&self) -> Result<(), String> {
        match (
            self.state.is_terminal(),
            &self.terminal_cause,
            self.terminal_at_ms,
        ) {
            (true, Some(cause), Some(_)) if cause.state() == self.state => Ok(()),
            (false, None, None) => Ok(()),
            _ => Err(format!(
                "ingress item `{}` has state `{}` with terminal cause {:?} at {:?}",
                self.item_id,
                self.state.as_str(),
                self.terminal_cause,
                self.terminal_at_ms
            )),
        }
    }
}

/// What admission answered (ADR 0101 §1).
#[derive(Clone, Debug)]
pub enum IngressEnqueueOutcome {
    /// A new row was admitted.
    Inserted(IngressItem),
    /// An identical submission already has a row or a tombstone. A
    /// `cancelled` tombstone is never reopened.
    Existing(IngressItem),
    /// The source key (or provisioned id) names a row with a different
    /// submission. Nothing was written, except that a wake's conflict is its
    /// terminal discard and raised the redelivery floor.
    Conflict { existing_item_id: IngressItemId },
    /// A wake at or below the session's redelivery floor with no row left to
    /// match: already consumed, and absorbed.
    WakeRewound {
        process_id: ProcessId,
        sequence: u64,
        floor: u64,
    },
}

impl IngressEnqueueOutcome {
    /// The admitted or matched row, when there is one.
    #[must_use]
    pub fn item(&self) -> Option<&IngressItem> {
        match self {
            Self::Inserted(item) | Self::Existing(item) => Some(item),
            Self::Conflict { .. } | Self::WakeRewound { .. } => None,
        }
    }
}

/// An open item as a list read reports it.
#[derive(Clone, Debug)]
pub struct IngressItemRead {
    pub item: IngressItem,
    pub status: IngressReadStatus,
}

/// Whether a live claim holds an open item right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressReadStatus {
    /// Claimable: unclaimed, or pinned to a drive epoch a later admission
    /// superseded.
    Pending,
    /// A claim pinning the session's current drive epoch holds it. Nothing
    /// expires it: only a later admission's seal does.
    Held { drive_epoch: u64 },
}

/// One host-withdrawal target.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum IngressWithdrawTarget {
    ItemId(IngressItemId),
    SourceKey(String),
}

impl IngressWithdrawTarget {
    #[must_use]
    pub const fn selector(&self) -> IngressWithdrawSelector {
        match self {
            Self::ItemId(_) => IngressWithdrawSelector::ItemId,
            Self::SourceKey(_) => IngressWithdrawSelector::SourceKey,
        }
    }
}

/// The one cancel outcome every kind shares (ADR 0101 §10).
#[derive(Clone, Debug)]
pub enum IngressWithdrawOutcome {
    /// This call withdrew the item: its `cancelled` tombstone and its record.
    Withdrawn(IngressAffectedItem),
    /// A live claim holds the item; nothing changed.
    Held(IngressItem),
    AlreadyCompleted(IngressItem),
    AlreadyCancelled(IngressItem),
    NotFound,
}

impl IngressWithdrawOutcome {
    #[must_use]
    pub fn is_withdrawn(&self) -> bool {
        matches!(self, Self::Withdrawn(_))
    }
}

/// One target's answer in a batch withdrawal.
#[derive(Clone, Debug)]
pub struct IngressWithdrawReceipt {
    pub target: IngressWithdrawTarget,
    pub outcome: IngressWithdrawOutcome,
}

/// A suffix withdrawal's answer: every item at or after the anchor in its
/// lane, each with its own outcome, in `enqueue_seq` order.
#[derive(Clone, Debug)]
pub enum IngressSuffixWithdrawOutcome {
    AnchorNotFound {
        anchor: IngressWithdrawTarget,
    },
    Outcomes {
        anchor: IngressWithdrawTarget,
        outcomes: Vec<IngressWithdrawOutcome>,
    },
}

/// How an item left a claim without being delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressUndeliveredDisposition {
    /// Released at its own position, to be claimed again.
    Defer,
    /// Tombstoned `cancelled`.
    Drop,
}

impl From<TurnCancelDisposition> for IngressUndeliveredDisposition {
    fn from(value: TurnCancelDisposition) -> Self {
        match value {
            TurnCancelDisposition::Defer => Self::Defer,
            TurnCancelDisposition::Drop => Self::Drop,
        }
    }
}

/// The record of one item a cancel or withdrawal affected (ADR 0101 §10).
///
/// Every affected item is recorded, deferred or dropped, and a drop cannot be
/// planned without its record. `fence_floor_after` is the session's wake
/// redelivery floor for the wake's process once a dropped wake raised it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IngressAffectedItem {
    pub item_id: IngressItemId,
    pub kind: IngressKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub enqueue_seq: u64,
    pub disposition: IngressUndeliveredDisposition,
    pub reason: IngressCancelReason,
    pub payload: IngressPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence_floor_after: Option<u64>,
}

/// The claim mode: where a claim delivers what it takes (ADR 0101 §1, §5).
///
/// An ingress drive is always a claimed drive; there is no unclaimed mode.
/// Host-selected out-of-order selection does not exist (FIG-3600): selecting
/// items survives only as withdrawal or cancel.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClaimMode {
    /// A turn boundary with no turn running: the command lane, or the start
    /// of the next turn.
    Idle,
    /// A checkpoint of the running turn.
    Checkpoint {
        turn_id: TurnId,
        checkpoint: CheckpointKind,
    },
}

impl ClaimMode {
    /// The turn a checkpoint claim delivers into.
    #[must_use]
    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::Idle => None,
            Self::Checkpoint { turn_id, .. } => Some(turn_id),
        }
    }
}

/// The one ingress claim (ADR 0101 §1, amended): keyed by the drive epoch and
/// the admission that took it, with a clock-free claim token. It carries no
/// lease fields: under ADR 0104/0105 the drive fence, not an SQL
/// session-execution lease, is the claim authority.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IngressClaim {
    pub session_id: SessionId,
    pub claim_id: String,
    /// Derived from the session, the admission and the claim id alone, so a
    /// replay of the claim step re-derives the same token.
    pub claim_token: String,
    /// The claim's own fencing token: the head row's successor token.
    pub fencing_token: u64,
    /// The drive epoch the claim pins. The claim is live exactly while the
    /// session's drive epoch is this value.
    pub drive_epoch: u64,
    /// The admission whose fence took the claim.
    pub admission: crate::store::AdmissionId,
    pub mode: ClaimMode,
    /// Every claimed item, in `enqueue_seq` order.
    pub items: Vec<IngressItem>,
    /// The interrupted claim this one re-derives, when it is the redrive of a
    /// claim whose drive epoch was superseded (ADR 0101 §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<IngressClaimIdentity>,
}

/// What a resumed run's reclaim of its own claim answered (FIG-3552).
///
/// Ownership of a claimed row moves only through the claim CAS: a resumed
/// run takes every row it still owns under its new drive epoch, and a
/// run a peer superseded — any of its rows re-claimed, settled or withdrawn
/// since — cedes and writes nothing. There is never a drop-and-retry.
#[derive(Clone, Debug)]
pub enum IngressReclaimOutcome {
    /// Every row the claim held now carries this claim, pinned to the
    /// reclaiming drive epoch. When the claim already pinned it, this is the
    /// claim unchanged.
    Reclaimed(Box<IngressClaim>),
    /// A peer superseded the claim; nothing was written.
    Ceded,
}

/// A claim's identity as it is persisted on each row it holds.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct IngressClaimIdentity {
    pub claim_id: String,
    pub claim_token: String,
}

impl IngressClaim {
    /// This claim's persisted identity.
    #[must_use]
    pub fn identity(&self) -> IngressClaimIdentity {
        IngressClaimIdentity {
            claim_id: self.claim_id.clone(),
            claim_token: self.claim_token.clone(),
        }
    }

    /// The claimed item ids, in `enqueue_seq` order.
    #[must_use]
    pub fn item_ids(&self) -> Vec<IngressItemId> {
        self.items.iter().map(|item| item.item_id.clone()).collect()
    }

    /// The fixed render order within one claim (ADR 0101 §6): host inputs
    /// first, then wake causes, each in `enqueue_seq` order. Commands are
    /// never rendered.
    #[must_use]
    pub fn render_order(&self) -> Vec<&IngressItem> {
        let mut rendered = self
            .items
            .iter()
            .filter(|item| item.kind() != IngressKind::SessionCommand)
            .collect::<Vec<_>>();
        rendered.sort_by_key(|item| (item.kind() != IngressKind::Input, item.enqueue_seq));
        rendered
    }
}
