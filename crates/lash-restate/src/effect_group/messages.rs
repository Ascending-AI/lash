//! The request and response types of the effect-group handlers.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupOpenResponse {
    /// A fresh group: `dispatch_route` is the recorded route the dispatch
    /// runs under, as the request declared it (FIG-3795 S10).
    OpenedFresh {
        dispatch_route: String,
    },
    ReopenedReady,
    /// A preparing group the caller may submit the dispatch for:
    /// `dispatch_route` is the route the index retains, which a reopen's
    /// offer never overrides.
    ReopenedPreparing {
        dispatch_route: String,
    },
    ReopenedClosed {
        effective: EffectGroupCloseOutcome,
    },
    Retired,
    ShapeMismatch,
    /// A content-checked reopen offered a child that is not the retained one
    /// at `position`.
    ContentMismatch {
        position: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupProbeAdoptResponse {
    /// The dispatch was adopted; `shape` is the recorded shape and
    /// `membership` the retained membership, the authoritative child set — a
    /// reopen's offered children never reach dispatch.
    Adopted {
        shape: EffectGroupShape,
        membership: EffectGroupMembership,
    },
    /// A redrive of an already-adopted dispatch, answered with the same
    /// recorded shape and membership so the replayed `run` rebuilds the same
    /// children.
    AlreadyAdopted {
        shape: EffectGroupShape,
        membership: EffectGroupMembership,
    },
    DifferentDispatcher,
    Ready,
    Closed,
    UnknownGroup,
    Retired,
}

/// What registering a dispatch's children did (FIG-4308): the whole
/// position-to-invocation map and the move to ready are one index step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRegisterDispatchResponse {
    /// The map is recorded and the group is ready: every ADMIT wake and READY
    /// were resolved.
    Registered,
    /// The group is already ready under this same map, so a redriven
    /// registration re-resolves the same wakes and changes nothing.
    AlreadyRegistered,
    /// The group already closed under this same map.
    AlreadyClosed,
    /// The map does not name exactly the group's positions, disagrees with
    /// the one recorded, or the group has no adopted dispatcher to register.
    Mismatch,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRegisterRefusalResponse {
    Refused,
    AlreadyRegistered,
    AlreadyClosed,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupAdmissionResponse {
    Admitted,
    NotYetRecorded,
    /// The presenting invocation is not the one that holds this position: the
    /// index retains a *different* invocation id for it — the retained
    /// invocation's retention expired and the idempotency-keyed re-dispatch
    /// minted a fresh one — or the position's final is already committed, so
    /// the invocation that committed it is gone and this one re-runs its
    /// journal from nothing (FIG-4454). Distinct from `Refused` because the
    /// successor must seat the committed final, or surface the typed
    /// `AttachExpired` failure where none is committed, rather than exit
    /// silently — the rank it would never settle is a caller's wait (ADR 0099
    /// §8).
    AttachExpired,
    /// The close decided this child `Cancel` before it was admitted. The
    /// close owns everything that follows, including releasing a wait
    /// child's wait (ADR 0099 §12, FIG-3630), so the child just exits.
    CancelDecided,
    Refused,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRecordSettlementResponse {
    Recorded {
        rank: u64,
    },
    Duplicate {
        rank: u64,
    },
    /// Refused: the cancel disposition won this child's §4 point before its
    /// final record arrived. `rank` is the seat the decision holds — the
    /// child's own terminal journals nothing.
    CancelDecided {
        rank: u64,
    },
    UnknownChild,
    UnknownGroup,
    Retired,
}

/// One child's final record reaching the §4 point: the index-side decision
/// the durable tiers' `commit_group_child` mirrors.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupCommitChildRequest {
    /// The child's declared replay key; the index resolves its position from
    /// the retained shape rather than trusting a caller-supplied position.
    pub replay_key: String,
    /// The final this invocation offers the point. The index retains it with
    /// a winning commit, and answers it to every later commit of the child.
    pub committed: EffectGroupCommittedFinal,
}

/// What a child's final committed at the §4 point, as the index retains it
/// for any later invocation of the child (ADR 0099 §5).
///
/// The committed final wins: an invocation whose commit finds the point taken
/// seats the committed final, never its own. The invocation that committed it
/// may have ended before its seat, so the index keeps what any successor needs
/// to seat it — or, where no successor can, what it must report lost.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCommittedFinal {
    /// A tool child's terminal: its sealed drain input — its record, its
    /// declared intents and the attempt facts its settlement carries. A
    /// successor drains it and seats that final; the attempt never runs again.
    Tool { drain_input: String },
    /// An atomic body's or a wait's outcome, or a tool child's failure before
    /// its boundary: the committing invocation alone holds it, and publishes
    /// it at its seat.
    Held,
    /// The typed refusal of an invocation that could not run the child: its
    /// session's state generation was refused, or its attach expired.
    Refusal { error: RuntimeEffectControllerError },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCommitChildResponse {
    /// This child's final won the §4 point, and `rank` is the settlement
    /// rank the point reserved for it: its durable place in the group's
    /// decision order, which its seat publishes (FIG-4308).
    Committed {
        rank: u64,
    },
    /// The point already holds a final of this child, committed by an earlier
    /// invocation: the rank it reserved and the final the index retained,
    /// which wins over the one this commit offered.
    AlreadyCommitted {
        rank: u64,
        committed: EffectGroupCommittedFinal,
    },
    /// The cancel disposition won first; the child's final journals nothing.
    CancelDecided {
        rank: u64,
    },
    UnknownChild,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupReadRankResponse {
    Settled {
        settlement: EffectGroupSettlementRecord,
        /// The settled child's durable identity — the replay key a §6 prefix
        /// record or a §8 attach names, derived from the retained shape rather
        /// than stored twice on the settlement record.
        child_replay_key: String,
    },
    /// The answer to a read that asked for the run (FIG-4088): every rank
    /// seated consecutively from the one asked for, in rank order, each with
    /// its stored payload. A reader takes a burst of settlements from one
    /// journaled call instead of a read and a payload get per rank.
    SettledRun {
        ranks: Vec<EffectGroupServedRank>,
    },
    NotSettled,
    Closed,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCloseResponse {
    Closed,
    AlreadyClosed,
    WidenRefused,
    NotReady,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRetireResponse {
    Retired { cleanup: EffectGroupCleanupFacts },
    AlreadyRetired { cleanup: EffectGroupCleanupFacts },
    Tombstone,
    UnknownGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupFinishRetirementResponse {
    Finished,
    AlreadyFinished,
    NotRetired,
    UnknownGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRetirementCancelResponse {
    Applied,
    AlreadyApplied,
    Tombstone,
    NotRetired,
    UnknownGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupOpenRequest {
    pub shape: EffectGroupShape,
    /// The offered membership: every child's canonical envelope, in child
    /// order. A fresh open retains it; a reopen's is only compared, and the
    /// retained one wins (ADR 0099 §3).
    pub membership: EffectGroupMembership,
    /// The route — the full Restate service name — the group's dispatch is
    /// sent under (FIG-3795 S10). The index records it verbatim at open, and
    /// a reopen keeps the retained route: the route is data, and every later
    /// dispatcher self-call or host-side group call addresses the recorded
    /// route rather than recomputing a name.
    pub dispatch_route: String,
    /// The opener asked for a content-checked reopen (FIG-3586,
    /// `GroupReopen::RetainedContent`): a reopen whose offered membership
    /// differs from the retained one is refused rather than served.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_checked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupAdoptRequest {
    pub invocation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupRegisterDispatchRequest {
    /// Every child's invocation id by position: a dispatch records the whole
    /// group, and makes it ready, in one call (FIG-4088, FIG-4308).
    #[serde(with = "btree_map_as_pairs")]
    pub addresses: BTreeMap<usize, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupRefusalRequest {
    pub reason: EffectGroupRefusal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupAdmissionRequest {
    pub position: usize,
    pub invocation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupRecordSettlementRequest {
    pub position: usize,
    pub terminal: EffectGroupSettlementTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupReadRankRequest {
    pub rank: u64,
    /// The read is the group caller's own await. A caller is refused a
    /// closed group's ranks until it reopens the group, as the SQL engines'
    /// caller path refuses (FIG-3676); the host's own readers (drain,
    /// finalization, the settlement reader) see every recorded rank.
    #[serde(default)]
    pub for_caller: bool,
    /// Answer a seated `rank` with the run seated consecutively from it,
    /// payloads included ([`EffectGroupReadRankResponse::SettledRun`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run: bool,
}

/// One seated rank as a run read serves it (FIG-4088).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupServedRank {
    pub settlement: EffectGroupSettlementRecord,
    /// The settled child's durable identity, as
    /// [`EffectGroupReadRankResponse::Settled`] names it.
    pub child_replay_key: String,
    /// The child's payload, for a [`EffectGroupSettlementTerminal::StoredPayload`]
    /// settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<EffectGroupPayloadGetResponse>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupCloseRequest {
    pub disposition: LoserPolicy,
}
