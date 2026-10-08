//! Who owns a session's head when a head write arrives from outside a run
//! (FIG-4202, ADR 0105).
//!
//! The bound turn owns the session head. A head commit the session actor
//! writes on its own fenced transaction is the session's own: its epoch
//! fences it, so a turn's commit and a context-pressure frame its turn
//! opens (FIG-5355) pass whatever they name ([`HeadWriter::SessionActor`]).
//! So does a store commit that names the run it commits under, or the
//! command lane's commit that applies the open commands it settles, the
//! owner it would otherwise wait on
//! ([`RuntimeCommit::is_sessions_own_head_write`]). Any other commit is a
//! writer outside every run (a host-scoped service's write): the store
//! refuses it, in the commit's own transaction, while a run owns the head
//! or is owed it, so no such write moves a head a bound run or an open
//! command run was planned against. A host that must move the head
//! submits its write as a session command, which the session applies at a
//! turn boundary.
//!
//! The owners, as the transaction reads them:
//!
//! - an unfinished run: admitted, with its rows bound and no terminal;
//! - an open session command: a command run applies it at the next
//!   boundary.
//!
//! A session's first commit publishes over the created head, which is no
//! head to own (FIG-4099), so creation is never refused.
//!

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
    /// An open session command, the earliest by `enqueue_seq`.
    CommandLane { enqueue_seq: u64 },
}

impl std::fmt::Display for SessionHeadOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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

/// Whether `commit`, written by `writer` onto a head that exists
/// (`head_exists`), must be checked against the head's owners: exactly a
/// write that is not the session's own onto an existing head. The session
/// actor's writes are its own; a store commit is when it names its run or
/// the commands it applies ([`RuntimeCommit::is_sessions_own_head_write`]).
#[must_use]
pub fn head_write_needs_ownership(
    commit: &RuntimeCommit,
    writer: HeadWriter,
    head_exists: bool,
) -> bool {
    writer == HeadWriter::Store && !commit.is_sessions_own_head_write() && head_exists
}

/// Refuse a head write outside every run while `facts` name an owner of
/// the head (FIG-4202). A run outranks the command lane, so the refusal
/// names the owner a host waits on first.
pub fn require_unowned_head(
    session_id: &SessionId,
    facts: HeadOwnershipFacts,
) -> Result<(), StoreError> {
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
                unfinished_run: Some(TurnId::from("run")),
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
