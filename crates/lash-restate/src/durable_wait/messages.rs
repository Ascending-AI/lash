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

/// The promise's first-writer outcome or a typed native source refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum RestateDurableWaitResolveResponse {
    Outcome(ResolveOutcome),
    Refused(RestateDurableWaitResolveRefusal),
}

/// The refusal arm of [`RestateDurableWaitResolveResponse`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RestateDurableWaitResolveRefusal {
    Source {
        refusal: lash_core::tool_run::SourceRefusal,
    },
}

impl RestateDurableWaitResolveResponse {
    /// The host-facing outcome or typed source refusal.
    pub fn into_result(self) -> Result<ResolveOutcome, RuntimeError> {
        match self {
            Self::Outcome(outcome) => Ok(outcome),
            Self::Refused(RestateDurableWaitResolveRefusal::Source { refusal }) => {
                Err(lash_core::RuntimeEffectControllerError::from(refusal).into_runtime_error())
            }
        }
    }
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
pub struct RestateDurableWaitRunRequest {
    pub session_id: SessionId,
    pub run: lash_core::TurnId,
    /// The physical turn whose commit ended the run, when a commit did. That
    /// commit publishes its turn's terminal one-way after it, so the terminal
    /// may still be in flight when the run retires; `None` says no commit
    /// ended the run (FIG-4025).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_turn: Option<lash_core::TurnId>,
}

/// One executing effect under a scope's index (FIG-2499).
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitEffectRequest {
    pub replay_key: String,
}

/// A process segment's invocation, pinned before its runner can issue effects.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitProcessJournalRequest {
    pub process_id: lash_core::ProcessId,
    pub invocation_id: crate::RestateInvocationId,
}

/// One session catalog whose durable cancellation closure may still depend on
/// this physical scope's promises.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateTurnCancelClosureParticipantRequest {
    pub participant_id: String,
}

/// One watch on a wait: the awakeable the index resolves when the wait `key`
/// names ends or its index is revoked. A turn-cancel gate entry watches its
/// session's turn-control wait.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitAwakeableRequest {
    pub key: AwaitEventKey,
    pub awakeable_id: String,
    /// Set on a turn-cancel gate entry whose guarded wait its Run's successor
    /// segment may take over (FIG-4739): the build generation the waiting
    /// turn runs on. The drain of that generation wakes the entry
    /// ([`RestateTurnCancelWake::HandedOver`]); no other entry is ever woken
    /// for a drain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hand_over: Option<lash_core::engine::BuildGeneration>,
}

/// The drain of `generation` asking a session's parked turn waits to hand
/// over (FIG-4739).
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RestateDurableWaitHandOverRequest {
    pub generation: lash_core::engine::BuildGeneration,
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
    /// The build the waiting turn runs on is draining (FIG-4739): the parked
    /// wait is left open for the Run's successor segment, and the turn ends
    /// at a segment boundary. Only an entry that registered a hand-over
    /// generation is woken with it.
    HandedOver,
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
