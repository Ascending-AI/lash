//! The request and response types of the durable-wait handlers.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RestateDurableWaitClassification {
    DurableWait,
    TurnControl,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateDurableWaitAwaitRequest {
    pub key: AwaitEventKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<RestateDurableWaitDeadline>,
}

/// Decoder for the durable-wait workflow's clean-cutover request boundary.
///
/// The predecessor is decoded only so the handler can return the same typed,
/// actionable incompatibility as an unsupported stamped deadline. It is never
/// executed or translated into the current absolute-deadline request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum RestateDurableWaitAwaitInput {
    Current(RestateDurableWaitAwaitRequest),
    Predecessor { key: AwaitEventKey, timeout_ms: u64 },
}

impl From<RestateDurableWaitAwaitRequest> for RestateDurableWaitAwaitInput {
    fn from(request: RestateDurableWaitAwaitRequest) -> Self {
        Self::Current(request)
    }
}

/// Absolute deadline carried by the version-2 durable-wait request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateDurableWaitDeadline {
    pub version: u8,
    pub unix_epoch_ms: u64,
}

impl RestateDurableWaitDeadline {
    pub(super) fn validate(self) -> Result<(), TerminalError> {
        if self.version != DURABLE_WAIT_REQUEST_VERSION {
            return Err(incompatible_durable_wait_request(format!(
                "version {}",
                self.version
            )));
        }
        Ok(())
    }

    pub(crate) fn remaining(self, now_ms: u64) -> Result<Duration, TerminalError> {
        self.validate()?;
        Ok(Duration::from_millis(
            self.unix_epoch_ms.saturating_sub(now_ms),
        ))
    }
}

pub(super) fn incompatible_durable_wait_request(observed: impl std::fmt::Display) -> TerminalError {
    TerminalError::new(format!(
        "Lash Restate durable-wait request {observed} is incompatible with version {DURABLE_WAIT_REQUEST_VERSION}; drain deadline-bearing waits before opening this deployment"
    ))
}

#[cfg(test)]
impl RestateDurableWaitAwaitRequest {
    pub(crate) fn address(&self) -> RestateDurableWaitAddress {
        RestateDurableWaitAddress::for_key(&self.key)
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitResolveRequest {
    pub key: AwaitEventKey,
    pub resolution: Resolution,
}

/// What `LashDurableWaitIndex/resolve` answers: the promise's first-writer
/// outcome, or the typed refusal a completion delivered to a cancel-decided
/// group child's key earns (ADR 0099 §4, W17).
///
/// The encoding is a superset of [`ResolveOutcome`]'s: an outcome encodes
/// exactly as before, so a journal that recorded this handler's answer before
/// the refusal existed still replays, and the refusal is the one further
/// `status` no outcome carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum RestateDurableWaitResolveResponse {
    Outcome(ResolveOutcome),
    Refused(RestateDurableWaitResolveRefusal),
}

/// The refusal arm of [`RestateDurableWaitResolveResponse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RestateDurableWaitResolveRefusal {
    CancelDecided,
}

impl RestateDurableWaitResolveResponse {
    /// The host-facing answer: the outcome, or
    /// `RuntimeEffectGroupChildCancelDecided`.
    pub fn into_result(self) -> Result<ResolveOutcome, RuntimeError> {
        match self {
            Self::Outcome(outcome) => Ok(outcome),
            Self::Refused(RestateDurableWaitResolveRefusal::CancelDecided) => {
                Err(lash_core::facade_support::await_event_identity::cancel_decided_refusal())
            }
        }
    }
}

/// Closes the completion key `scope`/`wait` names, because the group child
/// that owns it is cancel-decided (ADR 0099 §4, W17). Sent by the group index
/// that decides the child, to the index object that owns `scope`'s waits.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitCancelDecidedRequest {
    pub scope: ExecutionScope,
    pub wait: AwaitEventWaitIdentity,
}

#[cfg(test)]
impl RestateDurableWaitResolveRequest {
    pub(crate) fn address(&self) -> RestateDurableWaitAddress {
        RestateDurableWaitAddress::for_key(&self.key)
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitIndexRequest {
    pub key: AwaitEventKey,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitSettleRequest {
    pub key: AwaitEventKey,
    pub resolution: Resolution,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitRootRequest {
    pub session_id: SessionId,
    pub root: lash_core::TurnId,
    /// The physical turn whose commit ended the root, when a commit did. That
    /// commit publishes its turn's terminal one-way after it, so the terminal
    /// may still be in flight when the root retires; `None` says no commit
    /// ended the root (FIG-4025).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_turn: Option<lash_core::TurnId>,
}

/// One executing effect under a scope's index (FIG-2499).
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitEffectRequest {
    pub replay_key: String,
}

/// One effect group opened under a scope's index (FIG-2499).
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitGroupRequest {
    pub group_key: String,
}

/// One group child's durable membership binding under its own scope's index:
/// the replay key the §4 boundary names and the group the dispatch admitted
/// it to. The row is the Restate twin of the SQL tiers' `group_key` column —
/// who asked carries no weight; the record decides (FIG-3409).
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitGroupChildRequest {
    pub replay_key: String,
    pub group_key: String,
}

/// The membership read [`RestateDurableWaitGroupChildRequest`] records.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitGroupChildMembershipRequest {
    pub replay_key: String,
}

/// One session catalog whose durable cancellation closure may still depend on
/// this physical scope's promises.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateTurnCancelClosureParticipantRequest {
    pub participant_id: String,
}

/// One turn-cancel gate entry: the awakeable the index resolves when this
/// session's turn-control wait settles or the session is revoked.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAwakeableRequest {
    pub key: AwaitEventKey,
    pub awakeable_id: String,
}

/// Why a registered turn-cancel gate awakeable fired.
///
/// Every gate — sleep, await-event, process await — takes this one payload, so
/// the index resolves a gate entry without knowing which wait registered it.
/// The payload is the awakeable's journaled value, so the mode of the request
/// that settled the gate is part of the journal and replay reads the identical
/// verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestateTurnCancelWake {
    /// The gate settled with an `Immediate` request: the parked wait unwinds
    /// now. Journals written before the request mode existed carry this
    /// value, which is why it keeps its pre-mode name and wire literal.
    TurnCancelled,
    /// The gate settled with an `AfterStep` request: the parked wait keeps
    /// waiting, the iteration finishes, and the turn stops at its step
    /// boundary. The waiter re-parks on the escalation promise so a later
    /// `Immediate` request still unwinds it.
    TurnCancelDeferred,
    SessionRevoked,
}

impl RestateTurnCancelWake {
    /// The wake a settled turn-control resolution owes its parked waiters.
    ///
    /// The gate resolution is lash-core's journaled `TurnGateTerminal`; only
    /// its `cancellation.mode` matters here. Anything that is not a decodable
    /// after-step request — an immediate request, a pre-mode record, a sealed
    /// completion, an unexpected shape — wakes as `TurnCancelled`, which is
    /// the verdict every gate resolution produced before the mode existed.
    pub(crate) fn for_gate_resolution(resolution: &Resolution) -> Self {
        let Resolution::Ok(value) = resolution else {
            return Self::TurnCancelled;
        };
        let evidence = value.get("cancellation").cloned().and_then(|cancellation| {
            serde_json::from_value::<lash_core::facade_support::TurnCancellationEvidence>(
                cancellation,
            )
            .ok()
        });
        match evidence {
            Some(evidence) if !evidence.mode.is_immediate() => Self::TurnCancelDeferred,
            _ => Self::TurnCancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum RestateDurableWaitRegistration {
    Registered,
    Resolved(Resolution),
    Revoked,
}

/// What a turn cancellation gate's peek reads from its session's index
/// (FIG-3978).
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum RestateTurnGatePeek {
    /// The session was revoked: the gate has nothing left to read.
    Revoked,
    /// The gate's terminal, or `None` while the gate is open.
    Open(Option<Resolution>),
}
