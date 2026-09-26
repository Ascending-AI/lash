//! Durable turn-input vocabulary.
//!
//! The pending-input rows a store persists, their claim and completion
//! payloads, and the checkpoint boundary rule stores filter on. The ingress
//! driver that normalizes and applies them stays in `lash-core`.

use crate::{
    CheckpointKind, PluginMessage, RuntimeError, RuntimeErrorCode, SessionId, TurnCause, TurnId,
};
use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// The input id a journaled turn acceptance provisions before its body runs
/// (ADR 0069 §6).
///
/// It is a function of the acceptance effect's address alone, so every
/// execution of the same acceptance, including one re-run because the first
/// one's outcome was never recorded, names the same row, and the store adopts
/// that row instead of admitting a second one. The address is session-scoped
/// and collision-free ([`crate::EffectAddress::graph_key`]), which keeps the id
/// unique across sessions. Format: `ti:<blake3-hex>`, the shape every other
/// pending turn-input id has.
#[must_use]
pub fn provisioned_turn_input_id(acceptance: &crate::EffectAddress) -> String {
    format!(
        "ti:{}",
        crate::stable_hash::blake3_hex(
            "lash-accepted-turn-input/v1",
            acceptance.graph_key().as_bytes(),
        )
    )
}

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
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
/// The `active_turn` admission scope's payload — carried by every
/// [`TurnInputState`] variant the persisted CHECKs pin to that scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ActiveTurnIngress {
    pub turn_id: crate::TurnId,
    #[serde(default)]
    pub min_boundary: TurnInputCheckpointBoundary,
}
impl From<ActiveTurnIngress> for TurnInputIngress {
    fn from(scope: ActiveTurnIngress) -> Self {
        Self::ActiveTurn {
            turn_id: scope.turn_id,
            min_boundary: scope.min_boundary,
        }
    }
}

/// The scope-free name of a persisted turn-input state.
///
/// This is the `state` column's vocabulary for SQL predicates and decoders;
/// the value type [`TurnInputState`] carries the scope each name is pinned
/// to, so the two can never disagree in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnInputStateKind {
    PendingActive,
    DeferredNextTurn,
    Accepted,
    Cancelled,
    Completed,
}
impl TurnInputStateKind {
    /// Returns whether this state name is settled and eligible for tombstone vacuum.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed)
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::PendingActive | Self::DeferredNextTurn)
    }

    pub fn is_next_turn_pending(self) -> bool {
        matches!(self, Self::DeferredNextTurn)
    }
}

/// A pending input's durable lifecycle state, carrying the admission scope
/// each variant is pinned to.
///
/// The scope lives inside the state so a value cannot disagree with the
/// persisted `ingress_json`/`state` column pair: `pending_active` and
/// `accepted` always carry `active_turn` scope, `deferred_next_turn` is
/// always `next_turn` scope, and the terminal variants keep whichever scope
/// the row was admitted under. The backend `CHECK` constraints remain as a
/// belt-and-braces check on a value the type can no longer contradict.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputState {
    PendingActive(ActiveTurnIngress),
    DeferredNextTurn,
    Accepted(ActiveTurnIngress),
    Cancelled(TurnInputIngress),
    Completed(TurnInputIngress),
}
impl TurnInputState {
    /// The only legal initial durable state for an input admitted under `ingress`.
    #[must_use]
    pub fn open(ingress: TurnInputIngress) -> Self {
        match ingress {
            TurnInputIngress::ActiveTurn {
                turn_id,
                min_boundary,
            } => Self::PendingActive(ActiveTurnIngress {
                turn_id,
                min_boundary,
            }),
            TurnInputIngress::NextTurn => Self::DeferredNextTurn,
        }
    }

    /// Returns `None` for an unknown spelling or a state/scope pair the
    /// persisted CHECKs forbid (`pending_active` under `next_turn` scope,
    /// `deferred_next_turn` under `active_turn` scope, or `accepted` under
    /// `next_turn` scope) — the same disagreement the type refuses to
    /// construct.
    pub fn from_persisted(state: &str, ingress: TurnInputIngress) -> Option<Self> {
        match (TurnInputStateKind::from_wire_str(state)?, ingress) {
            (
                TurnInputStateKind::PendingActive,
                TurnInputIngress::ActiveTurn {
                    turn_id,
                    min_boundary,
                },
            ) => Some(Self::PendingActive(ActiveTurnIngress {
                turn_id,
                min_boundary,
            })),
            (
                TurnInputStateKind::Accepted,
                TurnInputIngress::ActiveTurn {
                    turn_id,
                    min_boundary,
                },
            ) => Some(Self::Accepted(ActiveTurnIngress {
                turn_id,
                min_boundary,
            })),
            (TurnInputStateKind::DeferredNextTurn, TurnInputIngress::NextTurn) => {
                Some(Self::DeferredNextTurn)
            }
            (TurnInputStateKind::Cancelled, ingress) => Some(Self::Cancelled(ingress)),
            (TurnInputStateKind::Completed, ingress) => Some(Self::Completed(ingress)),
            _ => None,
        }
    }

    /// The scope-free name this state persists under — the `state` column's spelling.
    pub fn kind(&self) -> TurnInputStateKind {
        match self {
            Self::PendingActive(_) => TurnInputStateKind::PendingActive,
            Self::DeferredNextTurn => TurnInputStateKind::DeferredNextTurn,
            Self::Accepted(_) => TurnInputStateKind::Accepted,
            Self::Cancelled(_) => TurnInputStateKind::Cancelled,
            Self::Completed(_) => TurnInputStateKind::Completed,
        }
    }

    /// The stable wire spelling this state persists under.
    pub fn as_str(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The admission scope this state carries — the `ingress_json` column's value.
    pub fn ingress(&self) -> TurnInputIngress {
        match self {
            Self::PendingActive(scope) | Self::Accepted(scope) => scope.clone().into(),
            Self::DeferredNextTurn => TurnInputIngress::NextTurn,
            Self::Cancelled(ingress) | Self::Completed(ingress) => ingress.clone(),
        }
    }

    /// The turn id this input is scoped to, when its scope is `active_turn`.
    pub fn active_turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::PendingActive(scope) | Self::Accepted(scope) => Some(&scope.turn_id),
            Self::Cancelled(ingress) | Self::Completed(ingress) => ingress.active_turn_id(),
            Self::DeferredNextTurn => None,
        }
    }

    /// Rebinds an `active_turn`-scoped open state to `accepted`, carrying its
    /// scope forward. Returns `None` for `next_turn`-scoped or terminal
    /// states — the pairs the persisted CHECKs refuse.
    pub fn accepted(&self) -> Option<Self> {
        match self {
            Self::PendingActive(scope) | Self::Accepted(scope) => {
                Some(Self::Accepted(scope.clone()))
            }
            _ => None,
        }
    }

    /// Lets store, effect-host, and protocol implementors test whether this `TurnInputState` is
    /// next turn pending while materializing, executing, or persisting a session turn.
    pub fn is_next_turn_pending(&self) -> bool {
        self.kind().is_next_turn_pending()
    }

    /// Returns whether this state is settled and eligible for tombstone vacuum.
    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }

    pub fn is_open(&self) -> bool {
        self.kind().is_open()
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

    /// The canonical digest of this submission: the immutable value source-key
    /// replay compares.
    ///
    /// Stores write it once, beside the submitted ingress, when they admit the
    /// row, and never rewrite it. A retry under the same source key is the same
    /// submission exactly when its digest equals the stored one; the row's
    /// *current* ingress and state (which a Defer rewrites to `next_turn` when
    /// the named turn ends) take no part in the verdict (ADR 0010).
    ///
    /// The digest covers the submission as the host made it and nothing the
    /// store generates or mutates:
    ///
    /// - the ingress as submitted — scope, and for `active_turn` the turn id
    ///   and minimum checkpoint boundary;
    /// - the input's canonical JSON (the persisted `TurnInput` serde form with
    ///   object keys sorted and `-0.0` folded to `0.0`), as one opaque leaf.
    ///
    /// Excluded: the session id and source key (the row is found by them), the
    /// generated input id, the enqueue time, and every lifecycle and claim
    /// field. The preimage is the `lash.turn-input-submission` identity family
    /// at [`TURN_INPUT_SUBMISSION_FAMILY_VERSION`]; the rendered form is
    /// `turn-input-submission:v<family>:blake3:<hex>`.
    pub fn submission_digest(&self) -> Result<String, serde_json::Error> {
        let preimage = turn_input_submission_preimage(&self.ingress, &self.input)?;
        Ok(crate::stable_identity::rendered_hash(
            "turn-input-submission",
            TURN_INPUT_SUBMISSION_FAMILY_VERSION,
            &preimage,
        ))
    }

    /// Adopt the row a provisioned `input_id` already names, for turn-input
    /// store implementors enqueueing a draft that carries its own id.
    ///
    /// A provisioned id names one admission (ADR 0069 §6): a submission whose
    /// digest equals the stored row's immutable
    /// [`submission_digest`](Self::submission_digest), in the same session, is
    /// the same admission re-run and returns the existing row, whatever its
    /// lifecycle state; a different digest, or the same id in another session,
    /// is a typed
    /// [`StoreError::PendingTurnInputIdConflict`](crate::store::StoreError::PendingTurnInputIdConflict).
    pub fn adopt_provisioned_row(
        &self,
        existing: PendingTurnInput,
        existing_submission_digest: &str,
    ) -> Result<PendingTurnInput, crate::store::StoreError> {
        let digest = self.submission_digest().map_err(|err| {
            crate::store::StoreError::Backend(format!(
                "failed to digest pending turn input submission: {err}"
            ))
        })?;
        if existing.session_id != self.session_id || digest != existing_submission_digest {
            return Err(crate::store::StoreError::PendingTurnInputIdConflict {
                session_id: self.session_id.clone(),
                input_id: existing.input_id,
            });
        }
        Ok(existing)
    }
}

/// Family version of the turn-input submission digest
/// ([`PendingTurnInputDraft::submission_digest`]).
///
/// Stored digests are compared for equality against freshly computed ones, so
/// any change to the preimage grammar or to the `TurnInput` serde form it
/// hashes must bump this version together with both SQL store schema versions:
/// a row admitted under the old grammar would otherwise refuse its own
/// identical retry as a conflict.
pub const TURN_INPUT_SUBMISSION_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for the turn-input submission preimage.
///
/// Ingress scope: 1 `active_turn` (followed by the turn id and the boundary),
/// 2 `next_turn`. Boundary: 1 `after_work`, 2 `before_completion`. The input
/// follows as one canonical JSON payload leaf. Retired tags remain burned.
fn turn_input_submission_preimage(
    ingress: &TurnInputIngress,
    input: &TurnInput,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.turn-input-submission",
        TURN_INPUT_SUBMISSION_FAMILY_VERSION,
    );
    match ingress {
        TurnInputIngress::ActiveTurn {
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
        TurnInputIngress::NextTurn => identity.tag(2),
    }
    identity.bytes(&crate::identity_json::payload_leaf(&serde_json::to_value(
        input,
    )?));
    Ok(identity.finish())
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingTurnInput {
    pub input_id: crate::InputId,
    pub session_id: SessionId,
    pub enqueue_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// The admission scope lives inside `state`: every open or accepted
    /// variant carries its scope, so the persisted `ingress_json`/`state`
    /// column pair cannot disagree.
    pub state: TurnInputState,
    pub enqueued_at_ms: u64,
    pub input: TurnInput,
}

/// Host-facing projection of one open pending turn-input record.
///
/// This projection is separate from [`PendingTurnInput`] because a row's
/// durable lifecycle state and its read-time claim status answer different
/// questions. A live matching session-execution-lease generation makes the
/// row held; an expired, released, or mismatched generation leaves it pending
/// for successor reclaim under ADR 0029, unless an aborted direct turn's
/// binding reserves it for that turn (FIG-3589).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct PendingTurnInputRead {
    /// The durable admission record.
    pub input: PendingTurnInput,
    /// Its factual status at the read's store-clock instant.
    pub status: PendingTurnInputReadStatus,
}

impl PendingTurnInputRead {
    /// Project an open row that no live matching claim currently holds.
    pub fn pending(input: PendingTurnInput) -> Self {
        Self {
            input,
            status: PendingTurnInputReadStatus::Pending,
        }
    }

    /// Project an open row held by the currently live matching lease generation.
    pub fn held(input: PendingTurnInput, lease_expires_at_ms: u64) -> Self {
        Self {
            input,
            status: PendingTurnInputReadStatus::Held {
                lease_expires_at_ms,
            },
        }
    }

    /// Project an open row whose claim is bound to the aborted direct turn
    /// `turn_id`, whose acceptance receipt names `receipt_input_id` (FIG-3589).
    pub fn turn_bound(
        input: PendingTurnInput,
        turn_id: crate::TurnId,
        receipt_input_id: crate::InputId,
    ) -> Self {
        Self {
            input,
            status: PendingTurnInputReadStatus::TurnBound {
                turn_id,
                receipt_input_id,
            },
        }
    }
}

/// `Held` reports only durable lease facts. It does not assert that the holder
/// process is alive, and lease expiry does not itself supersede the holder's
/// completion authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingTurnInputReadStatus {
    /// No currently live matching lease generation holds the row.
    Pending,
    /// The row's claim matches the currently live session lease generation.
    Held {
        /// Exact expiry stored on that matching session-execution lease.
        lease_expires_at_ms: u64,
    },
    /// The row belongs to a direct turn that aborted with `Err` while it held
    /// the row's claim (FIG-3589, ADR 0069 §7).
    ///
    /// No drain and no other turn claims it, whatever lease generation holds
    /// the session. Only a redrive of `turn_id`, which replays that turn's
    /// journal and settles the row with its claim, or a cancel of the input
    /// consumes it. The aborted turn's `Err` carries the acceptance receipt
    /// that names both.
    TurnBound {
        /// The aborted turn the row is bound to.
        turn_id: crate::TurnId,
        /// The input the aborted turn's acceptance receipt names: the one row
        /// of its drive a cancel may target.
        receipt_input_id: crate::InputId,
    },
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
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
            ingress: input.ingress(),
        }
    }
}
impl PendingTurnInput {
    /// The row's admission scope — derived from [`Self::state`], which carries
    /// it, so the persisted `ingress_json`/`state` pair cannot disagree.
    pub fn ingress(&self) -> TurnInputIngress {
        self.state.ingress()
    }

    /// Exposes accepted input to store and durable-substrate implementors while claiming and
    /// settling durable turn inputs.
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
    /// Refused: the row belongs to the drive of the aborted direct turn
    /// `turn_id`, and it is not the input that turn's acceptance receipt names
    /// (FIG-3589). Cancelling it alone would change the drive set that turn's
    /// journal replays. Cancel `receipt_input_id` instead, which settles the
    /// receipt's input and returns this row to the queue.
    TurnBound {
        input: PendingTurnInput,
        turn_id: crate::TurnId,
        receipt_input_id: crate::InputId,
    },
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
            | Self::AlreadyCancelled(input)
            | Self::TurnBound { input, .. } => Some(input),
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

pub(crate) fn source_key_display_id(source: &str) -> String {
    source
        .strip_prefix("host:")
        .or_else(|| source.strip_prefix("injection:"))
        .unwrap_or(source)
        .to_string()
}

/// The initial drive set of an accepted turn input, as the
/// `ClaimAcceptedTurnInput` runtime effect journals it (ADR 0069 §6).
///
/// It is a self-contained authority snapshot: a claimed drive carries the rows
/// with their content and claim token, and a refusal names why the root cedes.
/// A drive admits the head of the queue, so the claim always reaches its row
/// or refuses it (FIG-3600).
/// Replay returns this value and never reconstructs it from pending rows, so
/// `vacuum()` pruning terminal rows cannot change what a replayed turn does.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "drive", rename_all = "snake_case")]
pub enum AcceptedTurnInputDrive {
    /// The claim reached the accepted row: drive every claimed row and settle
    /// them under the claim predicate.
    ///
    /// This is the turn's admission, so it also records what the turn was
    /// admitted on: the session head (`base`) and the turn index. A replay
    /// rebuilds the turn's input state from `base` and addresses its effects
    /// under `turn_index`, never re-reading either from the live head, which
    /// the turn's own commit may already have advanced (FIG-3682). It also
    /// records the executable generation the turn runs under (FIG-3571). The
    /// session drive runs this claim as its root's recorded claim step.
    Claimed {
        claim: Box<TurnInputClaim>,
        base: crate::store::SessionHeadRef,
        turn_index: u64,
        /// The executable generation the turn was admitted under (FIG-3571):
        /// a redrive under another one is refused before any effect.
        generation: Option<crate::executable_generation::ExecutableGeneration>,
    },
    /// The accepted row cannot be driven by this turn.
    Refused { refusal: AcceptedTurnInputRefusal },
}

/// Why an accepted turn input's first execution ceded instead of driving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedTurnInputRefusal {
    /// The accepted row is held by a claim of the live lease generation that
    /// is not this drive's. The drive's turn holds that lease, so the holder is
    /// another claim of the same lane: an earlier run of this drive body, or a
    /// claim whose hand-back failed. The row stays claim-pinned until that
    /// claim settles or the generation turns over, and is then drained.
    HeldByLiveClaim,
    /// The accepted row is no longer open: another driver settled it, the host
    /// cancelled it, or `vacuum()` pruned it after either.
    SettledOrRemoved,
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
pub fn materialize_turn_input(inputs: &[PendingTurnInput]) -> TurnInput {
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
#[derive(Clone, Debug, Default)]
pub struct QueuedCheckpointTurnInput {
    pub messages: Vec<crate::Message>,
    pub turn_causes: Vec<TurnCause>,
}
pub(crate) fn plugin_message_from_turn_input(input: &TurnInput) -> Option<PluginMessage> {
    let parts: Vec<_> = input
        .items
        .iter()
        .map(|item| match item {
            crate::InputItem::Text { text } => crate::Part::text(String::new(), text.clone(), None),
            crate::InputItem::Attachment { source } => crate::Part::attachment_part(
                String::new(),
                String::new(),
                Some(lash_sansio::PartAttachment {
                    source: source.clone(),
                }),
            ),
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(PluginMessage {
        id: None,
        role: crate::MessageRole::User,
        origin: None,
        parts,
    })
}

async fn committed_message_from_pending_input(
    pending: &PendingTurnInput,
    turn_id: &crate::TurnId,
    attachment_store: &crate::SessionAttachmentStore,
    attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<Option<crate::Message>, String> {
    let normalized = crate::input_normalization::normalize_input_items(
        &pending.input.items,
        attachment_store,
        attachment_source_policy,
    )
    .await?;
    let message_id = ingress_message_id(&pending.input_id);
    let mut parts = Vec::new();
    for item in normalized {
        match item {
            crate::NormalizedItem::Text(text) if !text.is_empty() => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part::text(part_id, text, None));
            }
            crate::NormalizedItem::Text(_) => {}
            crate::NormalizedItem::Attachment(source) => {
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
            turn_id: turn_id.clone(),
            input_id: Some(pending.input_id.clone()),
        }),
        parts: crate::shared_parts(parts),
    }))
}
pub fn ingress_message_id(input_id: &str) -> String {
    format!("m_ingress_{input_id}")
}

/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Text { text: String },
    Attachment { source: crate::AttachmentSource },
}
impl InputItem {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn attachment(source: crate::AttachmentSource) -> Self {
        Self::Attachment { source }
    }
}
/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TurnInput {
    pub items: Vec<InputItem>,
    /// Per-turn override for protocol-owned turn options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    /// Internal protocol transport carrier for the facade builder's turn ID.
    ///
    /// All non-advanced facade paths overwrite this field.
    /// Only low-level protocol transport should read this field directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_turn_id: Option<TurnId>,
    #[serde(skip)]
    pub protocol_extension: Option<ProtocolTurnExtensionHandle>,
    #[serde(skip)]
    pub turn_context: TurnContext,
}
impl TurnInput {
    pub fn empty() -> Self {
        Self::items(std::iter::empty())
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::items([InputItem::text(text)])
    }

    /// Collects mixed input items in caller order for protocol implementors materializing a turn.
    pub fn items(items: impl IntoIterator<Item = InputItem>) -> Self {
        Self {
            items: items.into_iter().collect(),
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: TurnContext::default(),
        }
    }

    pub fn with_attachment(mut self, source: crate::AttachmentSource) -> Self {
        self.items.push(InputItem::attachment(source));
        self
    }

    pub fn with_protocol_turn_options(mut self, options: crate::ProtocolTurnOptions) -> Self {
        self.protocol_turn_options = Some(options);
        self
    }
}
/// Per-turn, in-process side channel of typed plugin inputs.
///
/// This is an `Any`-keyed map of live Rust values handed to plugins for a
/// single turn. It is deliberately **not** serializable: the values never
/// survive a process boundary, so durable effect-host runs explicitly reject a
/// turn that carries any live inputs with
/// [`RuntimeErrorCode::DurableEffectLivePluginInput`]. Durable callers must
/// instead encode replayable data in `protocol_turn_options` or persisted
/// plugin state.
#[derive(Clone, Default)]
pub struct LiveTurnInputs {
    inputs: HashMap<&'static str, Arc<dyn Any + Send + Sync>>,
}
impl LiveTurnInputs {
    fn insert<T>(&mut self, plugin_id: &'static str, input: T)
    where
        T: Send + Sync + 'static,
    {
        self.inputs.insert(plugin_id, Arc::new(input));
    }

    fn get<T>(&self, plugin_id: &'static str) -> Option<&T>
    where
        T: 'static,
    {
        self.inputs
            .get(plugin_id)
            .and_then(|input| input.downcast_ref::<T>())
    }

    fn contains(&self, plugin_id: &'static str) -> bool {
        self.inputs.contains_key(plugin_id)
    }

    pub fn plugin_ids(&self) -> Vec<&'static str> {
        self.inputs.keys().copied().collect()
    }

    /// Returns an error when live per-turn inputs would make a durable effect
    /// host replay depend on process-local values.
    pub fn durable_effect_rejection(&self) -> Result<(), RuntimeError> {
        if self.inputs.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::new(
            RuntimeErrorCode::DurableEffectLivePluginInput,
            "durable effect hosts do not support live TurnContext plugin inputs; encode replayable data in protocol_turn_options or persisted plugin state",
        ))
    }
}
/// How a running turn treats durable queued work.
///
/// Written as one fact: an automatic drain may keep claiming checkpoint
/// batches as it runs, while a Selected Queued-Work Drain is closed over the
/// host-pinned batch-id set — checkpoint pull-in is forbidden and the pinned
/// composition's cost bound is enforced. The remaining flag combinations are
/// unrepresentable.
#[derive(Clone, Copy)]
enum QueuedWorkDrainMode {
    Automatic,
    Selected,
}
#[derive(Clone)]
pub struct TurnContext {
    plugin_inputs: LiveTurnInputs,
    prompt: crate::PromptLayer,
    runtime_correlation: Option<Arc<dyn Any + Send + Sync>>,
    queued_work_drain: QueuedWorkDrainMode,
}
impl Default for TurnContext {
    fn default() -> Self {
        Self {
            plugin_inputs: LiveTurnInputs::default(),
            prompt: crate::PromptLayer::default(),
            runtime_correlation: None,
            queued_work_drain: QueuedWorkDrainMode::Automatic,
        }
    }
}
impl TurnContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_plugin_input<T>(&mut self, plugin_id: &'static str, input: T)
    where
        T: Send + Sync + 'static,
    {
        self.plugin_inputs.insert(plugin_id, input);
    }

    pub fn mark_selected_queued_work_drain(&mut self) {
        self.queued_work_drain = QueuedWorkDrainMode::Selected;
    }

    pub fn enforces_selected_queued_work_cost_bound(&self) -> bool {
        matches!(self.queued_work_drain, QueuedWorkDrainMode::Selected)
    }

    pub fn checkpoint_queued_work_limit(&self, default_limit: usize) -> usize {
        match self.queued_work_drain {
            QueuedWorkDrainMode::Automatic => default_limit,
            QueuedWorkDrainMode::Selected => 0,
        }
    }

    pub fn plugin_input<T>(&self, plugin_id: &'static str) -> Option<&T>
    where
        T: 'static,
    {
        self.plugin_inputs.get(plugin_id)
    }

    /// Lets protocol implementors detect type-erased live plugin inputs that cannot cross a durable
    /// serialization boundary.
    pub fn has_live_plugin_inputs(&self) -> bool {
        !self.plugin_inputs.inputs.is_empty()
    }

    /// Lists only type-erased live plugin inputs for protocol implementors that must reject
    /// non-persistable turn extensions before a durable boundary.
    pub fn live_plugin_input_ids(&self) -> Vec<&'static str> {
        self.plugin_inputs.plugin_ids()
    }

    /// Live plugin inputs for this turn. The durable boundary inspects this to
    /// reject turns carrying non-serializable live state.
    pub fn live_plugin_inputs(&self) -> &LiveTurnInputs {
        &self.plugin_inputs
    }

    pub fn set_prompt_layer(&mut self, prompt: crate::PromptLayer) {
        self.prompt = prompt;
    }

    pub fn prompt_layer(&self) -> &crate::PromptLayer {
        &self.prompt
    }

    /// Installs one live runtime-owned correlation value.
    ///
    /// The value's private concrete type is its authority boundary: callers
    /// cannot fabricate or read a correlation type owned by another crate.
    #[doc(hidden)]
    pub fn set_runtime_correlation<T>(&mut self, value: T)
    where
        T: Any + Send + Sync,
    {
        self.runtime_correlation = Some(Arc::new(value));
    }

    #[doc(hidden)]
    pub fn runtime_correlation<T: Any>(&self) -> Option<&T> {
        self.runtime_correlation.as_ref()?.downcast_ref()
    }

    #[doc(hidden)]
    pub fn clear_runtime_correlation<T: Any>(&mut self) {
        if self
            .runtime_correlation
            .as_ref()
            .is_some_and(|value| value.is::<T>())
        {
            self.runtime_correlation = None;
        }
    }
}
impl facade_ops::TurnContextFacadeOps for TurnContext {
    fn has_plugin_input(&self, plugin_id: &'static str) -> bool {
        self.plugin_inputs.contains(plugin_id)
    }

    fn set_prompt_template(&mut self, template: crate::PromptTemplate) {
        self.prompt.template = Some(template);
    }

    fn add_prompt_contribution(&mut self, contribution: crate::PromptContribution) {
        self.prompt.add_contribution(contribution);
    }

    fn replace_prompt_slot(
        &mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) {
        self.prompt.replace_slot(slot, contributions);
    }

    fn clear_prompt_slot(&mut self, slot: crate::PromptSlot) {
        self.prompt.clear_slot(slot);
    }
}
impl fmt::Debug for TurnContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnContext")
            .field("plugin_inputs", &self.plugin_inputs.plugin_ids())
            .field("has_prompt_layer", &(!self.prompt.is_empty()))
            .finish()
    }
}
#[derive(Clone)]
pub struct ProtocolTurnExtensionHandle(Arc<dyn ProtocolTurnExtension>);
impl ProtocolTurnExtensionHandle {
    /// Type-erases and shares a turn extension for protocol implementors while retaining its
    /// downcast and prompt-contribution behavior.
    pub fn new(extension: impl ProtocolTurnExtension + 'static) -> Self {
        Self(Arc::new(extension))
    }

    /// Exposes the erased extension for protocol implementors that must downcast back to their
    /// concrete turn-extension type.
    pub fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }

    pub fn prompt_contributions(&self) -> Vec<crate::PromptContribution> {
        self.0.prompt_contributions()
    }
}
impl fmt::Debug for ProtocolTurnExtensionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtocolTurnExtensionHandle(..)")
    }
}
pub trait ProtocolTurnExtension: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn prompt_contributions(&self) -> Vec<crate::PromptContribution> {
        Vec::new()
    }
}

/// Stable identifier for a semantic turn activity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnActivityId(pub Arc<str>);
impl TurnActivityId {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }
}

pub mod facade_ops {

    /// Facade-internal operations for [`TurnContext`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait TurnContextFacadeOps {
        fn has_plugin_input(&self, plugin_id: &'static str) -> bool;

        fn set_prompt_template(&mut self, template: crate::PromptTemplate);

        fn add_prompt_contribution(&mut self, contribution: crate::PromptContribution);

        // APIT is intentionally non-dyn-compatible; this trait has one static-dispatch impl.
        fn replace_prompt_slot(
            &mut self,
            slot: crate::PromptSlot,
            contributions: impl IntoIterator<Item = crate::PromptContribution>,
        );

        fn clear_prompt_slot(&mut self, slot: crate::PromptSlot);
    }
}

/// The generated encoder and decoder matches are exhaustive, so adding a variant requires its
/// persisted spelling here and necessarily extends `ALL`.
macro_rules! turn_input_wire {
    ($type:ident, $visibility:vis, $encoder:ident, $decoder:ident {
        $($variant:ident => $wire:literal),+ $(,)?
    }) => {
        impl $type {
            #[allow(dead_code)]
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            $visibility fn $encoder(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }

            #[allow(dead_code)]
            $visibility fn $decoder(value: &str) -> Option<Self> {
                match value {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

turn_input_wire!(TurnInputCheckpointBoundary, pub, as_wire_str, from_wire_str {
    AfterWork => "after_work",
    BeforeCompletion => "before_completion",
});

turn_input_wire!(TurnInputStateKind, pub, as_str, from_wire_str {
    PendingActive => "pending_active",
    DeferredNextTurn => "deferred_next_turn",
    Accepted => "accepted",
    Cancelled => "cancelled",
    Completed => "completed",
});

impl TurnInput {
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
    pub fn durable_projection(&self) -> Self {
        Self {
            items: self.items.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: crate::TurnContext::default(),
        }
    }
}

#[cfg(test)]
#[path = "turn_input_vocabulary_tests.rs"]
mod tests;
