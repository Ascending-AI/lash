//! The port's typed refusals.

use crate::domain::DomainRefusal;
use crate::ids::{ActorKey, CommitLabel, Epoch, NodeId};

/// A transaction refused because its epoch is not the actor's current one.
///
/// The caller is no longer the owner, and its transaction wrote nothing. An
/// owner that sees this drops everything it holds for the actor: its cache
/// is never a grant.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("ownership of {actor} lost: held epoch {held}, current {current:?}")]
pub struct Fenced {
    /// The actor.
    pub actor: ActorKey,
    /// The epoch the caller held.
    pub held: Epoch,
    /// The actor's current epoch, or `None` when no such actor exists.
    pub current: Option<Epoch>,
}

/// Why a [`MailTx`](crate::MailTx) was refused. The whole transaction wrote
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MailRefusal {
    /// It created an actor that already exists.
    #[error("actor {0} already exists")]
    ActorExists(ActorKey),
    /// It wrote to an actor that does not exist.
    #[error("actor {0} does not exist")]
    UnknownActor(ActorKey),
    /// It wrote to an actor that has ended.
    #[error("actor {0} is terminal")]
    ActorTerminal(ActorKey),
}

/// What kind of store failure a [`StoreFailure`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreFailureKind {
    /// Lock contention or a busy database: retrying may succeed.
    Contended,
    /// The database could not be reached or the transaction did not finish.
    /// Whether it committed is unknown.
    Unavailable,
    /// A stored row did not decode: a defect or foreign data.
    Corrupt,
    /// A newer release finalized the store: this build may not write again.
    WriterRetired,
}

/// A failure of the store itself, not a refusal by the port's rules.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("durable store {kind:?}: {message}")]
pub struct StoreFailure {
    /// What kind of failure.
    pub kind: StoreFailureKind,
    /// The driver's account of it.
    pub message: String,
}

/// Everything a [`DurableStore`](crate::DurableStore) call can refuse with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DurableError {
    /// The caller's epoch is stale: it is no longer the owner.
    #[error(transparent)]
    OwnershipLost(Fenced),
    /// The node's lease is gone: it was reaped, released or never
    /// registered. The node must stop serving at once.
    #[error("node {node} has no live lease")]
    NodeLeaseLost {
        /// The node.
        node: NodeId,
    },
    /// The store serves no more nodes than it was configured for, and that
    /// many already listen through it: the node cannot serve here.
    #[error("node {node} exceeds the {served_nodes} served nodes this store is configured for")]
    NodeCapacityExceeded {
        /// The refused node.
        node: NodeId,
        /// The store's configured served-node count.
        served_nodes: u32,
    },
    /// A mailbox transaction broke a mailbox rule.
    #[error(transparent)]
    MailRefused(MailRefusal),
    /// An owner transaction acknowledged mail it never read.
    #[error("{actor} acknowledged mail it never read")]
    AckBeyondRead {
        /// The actor.
        actor: ActorKey,
    },
    /// The commit labelled `label` may have succeeded, but its
    /// acknowledgement was lost. The caller must treat its outcome as
    /// unknown.
    #[error("the acknowledgement of commit `{label}` was lost")]
    AckLost {
        /// The commit.
        label: CommitLabel,
    },
    /// A conditional domain write refused: the whole commit rolled back,
    /// nothing written.
    #[error(transparent)]
    Domain(DomainRefusal),
    /// The store failed.
    #[error(transparent)]
    Store(StoreFailure),
    /// The node's build no longer decodes format sets that `unmigrated`
    /// actors are still in (ADR 0115 §3.5, drain by release): serving, it
    /// would strand them, so it does not start. `command` is the operator
    /// command that has a node of the build before it carry them forward.
    #[error(
        "{unmigrated} actors are still in format sets this build no longer decodes: run `{command}` with a node of the build before this one serving, then start this one"
    )]
    Unmigrated {
        /// How many actors are still in a retired format set.
        unmigrated: u64,
        /// The operator command that carries them forward.
        command: String,
    },
}
