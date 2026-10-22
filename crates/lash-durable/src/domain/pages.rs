//! S6: store publication of persistent-heap pages (FIG-6195,
//! `docs/design/persistent-heap-seams.md`).
//!
//! Declarations only. Both SQL stores implement [`PageStore`] over the
//! generic content-addressed node store and the existing fenced owner
//! transaction:
//!
//! - a root lease is a root-catalog owner row: while it lives, reclamation
//!   keeps every node its root reaches, and the parent serves demand reads
//!   under it without a recursive closure query;
//! - a candidate's install checks and key-share locks only its direct
//!   external references ([`check_graph`](super::graph_commit::check_graph)'s
//!   inductive rule), inserts its fresh nodes in global digest order, then
//!   their edges, then publishes the execution root, host records, effect
//!   admissions, in the same fenced
//!   transaction;
//! - retiring an old root moves it to a durable retirement hold in the
//!   publication transaction; bounded background steps peel it later,
//!   keeping what they did not reach on a frontier of their own. A step
//!   waits on no publication, lease or peel (the PostgreSQL lock order,
//!   FIG-6193).

use std::sync::Arc;

use lash_core_store::checkpoint_graph::stage::CandidateManifest;
use lash_core_store::checkpoint_graph::{CommitId, NodeId};

use super::graph::RootOwner;
use super::keys::{ExecKey, Ordinal, OwnerKey, RunSeq};
use crate::error::DurableError;
use crate::ids::{ActorKey, CommitLabel, DurableInstant, Epoch};
use crate::port::{ActorCommit, DurableStore};
use crate::tx::ActorTx;

/// A root lease's id: its catalog row's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootLeaseId(pub u64);

/// Under whose authority a root lease holds its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseFence {
    /// An activation under `actor`'s fence at `epoch`: acquired and renewed
    /// only while that fence holds.
    Actor { actor: ActorKey, epoch: Epoch },
    /// A reader outside every actor, such as a replay of retained records:
    /// it only keeps its root from retiring until it expires, and it
    /// authorizes no write.
    Reader,
}

/// What a broker asks to lease, transactionally checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootLeaseRequest {
    /// The owning row whose root is leased (an execution, a session's
    /// carried root, a function bundle).
    pub owner: RootOwner,
    /// The root the owner must hold now; another is refused.
    pub expected: NodeId,
    pub fence: LeaseFence,
    pub ttl_millis: i64,
}

/// A live lease on one committed root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootLease {
    pub id: RootLeaseId,
    pub owner: RootOwner,
    pub root: NodeId,
    pub fence: LeaseFence,
    /// Past this instant of the store's clock every read and renewal under
    /// the lease is refused; the row stays a root owner until released or
    /// reaped by retirement.
    pub expires_at: DurableInstant,
}

/// One page read under a lease, its bytes as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPage {
    pub id: NodeId,
    pub bytes: Arc<[u8]>,
}

/// The one write of the owner commit that owns a candidate (FIG-6197 F1):
/// the root it publishes reaches the candidate's semantic root through
/// nodes the commit supplies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateOwner {
    /// The execution root `exec`'s row moves to (a park or an end). The
    /// manifest is built against the root the row holds now.
    ExecRootPut(ExecKey),
    /// The run fact appended at `(owner, run, ordinal)`: a process engine
    /// step's outcome, whose record owns the candidate until the process's
    /// own root names it. The manifest has no base.
    RunFactAppend {
        owner: OwnerKey,
        run: RunSeq,
        ordinal: Ordinal,
    },
    /// The session's execution root: a turn's working state
    /// ([`TurnWrite::Working`](super::TurnWrite::Working)) or a head
    /// commit's checkpoint
    /// ([`SessionCommitWrite::execution`](super::SessionCommitWrite::execution)).
    /// The root is the candidate's own semantic root, the session
    /// descriptor; the manifest has no base (FIG-6418).
    Session(lash_vocabulary::identity::SessionId),
}

/// A candidate published by the owner commit it travels with.
pub struct CandidatePublication {
    pub owner: CandidateOwner,
    pub manifest: Box<dyn CandidateManifest>,
}

/// An authoritative publication: the activation's new baseline. Its
/// effect admissions are committed; dispatch may start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateReceipt {
    pub actor: ActorCommit,
    pub root: NodeId,
    pub commit: CommitId,
}

/// The bounds of one retirement step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetirementBudget {
    /// The most nodes one step deletes.
    pub nodes: u64,
}

/// What one retirement step did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetirementProgress {
    /// Holds and frontier nodes the step took (each replaced by frontier
    /// nodes for its children that it did not delete).
    pub holds_advanced: u64,
    pub nodes_deleted: u64,
    /// Expired root leases the step removed.
    pub leases_reaped: u64,
}

/// The retirement backlog, for host admission and backpressure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetirementBacklog {
    /// Holds and frontier nodes waiting for a step.
    pub holds: u64,
    pub oldest_at: Option<DurableInstant>,
}

/// Publication, leases and reclamation of persistent-heap pages.
///
/// Implemented by the SQLite and PostgreSQL durable stores (layer
/// hp-publish, FIG-6191).
#[async_trait::async_trait]
pub trait PageStore: DurableStore {
    /// Leases `request.owner`'s root: in one transaction, under the actor's
    /// fence for an actor's lease, checks the owner holds `expected` and
    /// inserts the lease row. A reader's lease is checked against no fence.
    ///
    /// # Errors
    ///
    /// [`DurableError::OwnershipLost`] for a stale epoch, and a graph
    /// refusal (`RevisionConflict`, `RootRetired`) for another root.
    async fn acquire_root_lease(
        &self,
        request: RootLeaseRequest,
    ) -> Result<RootLease, DurableError>;

    /// Extends a live lease by its ttl under the same fence (none for a
    /// reader's).
    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError>;

    /// Ends a lease; its root stays owned by its other owners.
    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError>;

    /// Reads `pages` in one transaction that checks the lease is live. The
    /// caller authorizes each page by capability; the store checks only
    /// the lease, never a closure.
    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError>;

    /// Commits `tx` with `candidate` in its one fenced transaction: the
    /// candidate's nodes and edges with the transaction's own staged nodes,
    /// and its domain writes (the execution root, host records, effect
    /// admissions), atomically.
    async fn commit_candidate(
        &self,
        tx: ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
    ) -> Result<CandidateReceipt, DurableError>;

    /// The execution's current root and commit identity: how an ambiguous
    /// publication is resolved before it is retried or discarded.
    async fn candidate_state(
        &self,
        exec: &ExecKey,
    ) -> Result<Option<(NodeId, CommitId)>, DurableError>;

    /// One bounded retirement step: takes frontier nodes and holds, deletes
    /// the nodes under them that no root owner, lease or inbound edge
    /// keeps, and puts their children on the frontier, atomically. It
    /// waits on no publication, lease or other peel.
    async fn retire_step(
        &self,
        budget: RetirementBudget,
    ) -> Result<RetirementProgress, DurableError>;

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError>;
}
