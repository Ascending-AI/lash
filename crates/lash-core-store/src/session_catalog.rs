use crate::SessionId;
use crate::session_identity::SessionRelation;

/// Coarse durable relation carried by a host-facing session summary.
///
/// The summary deliberately projects only the relation shape and immediate
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

/// Read-only catalog projection for one durable session id.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub created_at_ms: u64,
    /// Time of the most recent settled runtime commit, or `None` before the
    /// first commit.
    pub last_commit_at_ms: Option<u64>,
    pub head_revision: u64,
    /// Coarse relation shape retained for filtering and deletion tombstones.
    pub relation: SessionRelationKind,
    /// Complete relation from durable session metadata, when that metadata was
    /// available to this catalog projection.
    ///
    /// `None` means the projection had no durable session metadata. Deletion
    /// tombstones retain only [`Self::relation`] and
    /// [`Self::parent_session_id`], so they always return `None` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_relation: Option<SessionRelation>,
    pub parent_session_id: Option<SessionId>,
    pub deleted: bool,
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
    pub fn matches(&self, summary: &SessionSummary) -> bool {
        self.relation
            .is_none_or(|relation| relation == summary.relation)
            && self
                .deleted
                .is_none_or(|deleted| deleted == summary.deleted)
            && self.caused_by.as_ref().is_none_or(|caused_by| {
                matches!(
                    &summary.durable_relation,
                    Some(SessionRelation::Child {
                        caused_by: recorded,
                        ..
                    }) if recorded.as_ref() == Some(caused_by)
                )
            })
    }
}
