use crate::session_identity::SessionRelation;
pub use crate::session_identity::{SessionCreationHead, SessionStoreCreateRequest};
use crate::session_policy::SessionPolicy;
use crate::{NodeId, SessionId};

/// A durable turn boundary whose continuation checkpoint is currently retained.
///
/// Past turn boundaries are not retained by default. A point remains available
/// while explicitly pinned or while it is the leaf of at least one live
/// session head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkPoint {
    pub node_id: NodeId,
    pub checkpoint_ref: crate::BlobRef,
    /// Provenance of the node, which may name a session that has since been
    /// deleted and is not required to remain readable for a fork.
    pub source_session_id: SessionId,
    /// Provider and model captured by the nearest retained frame boundary.
    pub config: crate::PersistedSessionConfig,
    pub pinned: bool,
}

/// Create a new session head at retained history without writing graph nodes.
#[derive(Clone, Debug)]
pub struct ForkSessionRequest {
    pub session_id: SessionId,
    pub node_id: NodeId,
    pub relation: SessionRelation,
    pub pending_observer_intents: Vec<crate::SessionObserverIntent>,
    pub policy: SessionPolicy,
}

/// Durable identity returned after a zero-node fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkSessionReceipt {
    pub session_id: SessionId,
    pub node_id: NodeId,
    /// Session that originally wrote `node_id`. This is history
    /// provenance, independent of host-declared lineage and observer selection.
    pub source_session_id: SessionId,
    /// Settlement receipts for the host-selected process observer intents.
    pub observed_processes: Vec<crate::session_identity::SessionObservedProcessReceipt>,
}

/// What the catalog holds for one session id
/// ([`SessionCatalogStore::lookup_session`](crate::store::SessionCatalogStore::lookup_session)).
///
/// `Absent` and `Deleted` are answers. A catalog that cannot answer returns
/// `Err`, never `Absent` (ADR 0119's negative-answer rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionLookup {
    /// Durable metadata exists and no deletion tombstone does.
    Live(crate::store::SessionMeta),
    /// The id carries a permanent deletion tombstone.
    Deleted,
    /// The catalog has never held this id.
    Absent,
}
