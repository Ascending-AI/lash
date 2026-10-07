//! Durable catalog and session-head records.
use super::*;

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub relation: crate::SessionRelation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_observer_intents: Vec<crate::SessionObserverIntent>,
    /// The process that runs this session as its own, recorded at creation
    /// (FIG-3607 R1): see [`SessionStoreCreateRequest::owning_process_id`](crate::SessionStoreCreateRequest::owning_process_id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_process_id: Option<crate::ProcessId>,
}

impl SessionMeta {
    /// Returns the parent session id, if any, derived from the canonical
    /// [`SessionRelation`](crate::SessionRelation) field.
    pub fn parent_session_id(&self) -> Option<&SessionId> {
        self.relation.parent_session_id()
    }
}

/// Outcome of admitting a session binding to a persistence handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAdmission {
    /// The admission durably created the session metadata row.
    Created,
    /// The handle was already durably bound to the same live session.
    Rebound,
}

pub fn validate_session_id(session_id: &SessionId) -> Result<(), StoreError> {
    if !namespace::is_valid_opaque_key(session_id) {
        Err(StoreError::InvalidSessionId {
            reason: "session ids must not contain NUL",
        })
    } else {
        Ok(())
    }
}

#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct BlobRef(pub String);

impl BlobRef {
    pub fn for_content(content: &[u8]) -> Self {
        Self(crate::stable_hash::blake3_hex(
            LASH_BLOB_DOMAIN_VERSION,
            content,
        ))
    }

    /// Exposes the opaque durable blob reference to store implementors for backend round-tripping
    /// without imposing path or URL semantics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BlobRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// JSON-owned fields persisted in a revision's `head_json` column.
///
/// Revision and graph/checkpoint references live in dedicated columns and are
/// deliberately absent from this serializable payload.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHeadPayload {
    pub schema_version: u32,
    #[serde(default = "default_root_session_id")]
    pub session_id: SessionId,
    pub config: crate::PersistedSessionConfig,
}

/// Fully assembled session-head metadata returned by a store.
///
/// This type is intentionally not serializable. Store implementations follow the
/// head pointer to its revision, decode its [`SessionHeadPayload`], and supply
/// that revision's references and the leaf-derived frame through [`Self::assemble`].
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionHeadMeta {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    /// Derived from the head leaf row when loading metadata.
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    pub checkpoint_ref: Option<BlobRef>,
    pub leaf_node_id: Option<crate::NodeId>,
}

impl SessionHeadMeta {
    /// The head a creating admission writes beside the catalog row, in the
    /// same store transaction (FIG-4099): the creator's config at config
    /// revision `0`, and nothing else — no frame, no checkpoint, no leaf, at
    /// head revision `0`. The session's first commit publishes over it like
    /// over an absent head, and every open before that reads the config the
    /// creator stated instead of re-deriving one.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    pub fn created(
        session_id: &SessionId,
        mut config: crate::PersistedSessionConfig,
        fleet_format: FleetFormat,
    ) -> Self {
        config.config_revision = 0;
        Self {
            schema_version: fleet_format
                .writer_version(crate::surface_format!(SESSION_HEAD_META_SCHEMA_VERSION)),
            session_id: session_id.clone(),
            head_revision: 0,
            config,
            current_frame_node_id: None,
            checkpoint_ref: None,
            leaf_node_id: None,
        }
    }

    /// Whether this head is the created head [`Self::created`] writes: the
    /// creator's config at head revision `0`, with no commit published over
    /// it yet. The first commit treats it like an absent head.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    pub fn is_created(&self) -> bool {
        self.head_revision == 0 && self.leaf_node_id.is_none() && self.checkpoint_ref.is_none()
    }

    /// The session's identity is owned by the row key the caller bound the
    /// query to, never by the payload: `session_id` is taken from
    /// `session_id` and the payload's copy is a checked redundancy. A payload
    /// naming a different session is refused as corrupt stored data rather
    /// than adopted, so a mis-keyed or hand-edited `head_json` can no longer
    /// rewrite a session's live identity.
    ///
    /// This remains public because external stores assemble rows from their
    /// own columns. Callers still enforce node derivation before assembly; the
    /// constructor cannot validate that fact from this projection alone.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    pub fn assemble(
        session_id: &SessionId,
        payload: SessionHeadPayload,
        head_revision: u64,
        checkpoint_ref: Option<BlobRef>,
        leaf_node_id: Option<crate::NodeId>,
        current_frame_node_id: Option<crate::FrameNodeId>,
    ) -> Result<Self, StoreError> {
        if payload.session_id != *session_id {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: format!(
                    "head_json names session `{}` but the row is keyed on session `{}`",
                    payload.session_id.as_str(),
                    session_id.as_str()
                ),
            });
        }
        Ok(Self {
            schema_version: payload.schema_version,
            session_id: session_id.clone(),
            head_revision,
            config: payload.config,
            current_frame_node_id,
            checkpoint_ref,
            leaf_node_id,
        })
    }

    /// Project the exact value that may be serialized into `head_json`.
    pub fn payload(&self) -> SessionHeadPayload {
        SessionHeadPayload {
            schema_version: self.schema_version,
            session_id: self.session_id.clone(),
            config: self.config.clone(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl Default for SessionHeadPayload {
    fn default() -> Self {
        Self {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: default_root_session_id(),
            config: crate::PersistedSessionConfig::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ),
        }
    }
}

impl crate::store::DurableRecord for SessionHeadPayload {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::SESSION_HEAD_META_SCHEMA_VERSION);
}

/// A session head a run names: the state generation, the head revision, the
/// leaf of its graph and its checkpoint (ADR 0105 §2, §9).
///
/// A commit names the head it expects. An admission names the head it was
/// admitted on, its base: a replay of the admitted turn rebuilds the turn's
/// input state from this reference, never from the live head, which the turn's
/// own commit or a lane service may have advanced since (FIG-3682). `leaf` and
/// `checkpoint` are `None` for a session with no committed graph or checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionHeadRef {
    pub generation: u32,
    pub revision: u64,
    pub leaf: Option<crate::NodeId>,
    pub checkpoint: Option<BlobRef>,
}

impl SessionHeadRef {
    /// Whether `head` is this head: the same revision, leaf and checkpoint.
    /// The generation is the store's, not the head row's, so it is compared
    /// by the caller that read it.
    #[must_use]
    pub fn names_head(&self, head: &SessionHeadMeta) -> bool {
        self.revision == head.head_revision
            && self.leaf == head.leaf_node_id
            && self.checkpoint == head.checkpoint_ref
    }
}
