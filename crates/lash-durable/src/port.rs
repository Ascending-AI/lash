//! The [`DurableStore`] port and the records it answers with.

use crate::domain::{DurableReads, MailAnswer};
use crate::error::DurableError;
use crate::formats::FormatSet;
use crate::ids::{
    ActorKey, BootId, CommitLabel, DurableInstant, Epoch, MailSeq, NodeId, StateRevision,
};
use crate::tx::{ActorTx, MailTx};

/// An actor's scheduling state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActorState {
    /// Unowned with nothing to do.
    Idle,
    /// Unowned and claimable now.
    Ready,
    /// Owned by a node.
    Owned,
    /// Unowned, blocked until mail arrives or its due time passes.
    Waiting,
    /// Unowned and set aside for an operator: only a control wake (a
    /// cancel request or a redrive) readies it.
    Parked,
    /// Finished; never claimed again.
    Terminal,
}

impl ActorState {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Ready => "ready",
            Self::Owned => "owned",
            Self::Waiting => "waiting",
            Self::Parked => "parked",
            Self::Terminal => "terminal",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "idle" => Some(Self::Idle),
            "ready" => Some(Self::Ready),
            "owned" => Some(Self::Owned),
            "waiting" => Some(Self::Waiting),
            "parked" => Some(Self::Parked),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// A node an actor is owned by: one boot of one node.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    /// The node.
    pub node: NodeId,
    /// Its boot.
    pub boot: BootId,
}

/// What a node registers as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    /// The node's stable name. Registering it fences every earlier boot of
    /// the same name: a node id names one serving process at a time.
    pub node: NodeId,
    /// The format sets this build decodes; it claims only actors in one.
    pub decodes: Vec<FormatSet>,
    /// How long one heartbeat keeps the lease, in milliseconds. Hosts take
    /// it from a validated [`LeaseConfig`](crate::LeaseConfig).
    pub ttl_millis: i64,
}

/// One boot's live lease, as [`DurableStore::register_node`] grants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLease {
    /// The node and boot.
    pub owner: Owner,
    /// The format sets it decodes.
    pub decodes: Vec<FormatSet>,
    /// How long one heartbeat keeps the lease, in milliseconds.
    pub ttl_millis: i64,
    /// When the lease expires unless renewed, as of the registration.
    pub expires_at: DurableInstant,
}

/// A heartbeat's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// The lease is renewed to `expires_at`.
    Renewed {
        /// The new expiry.
        expires_at: DurableInstant,
    },
    /// The lease is gone: the node was reaped or replaced. It must drop
    /// every actor and stop serving at once.
    Reaped,
}

/// Why a claim took an actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimCause {
    /// It was ready: new, woken by mail, or released by a reap.
    Ready,
    /// It was waiting and its due time passed.
    Due,
    /// This boot already owns it: [`DurableStore::owned`] answers it, so a
    /// node whose claim reply was lost can adopt what the claim took.
    Adopted,
}

/// One actor a claim took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The actor.
    pub actor: ActorKey,
    /// The epoch the claimer now owns it under.
    pub epoch: Epoch,
    /// Why it was claimable.
    pub cause: ClaimCause,
    /// What the claiming node may do with it.
    pub purpose: ClaimPurpose,
}

/// What a node claimed an actor for (ADR 0106 §1, ADR 0132 §11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPurpose {
    /// The node decodes the actor's format set: it runs the actor.
    Run,
    /// The node does not decode the actor's format set and claimed it only
    /// for its pending cancel: the activation ends it from registry state
    /// and never reads its state or calls its engine.
    CancelOnly,
}

impl ClaimPurpose {
    /// The purpose a node that decodes `decodes` claims an actor stored in
    /// `formats` for.
    #[must_use]
    pub fn of(decodes: &[FormatSet], formats: &str) -> Self {
        if decodes.iter().any(|set| set.as_str() == formats) {
            Self::Run
        } else {
            Self::CancelOnly
        }
    }
}

/// One actor a reap released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaped {
    /// The actor, now ready.
    pub actor: ActorKey,
    /// The dead owner.
    pub from: Owner,
    /// Its new epoch: the dead owner's is fenced from the reap on.
    pub epoch: Epoch,
}

/// An owner commit's receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActorCommit {
    /// The actor's state revision after the commit.
    pub revision: StateRevision,
    /// The state the actor was left in: [`ActorState::Owned`] unless the
    /// commit released it.
    pub state: ActorState,
}

/// One actor a mailbox commit woke, for the post-commit hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Woken {
    /// The actor.
    pub actor: ActorKey,
    /// Its state after the commit.
    pub state: ActorState,
    /// Its owner, when owned: the node to hint.
    pub owner: Option<Owner>,
}

/// A mailbox commit's receipt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailCommit {
    /// Where each appended mail landed, in write order.
    pub appended: Vec<(ActorKey, MailSeq)>,
    /// Every actor the commit woke, once each, in first-write order.
    pub woken: Vec<Woken>,
    /// One answer per [`MailDomainWrite`](crate::domain::MailDomainWrite),
    /// in write order.
    pub answers: Vec<MailAnswer>,
}

/// An actor's row, read without a fence. For operators and test laws; an
/// owner never decides anything from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorSnapshot {
    /// The actor.
    pub actor: ActorKey,
    /// Its scheduling state.
    pub state: ActorState,
    /// Its current epoch.
    pub epoch: Epoch,
    /// Its owner, when owned.
    pub owner: Option<Owner>,
    /// Whether mail arrived since the owner last acknowledged.
    pub has_mail: bool,
    /// The earliest durable deadline it waits on.
    pub next_due: Option<DurableInstant>,
    /// How many owner commits its state has taken.
    pub revision: StateRevision,
    /// The format set its state is written in.
    pub formats: FormatSet,
    /// How many mail rows are pending.
    pub pending_mail: u64,
    /// Why it is parked, encoded by its owner, while it is parked; kept
    /// through a control wake so its claimer knows it was parked.
    pub park: Option<String>,
    /// How many claims in a row found no commit since the previous claim:
    /// the actor's failed activations (ADR 0132 §3).
    pub failed_activations: u32,
}

/// The fenced transaction port: one implementation per dialect, its SQL in
/// one module there. Every method that writes runs one database
/// transaction, under a [`CommitLabel`].
///
/// No method has a default body: an implementation states every rule. It
/// answers the domain rows' [`DurableReads`] too.
#[async_trait::async_trait]
pub trait DurableStore: DurableReads {
    /// The store's clock: the database's on PostgreSQL, the injected one on
    /// SQLite.
    async fn now(&self) -> Result<DurableInstant, DurableError>;

    /// Register one boot of `spec.node` with a fresh lease.
    ///
    /// Earlier boots of the same node are reaped in the same transaction,
    /// their actors released with their epochs bumped.
    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError>;

    /// Renew `node`'s lease by its ttl. [`HeartbeatOutcome::Reaped`] when
    /// the lease is gone.
    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError>;

    /// Delete every node whose lease expired and release each one's actors
    /// as ready with their epochs bumped, in one transaction. `reaper` is
    /// the node doing it; it must hold a live lease.
    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError>;

    /// End `node`'s lease on a clean shutdown, releasing its actors as
    /// ready with their epochs bumped.
    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError>;

    /// Claim up to `limit` actors that are ready, or waiting with a passed
    /// due time, in a format set `node` decodes; and process actors in a set
    /// it does not decode that have a cancel request pending, so their
    /// cancel ends them without decoding their state
    /// ([`ClaimPurpose::CancelOnly`]). Each claim bumps the actor's
    /// epoch. A draining node claims nothing. Refused with
    /// [`DurableError::NodeLeaseLost`] when the node holds no live lease.
    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError>;

    /// Mark `node` draining (ADR 0106 §1): from this commit on it claims
    /// nothing, and its owners release their actors at their next committed
    /// phase. Refused with [`DurableError::NodeLeaseLost`] when the node
    /// holds no live lease.
    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError>;

    /// The format sets each live node decodes, one entry per node, for the
    /// fleet-format gate ([`fleet_writable`](crate::fleet_writable)).
    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError>;

    /// The actors `node`'s boot owns now, with their current epochs, read
    /// unfenced. Each answers [`ClaimCause::Adopted`].
    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError>;

    /// Open the owner's transaction over `actor` at `epoch`: a fenced read
    /// that returns the pending mail, the store's clock and, for a session
    /// actor, its unfinished turn's accepted cancel request, in one read.
    /// Refused with [`DurableError::OwnershipLost`] when `epoch` is not
    /// current.
    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError>;

    /// Apply `tx` atomically. The transaction's first statement re-checks
    /// the epoch; a stale one is [`DurableError::OwnershipLost`] and nothing
    /// is written.
    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError>;

    /// Apply `tx` atomically: creations, appends and wakes, nothing else.
    async fn commit_mail(&self, tx: MailTx, label: CommitLabel)
    -> Result<MailCommit, DurableError>;

    /// Read `actor`'s row, unfenced.
    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError>;
}
