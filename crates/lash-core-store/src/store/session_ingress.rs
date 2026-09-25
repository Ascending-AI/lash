//! The one session-ingress store seam (ADR 0101).
//!
//! [`SessionIngressStore`] replaces the two ingress families — pending turn
//! inputs and queued work — with one table, one order, one claim type and one
//! settlement planner. It owns admission, claim, abandon, host withdrawal by
//! id, source key or suffix, the list read (with `held`) and vacuum.
//!
//! Every claim, reclaim and settlement is fenced by the drive: it presents a
//! [`DriveFence`], and the store checks it against the session's drive epoch
//! (on `session_meta`) in the same transaction (ADR 0105 §2, B3). These are
//! storage operations an engine calls; the store acquires nothing, sweeps
//! nothing and expires nothing. The one ownership rule it applies itself is
//! ADR 0101 §7's takeover of interrupted holds inside a claim: an interrupted
//! idle claim at the lane head is re-derived exactly, an interrupted
//! checkpoint hold belongs to its turn's redrive, and an ended turn's hold
//! belongs to nobody. Every other ownership move is the claim CAS of
//! [`SessionIngressStore::reclaim_ingress_claim`].
//!
//! Settlement is not a method here: claims settle inside the head commit that
//! delivers or applies them. A backend observes the rows a settlement names
//! under its commit lock, hands them to
//! [`plan_ingress_settlement`](super::session_ingress_plan::plan_ingress_settlement)
//! and executes the planned writes in the same transaction.

use super::{DriveFence, StoreError};
use crate::session_ingress_vocabulary::{
    ClaimMode, IngressAffectedItem, IngressClaim, IngressClaimIdentity, IngressEnqueueOutcome,
    IngressItemDraft, IngressItemId, IngressItemRead, IngressReclaimOutcome,
    IngressSuffixWithdrawOutcome, IngressWithdrawReceipt, IngressWithdrawTarget,
};
use crate::{SessionId, TurnCancelDisposition, TurnCancelMode, TurnId};

/// The per-claim bounds of one turn-lane claim (ADR 0101 §5.2).
///
/// Every bound is a stop point inside the one FIFO prefix, never a filter: a
/// backlog beyond a bound stays queued in order. The head row is always
/// claimable on its own, so a claim is never empty because of a bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngressClaimPolicy {
    /// The host-input cap (FIG-3532; 64 by default).
    pub max_inputs: usize,
    /// The process-wake cap.
    pub max_wakes: usize,
    /// The one total bound over every kind.
    pub max_items: usize,
    /// Once the oldest claimable row has waited this long it is claimed
    /// alone, so a stream of later rows cannot hold it back.
    pub max_pending_age_ms: u64,
    /// Rendered-context capacity the claim may fill, counting one serialized
    /// payload byte as one token.
    pub available_tokens: usize,
}

impl IngressClaimPolicy {
    /// The FIG-3532 default turn-input cap.
    pub const DEFAULT_MAX_INPUTS: usize = 64;

    /// A policy bounded only by `max_items`, with every per-kind cap at that
    /// bound, no age bound and no token bound.
    #[must_use]
    pub const fn bounded(max_items: usize) -> Self {
        Self {
            max_inputs: max_items,
            max_wakes: max_items,
            max_items,
            max_pending_age_ms: u64::MAX,
            available_tokens: usize::MAX,
        }
    }
}

/// The durable turn cancel a settlement applies by author (ADR 0101 §10).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngressTurnCancel {
    pub turn_id: TurnId,
    pub request_id: String,
    pub mode: TurnCancelMode,
    /// Applied to the host-authored items addressed to the cancelled turn
    /// that it did not deliver.
    pub undelivered: TurnCancelDisposition,
}

/// How a drained command settled (ADR 0101 §12).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum IngressCommandResult {
    Applied,
    /// An `ApplyConfigPatch` refused because the running config revision was
    /// `head`, not the `base` it was written against. Nothing changed.
    StaleConfigRevision {
        base: u64,
        head: u64,
    },
}

/// One drained command's settlement.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngressCommandOutcome {
    pub item_id: IngressItemId,
    pub result: IngressCommandResult,
}

/// One claim a settlement covers: its identity and every item it holds.
///
/// The items are named so the store can check claim identity row by row: a
/// row that no longer carries this claim supersedes the whole settlement.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngressClaimRef {
    pub identity: IngressClaimIdentity,
    pub item_ids: Vec<IngressItemId>,
}

impl IngressClaimRef {
    /// The reference that settles `claim` whole.
    #[must_use]
    pub fn of(claim: &IngressClaim) -> Self {
        Self {
            identity: claim.identity(),
            item_ids: claim.item_ids(),
        }
    }
}

/// What the committing operation did with its claims.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngressSettlementIntent {
    /// A turn's commit. Every item in `delivered` was rendered by the turn and
    /// completes as `Delivered`; every other claimed item is released at its
    /// own position, or, when the turn was cancelled, takes the cancel's
    /// by-author disposition with its record.
    Turn {
        delivered: Vec<IngressItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel: Option<IngressTurnCancel>,
    },
    /// A command drain's commit at a turn boundary.
    Commands {
        outcomes: Vec<IngressCommandOutcome>,
    },
}

/// One settlement of ingress claims, executed inside the commit that
/// delivers or applies them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngressClaimSettlement {
    pub session_id: SessionId,
    pub claims: Vec<IngressClaimRef>,
    pub intent: IngressSettlementIntent,
}

/// What one settlement did beyond completing: every item it deferred or
/// dropped with a reason, in `enqueue_seq` order.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct IngressSettlementReceipt {
    pub affected: Vec<IngressAffectedItem>,
}

/// One session with open ingress, as the cross-session read answers it: the
/// session and its oldest open item, which names the drive a reconcile sweep
/// asks for (ADR 0104 O2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenIngressSession {
    pub session_id: SessionId,
    pub oldest_open_item: IngressItemId,
}

/// The one session-ingress capability (ADR 0101 §1).
///
/// Every producer takes the session lock before it takes the sequence
/// number, so enqueue order equals per-session commit order. Claims are fenced
/// by the session's drive epoch with per-row claim identity and no expiry: a
/// claim is live exactly while the epoch it pins is the session's epoch, and a
/// stale fence is refused with [`StoreError::StaleDriveFence`].
#[async_trait::async_trait]
pub trait SessionIngressStore: Send + Sync {
    /// Admit one item under the session lock.
    ///
    /// A replay under the same source key (or provisioned item id) compares
    /// the immutable submission digest only: the same digest is `Existing`,
    /// open or tombstoned, and a `cancelled` tombstone is never reopened; a
    /// different digest is `Conflict` for every kind. A wake's conflict is its
    /// terminal discard and raises the redelivery floor in the same
    /// transaction. A wake at or below the floor with no row left is
    /// `WakeRewound`.
    ///
    /// Refused with nothing stored: a reserved source-key prefix
    /// ([`StoreError::IngressReservedSourceKey`]) and a turn address that is
    /// neither the running turn nor an ended turn of the session
    /// ([`StoreError::IngressTurnAddressUnknown`]).
    async fn enqueue_ingress_item(
        &self,
        draft: IngressItemDraft,
    ) -> Result<IngressEnqueueOutcome, StoreError>;

    /// Claim the command lane at a turn boundary under `fence`.
    ///
    /// The claim takes the lane head, and when the head is an
    /// `ApplyConfigPatch`, every adjacent config patch after it, so they
    /// coalesce into one head commit; any other command is claimed alone. The
    /// command lane never looks at the turn lane, and the turn lane never
    /// waits for it. `None` when the lane is empty or its head is held.
    async fn claim_session_commands(
        &self,
        fence: &DriveFence,
    ) -> Result<Option<IngressClaim>, StoreError>;

    /// Claim the turn lane at `mode` under `fence` (ADR 0101 §5.2).
    ///
    /// A checkpoint claim of turn `t` takes the open `Turn{t}` items its
    /// checkpoint admits, in order, regardless of earlier rows. Both modes
    /// then take the FIFO prefix of the unaddressed turn-lane rows, stopping —
    /// never skipping — at the first row the mode cannot deliver, a held row,
    /// a per-kind cap, the total bound, or a policy bound. An interrupted
    /// idle claim at the head is re-derived exactly from its persisted row
    /// identity, without consulting the policy. `None` when nothing is
    /// claimable.
    async fn claim_turn_items(
        &self,
        fence: &DriveFence,
        mode: ClaimMode,
        policy: &IngressClaimPolicy,
    ) -> Result<Option<IngressClaim>, StoreError>;

    /// Reclaim, under `fence`, every row a resumed run's `claim` still holds
    /// (FIG-3552).
    ///
    /// Ownership of a claimed row moves only through the claim CAS. When every
    /// row of `claim` still carries it, all of them move to one new claim
    /// pinned to `fence`'s drive epoch, keeping the claim's mode and naming
    /// `claim` as its predecessor; when the stored rows already carry it at
    /// that epoch, the claim is returned as the rows record it. When a peer superseded any row — re-claimed,
    /// settled or withdrawn it — the reclaim cedes and writes nothing, so a
    /// redrive never drops a row and retries with the rest.
    async fn reclaim_ingress_claim(
        &self,
        fence: &DriveFence,
        claim: &IngressClaim,
    ) -> Result<IngressReclaimOutcome, StoreError>;

    /// Release, under `fence`, a claim of `fence`'s own drive epoch without
    /// settling it, so it can be claimed again.
    ///
    /// Every row the claim still holds at that epoch returns to `open` at its
    /// own position, never as a unit: the next claim recomposes from rows
    /// (ADR 0101 §7). A row the claim no longer holds is left alone. A stale
    /// fence, or a claim a superseded epoch took, is refused with
    /// [`StoreError::StaleDriveFence`] and releases nothing: only the current
    /// epoch moves a hold, and it does so through reclaim.
    async fn abandon_ingress_claim(
        &self,
        fence: &DriveFence,
        claim: &IngressClaim,
    ) -> Result<(), StoreError>;

    /// Withdraw items by id or source key, atomically, for the host
    /// (ADR 0101 §10). A withdrawn item gets a `cancelled` tombstone and its
    /// record; a withdrawn wake raises the redelivery floor in the same
    /// transaction. An item a live claim holds is not withdrawn.
    async fn withdraw_ingress_items(
        &self,
        session_id: &SessionId,
        targets: &[IngressWithdrawTarget],
    ) -> Result<Vec<IngressWithdrawReceipt>, StoreError>;

    /// Withdraw the anchor and every later item in its lane, atomically.
    async fn withdraw_ingress_suffix(
        &self,
        session_id: &SessionId,
        anchor: &IngressWithdrawTarget,
    ) -> Result<IngressSuffixWithdrawOutcome, StoreError>;

    /// Every open item of the session, in `(lane, enqueue_seq)` order, with a
    /// live claim's hold projected as `Held`.
    async fn list_ingress_items(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<IngressItemRead>, StoreError>;

    /// Delete the session's tombstones, uniformly across kinds. A wake
    /// tombstone above its process's redelivery floor is never removed.
    /// Returns how many rows were removed.
    async fn vacuum_session_ingress(&self, session_id: &SessionId) -> Result<u64, StoreError>;
}
