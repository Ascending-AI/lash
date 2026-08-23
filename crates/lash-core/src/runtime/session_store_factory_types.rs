use crate::{SessionPolicy, SessionRelation};

#[derive(Clone)]
pub struct SessionStoreCreateRequest {
    pub session_id: String,
    pub relation: SessionRelation,
    pub policy: SessionPolicy,
}

impl SessionStoreCreateRequest {
    /// Exposes the parent session ID to session-store factories for child and fork relations,
    /// returning `None` for a root session.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.relation.parent_session_id()
    }
}

/// A durable turn boundary whose continuation checkpoint is currently retained.
///
/// Past turn boundaries are not retained by default. A point remains available
/// while explicitly pinned or while it is the leaf of at least one live
/// session head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkPoint {
    pub node_id: String,
    pub checkpoint_ref: crate::BlobRef,
    /// Provenance of the node, which may name a session that has since been
    /// deleted and is not required to remain readable for a fork.
    pub source_session_id: String,
    /// Provider and model captured by the nearest retained frame boundary.
    pub config: crate::PersistedSessionConfig,
    pub pinned: bool,
}

/// Create a new session head at retained history without writing graph nodes.
#[derive(Clone, Debug)]
pub struct ForkSessionRequest {
    pub session_id: String,
    pub node_id: String,
    pub relation: SessionRelation,
    pub policy: SessionPolicy,
}

/// Durable identity returned after a zero-node fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkSessionReceipt {
    pub session_id: String,
    pub node_id: String,
    /// Session that originally wrote `node_id`. This is process-observer
    /// provenance, not a required source-session argument to the fork.
    pub source_session_id: String,
}
