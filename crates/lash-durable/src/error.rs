//! The port's typed refusals.

use crate::domain::DomainRefusal;
use crate::ids::{ActorKey, CommitLabel, Epoch, NodeId, StateRevision};

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

pub use lash_core_store::store_failure::{StoreFailure, StoreFailureKind};

/// Everything a [`DurableStore`](crate::DurableStore) call can refuse with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DurableError {
    #[error("serving requires owner_retention")]
    MissingOwnerRetention,
    /// All resident slots are occupied. Retry when an owner releases one.
    #[error("owner {actor} cannot activate: {resident} owners are resident (capacity {capacity})")]
    OwnerCapacityExceeded {
        actor: ActorKey,
        resident: usize,
        capacity: usize,
    },
    /// One mailbox item exceeds the configured external-read size limit.
    #[error("mail for {actor} has {bytes} bytes (limit {limit})")]
    MailboxItemTooLarge {
        actor: ActorKey,
        bytes: u64,
        limit: u64,
    },
    /// The caller's epoch is stale: it is no longer the owner.
    #[error(transparent)]
    OwnershipLost(Fenced),
    /// The grant still holds, but this draft was derived from older state.
    /// Nothing was written; discard the draft and reload before replanning.
    #[error(
        "revision conflict for {actor} at epoch {epoch}: expected {expected:?}, actual {actual:?}"
    )]
    RevisionConflict {
        actor: ActorKey,
        epoch: Epoch,
        expected: StateRevision,
        actual: StateRevision,
    },
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
    /// A necessary mailbox body cannot fit in the requested page.
    #[error("mailbox body for {actor} has {bytes} bytes, exceeding page limit {limit}")]
    MailPageTooSmall {
        actor: ActorKey,
        bytes: u64,
        limit: u64,
    },
    /// A mailbox transaction broke a mailbox rule.
    #[error(transparent)]
    MailRefused(MailRefusal),
    /// Repeatable first-use discovery failed without recording a terminal pin.
    #[error("resolution discovery failed: {message}")]
    ResolutionDiscovery { message: String },

    /// A process receipt did not contain exactly its complete acknowledged view.
    #[error("invalid process acknowledgement for {actor}: {message}")]
    ProcessPostimage {
        actor: crate::ActorKey,
        message: String,
    },

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
    /// A stored record this build refuses to read, with the store's typed
    /// refusal: a session state generation it does not serve, say. No
    /// retry clears it (FIG-6209).
    #[error(transparent)]
    StoreRefused(lash_core_store::store::StoreRefusal),
    /// The commit's checkpoint graph, or a graph read, was refused: a node
    /// that is not canonical, a reference to a node neither staged nor
    /// stored, a root over its bound, a commit over its budget, a read past
    /// its limits. The whole commit rolled back.
    #[error("checkpoint graph refused: {0}")]
    Graph(lash_core_store::checkpoint_graph::GraphRefusal),
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
    /// A first resolution raced another state change. Reopen and retry.
    #[error("resolution owner {actor} is at revision {current:?}, expected {expected:?}")]
    ResolutionRevisionConflict {
        actor: ActorKey,
        expected: crate::StateRevision,
        current: crate::StateRevision,
    },
    /// An immutable package record or execution registration was refused.
    #[error("package record refused: {0}")]
    Package(#[from] crate::packages::PackageRefusal),
}

impl From<lash_core_store::checkpoint_graph::GraphRefusal> for DurableError {
    /// A graph refusal as the port reports it: a store failure keeps its
    /// kind, so a transient one stays retryable.
    fn from(refusal: lash_core_store::checkpoint_graph::GraphRefusal) -> Self {
        match refusal {
            lash_core_store::checkpoint_graph::GraphRefusal::Backend(failure) => {
                Self::Store(failure)
            }
            refusal => Self::Graph(refusal),
        }
    }
}
