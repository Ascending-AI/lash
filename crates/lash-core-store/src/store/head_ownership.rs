//! Head writes from outside the session actor require an unclaimed actor.
//! The mutation transaction proves this independently of run/command identity
//! and head materialization. Own actor writes use their owner fence instead.

use crate::{SessionId, TurnId};

use super::{RuntimeCommit, StoreError};

/// What owns a session's head, refusing a head write outside every run.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "owner", rename_all = "snake_case")]
pub enum SessionHeadOwner {
    /// An admitted run without terminal evidence.
    Run { run: TurnId },
    /// The session actor currently holds its grant.
    Actor { epoch: i64 },
    /// An open session command, the earliest by `enqueue_seq`.
    CommandLane { enqueue_seq: u64 },
}

impl std::fmt::Display for SessionHeadOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Actor { epoch } => write!(formatter, "session actor at epoch `{epoch}`"),
            Self::Run { run } => write!(formatter, "unfinished run `{run}`"),
            Self::CommandLane { enqueue_seq } => {
                write!(formatter, "the open session command at `{enqueue_seq}`")
            }
        }
    }
}

/// What a backend read in a head commit's transaction about who owns the
/// session head.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadOwnershipFacts {
    /// The session's unfinished run, if one is admitted.
    pub unfinished_run: Option<TurnId>,
    /// The claimed epoch, only while the actor is owned.
    pub claimed_epoch: Option<i64>,
    /// The `enqueue_seq` of the earliest open session command, if any.
    pub open_command: Option<u64>,
}

/// Who writes a head commit, as the transaction applying it proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadWriter {
    /// The session store's own commit, outside every actor's fence.
    Store,
    /// The session's own actor, on its fenced owner transaction.
    SessionActor,
}

/// Whether the run/command guard applies, in addition to the unconditional
/// actor guard on every fresh Store mutation. Run/command identity only
/// exempts its own lane on a materialized head; it never grants actor authority.
#[must_use]
pub fn head_write_needs_ownership(
    commit: &RuntimeCommit,
    writer: HeadWriter,
    head_exists: bool,
) -> bool {
    writer == HeadWriter::Store && !commit.is_sessions_own_head_write() && head_exists
}

/// Refuse every fresh direct mutation while the session actor is claimed.
pub fn require_unclaimed_actor(
    session_id: &SessionId,
    facts: &HeadOwnershipFacts,
) -> Result<(), StoreError> {
    match facts.claimed_epoch {
        Some(epoch) => Err(StoreError::SessionHeadOwned {
            session_id: session_id.clone(),
            owner: SessionHeadOwner::Actor { epoch },
        }),
        None => Ok(()),
    }
}

/// Refuse a head write outside every run while `facts` name an owner of
/// the head. A claimed actor outranks a run, which outranks the command lane.
pub fn require_unowned_head(
    session_id: &SessionId,
    facts: HeadOwnershipFacts,
) -> Result<(), StoreError> {
    require_unclaimed_actor(session_id, &facts)?;
    let owner = if let Some(run) = facts.unfinished_run {
        SessionHeadOwner::Run { run }
    } else if let Some(enqueue_seq) = facts.open_command {
        SessionHeadOwner::CommandLane { enqueue_seq }
    } else {
        return Ok(());
    };
    Err(StoreError::SessionHeadOwned {
        session_id: session_id.clone(),
        owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_owner_a_host_waits_on_is_named() {
        let error = require_unowned_head(
            &SessionId::from("s"),
            HeadOwnershipFacts {
                claimed_epoch: Some(9),
                unfinished_run: Some(TurnId::from("run")),
                open_command: Some(7),
            },
        )
        .expect_err("actor authority precedes run and command identity");
        assert!(matches!(
            error,
            StoreError::SessionHeadOwned {
                owner: SessionHeadOwner::Actor { epoch: 9 },
                ..
            }
        ));
        let error = require_unowned_head(
            &SessionId::from("s"),
            HeadOwnershipFacts {
                unfinished_run: Some(TurnId::from("run")),
                claimed_epoch: None,
                open_command: Some(7),
            },
        )
        .expect_err("a bound run owns the head");
        assert!(matches!(
            error,
            StoreError::SessionHeadOwned {
                owner: SessionHeadOwner::Run { .. },
                ..
            }
        ));
        let error = require_unowned_head(
            &SessionId::from("s"),
            HeadOwnershipFacts {
                open_command: Some(7),
                ..HeadOwnershipFacts::default()
            },
        )
        .expect_err("an open command owns the head");
        assert!(matches!(
            error,
            StoreError::SessionHeadOwned {
                owner: SessionHeadOwner::CommandLane { enqueue_seq: 7 },
                ..
            }
        ));
    }
}
