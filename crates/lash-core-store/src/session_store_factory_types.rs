use crate::session_identity::SessionRelation;
pub use crate::session_identity::{SessionCreationHead, SessionStoreCreateRequest};
use crate::{NodeId, SessionId};

/// What a host names to pin or to fork from: an accepted input, a turn or a
/// head revision.
///
/// The name of a state is `(session_id, head_revision)`. The other two
/// targets resolve to one through rows the session already keeps: an input
/// names the root that applied it, and a turn (a logical root) names the
/// revision its terminal commit published. A merged or re-deferred input
/// therefore resolves to the root that actually applied it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Target {
    /// The root that applies this accepted input.
    Input(crate::InputId),
    /// This logical root.
    Turn(crate::TurnId),
    /// This head revision of the session.
    Revision(u64),
}

impl Target {
    /// The stored `target_kind` spelling.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Input(_) => "input",
            Self::Turn(_) => "turn",
            Self::Revision(_) => "revision",
        }
    }

    /// The stored `target_id` spelling. A revision is its decimal text.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Input(input) => input.to_string(),
            Self::Turn(turn) => turn.to_string(),
            Self::Revision(revision) => revision.to_string(),
        }
    }

    /// Decode a stored `(target_kind, target_id)` pair.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`](crate::StoreError::StoredDataCorrupt)
    /// for an unknown kind or a revision that is not a decimal number.
    pub fn from_stored(kind: &str, id: &str) -> Result<Self, crate::StoreError> {
        let corrupt = |message: String| crate::StoreError::StoredDataCorrupt {
            record_kind: "Pin",
            message,
        };
        match kind {
            "input" => Ok(Self::Input(crate::InputId::parse(id)?)),
            "turn" => Ok(Self::Turn(crate::TurnId::parse(id)?)),
            "revision" => id
                .parse()
                .map(Self::Revision)
                .map_err(|_| corrupt(format!("revision pin `{id}` is not a head revision"))),
            other => Err(corrupt(format!("unknown pin target kind `{other}`"))),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(input) => write!(f, "input `{input}`"),
            Self::Turn(turn) => write!(f, "turn `{turn}`"),
            Self::Revision(revision) => write!(f, "revision {revision}"),
        }
    }
}

/// What a session keeps besides its head and its pins. The host states it
/// when the session is created and may change it afterwards; there is no
/// default (D-DEFAULTS2), and it is never part of the session's replayed
/// config.
///
/// A pin always retains the revision it resolves to, and a session's head is
/// always retained, whatever the policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "turns", rename_all = "snake_case")]
pub enum Retention {
    /// Every revision stays forkable until the host collects
    /// ([`StoreMaintenance::gc_unreachable`](crate::store::StoreMaintenance::gc_unreachable)).
    /// A collection keeps only the head and the pinned revisions.
    UntilGc,
    /// The revisions the last `n` terminal roots published are kept, through
    /// collections too. Only a root whose terminal commit published a
    /// revision counts; one that ended without a commit has none to keep.
    /// Older unpinned revisions are released as the session commits.
    LastTurns(std::num::NonZeroU32),
    /// Only the head and the pinned revisions are kept. Every other revision
    /// is released as the session commits.
    HeadOnly,
}

impl Retention {
    /// The stored `retention_kind` spelling.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::UntilGc => "until_gc",
            Self::LastTurns(_) => "last_turns",
            Self::HeadOnly => "head_only",
        }
    }

    /// The stored `retention_last_turns` window, for [`Self::LastTurns`].
    #[must_use]
    pub fn last_turns(self) -> Option<u32> {
        match self {
            Self::LastTurns(turns) => Some(turns.get()),
            Self::UntilGc | Self::HeadOnly => None,
        }
    }

    /// Whether a commit releases revisions itself. [`Self::UntilGc`] leaves
    /// every release to the host's collection, so its commits read no pin.
    #[must_use]
    pub fn releases_at_commit(self) -> bool {
        !matches!(self, Self::UntilGc)
    }

    /// Decode the stored `(retention_kind, retention_last_turns)` pair.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`](crate::StoreError::StoredDataCorrupt)
    /// for an unknown kind or a window that disagrees with it.
    pub fn from_stored(kind: &str, last_turns: Option<i64>) -> Result<Self, crate::StoreError> {
        let corrupt = |message: String| crate::StoreError::StoredDataCorrupt {
            record_kind: "Retention",
            message,
        };
        match (kind, last_turns) {
            ("until_gc", None) => Ok(Self::UntilGc),
            ("head_only", None) => Ok(Self::HeadOnly),
            ("last_turns", Some(turns)) => u32::try_from(turns)
                .ok()
                .and_then(std::num::NonZeroU32::new)
                .map(Self::LastTurns)
                .ok_or_else(|| corrupt(format!("last_turns window {turns} is not positive"))),
            (kind, window) => Err(corrupt(format!(
                "retention kind `{kind}` with window {window:?}"
            ))),
        }
    }
}

/// A head revision a session still retains: a point it can be forked at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedRevision {
    pub session_id: SessionId,
    pub head_revision: u64,
    /// The leaf the revision published. A revision published before the
    /// session's first node has none.
    pub leaf_node_id: Option<NodeId>,
    /// The checkpoint root the revision published, once the session has one.
    pub checkpoint_ref: Option<crate::BlobRef>,
    /// The configuration a fork of this revision records: the one the head
    /// recorded when the revision was published.
    pub config: crate::PersistedSessionConfig,
    /// Whether this is the session's head.
    pub head: bool,
    /// The pins that resolve to this revision.
    pub pinned_by: Vec<Target>,
}

/// Create a new session head at a retained revision without writing graph
/// nodes.
#[derive(Clone, Debug)]
pub struct ForkSessionRequest {
    pub session_id: SessionId,
    /// The session whose revision is forked.
    pub source_session_id: SessionId,
    /// The revision of `source_session_id` the fork starts at.
    pub head_revision: u64,
    pub relation: SessionRelation,
    pub pending_observer_intents: Vec<crate::SessionObserverIntent>,
    /// The config the fork records: the forked revision's recorded config in
    /// full, model, execution controls, generation, tool access and plugin
    /// configuration alike (FIG-4594). It is the new
    /// session's own head, so its `config_revision` starts at `0`.
    pub config: crate::PersistedSessionConfig,
    /// Which revisions the fork keeps besides its head and its pins, as
    /// its creator states it. A fork records its own retention; it does not
    /// inherit its source's.
    pub retention: Retention,
}

impl RetainedRevision {
    /// The config a fork of this revision records: everything the revision's
    /// head recorded, at the new session's first config revision. Nothing
    /// of the deployment that forks stands in for any of it (FIG-4594). A
    /// change the source's observers are owed stays the source's: the fork
    /// owes its own observers nothing (FIG-5397).
    pub fn fork_config(&self) -> crate::PersistedSessionConfig {
        crate::PersistedSessionConfig {
            config_revision: 0,
            undelivered_change: None,
            ..self.config.clone()
        }
    }
}

/// Durable identity returned after a zero-node fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkSessionReceipt {
    pub session_id: SessionId,
    /// The session whose revision was forked. This is the state's own name,
    /// independent of host-declared lineage and observer selection.
    pub source_session_id: SessionId,
    /// The revision of `source_session_id` the fork started at.
    pub head_revision: u64,
    /// The leaf the fork's head points at. A fork of a revision with no
    /// history has none.
    pub leaf_node_id: Option<NodeId>,
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
