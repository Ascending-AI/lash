use crate::SessionId;
use crate::session_identity::SessionRelation;

/// Coarse durable relation carried by a host-facing session view.
///
/// The view deliberately projects only the relation shape and immediate
/// parent. Causal details and fork anchors remain part of the full session
/// metadata loaded when a host opens one session.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionRelationKind {
    #[default]
    Root,
    Child,
    Fork,
}

impl SessionRelationKind {
    pub fn from_relation(relation: &SessionRelation) -> Self {
        match relation {
            SessionRelation::Root => Self::Root,
            SessionRelation::Child { .. } => Self::Child,
            SessionRelation::Fork { .. } => Self::Fork,
        }
    }
}

/// Where a catalogued session is in its life, with exactly the relation
/// evidence that stage keeps.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionEntry {
    /// The session accepts work. `relation` is its complete recorded
    /// relation.
    Live { relation: SessionRelation },
    /// The session's close has begun: it refuses new work, and its physical
    /// delete is still owed. Its metadata is still stored.
    Closing { relation: SessionRelation },
    /// The session is deleted. Its tombstone keeps only the coarse relation
    /// and the immediate parent.
    Deleted {
        kind: SessionRelationKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<SessionId>,
    },
}

/// Read-only catalog projection for one durable session id.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SessionView {
    pub session_id: SessionId,
    pub created_at_ms: u64,
    /// Time of the most recent settled runtime commit, or `None` before the
    /// first commit.
    pub last_commit_at_ms: Option<u64>,
    pub head_revision: u64,
    pub entry: SessionEntry,
}

impl SessionView {
    /// The coarse relation shape, which every stage keeps.
    #[must_use]
    pub fn relation_kind(&self) -> SessionRelationKind {
        match &self.entry {
            SessionEntry::Live { relation } | SessionEntry::Closing { relation } => {
                SessionRelationKind::from_relation(relation)
            }
            SessionEntry::Deleted { kind, .. } => *kind,
        }
    }

    /// The complete recorded relation; `None` for a deletion tombstone,
    /// which keeps only the coarse shape and the parent.
    #[must_use]
    pub fn relation(&self) -> Option<&SessionRelation> {
        match &self.entry {
            SessionEntry::Live { relation } | SessionEntry::Closing { relation } => Some(relation),
            SessionEntry::Deleted { .. } => None,
        }
    }

    /// The immediate parent, when the session is a child.
    #[must_use]
    pub fn parent_session_id(&self) -> Option<&SessionId> {
        match &self.entry {
            SessionEntry::Live { relation } | SessionEntry::Closing { relation } => {
                match relation {
                    SessionRelation::Child {
                        parent_session_id, ..
                    } => Some(parent_session_id),
                    SessionRelation::Root | SessionRelation::Fork { .. } => None,
                }
            }
            SessionEntry::Deleted { parent, .. } => parent.as_ref(),
        }
    }

    /// Whether the session is deleted.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        matches!(self.entry, SessionEntry::Deleted { .. })
    }
}

/// Conjunctive filters for durable session enumeration.
///
/// Absence means no restriction. In particular, the default includes both
/// live sessions and permanent deletion tombstones.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SessionListFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<SessionRelationKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    /// Restrict to sessions whose recorded `caused_by` equals this reference —
    /// for example `CausalRef::Process` selects the sessions a process caused.
    /// Tombstones carry no durable relation, so they never match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
}

impl SessionListFilter {
    pub fn matches(&self, view: &SessionView) -> bool {
        self.relation
            .is_none_or(|relation| relation == view.relation_kind())
            && self
                .deleted
                .is_none_or(|deleted| deleted == view.is_deleted())
            && self.caused_by.as_ref().is_none_or(|caused_by| {
                matches!(
                    view.relation(),
                    Some(SessionRelation::Child {
                        caused_by: recorded,
                        ..
                    }) if recorded.as_ref() == Some(caused_by)
                )
            })
    }
}
