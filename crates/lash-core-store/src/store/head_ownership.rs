//! Who owns a session's head when a head write arrives from outside a shift
//! (FIG-4202, ADR 0105).
//!
//! The bound turn owns the session head. A head commit that presents a
//! shift fence is the owner's own: the store checks the fence is current and
//! nothing more. A commit that presents none is a writer outside every shift
//! (a dirty park's flush, a host-scoped service's write): the store refuses
//! it, in the commit's own transaction, while a shift owns the head or is
//! owed it, so no such write moves a head a bound run, an owed follow-on or
//! an admitted command run was planned against. A host that must move the
//! head submits its write as a session command, which the shift applies at a
//! turn boundary.
//!
//! The owners, as the transaction reads them:
//!
//! - an unfinished run: admitted, with its rows bound and no terminal;
//! - an owed follow-on on the head (ADR 0101 §3);
//! - an open session command: a command run applies it at the next
//!   boundary, and a command run that sealed and read it holds no binding a
//!   narrower check would see.
//!
//! A session's first commit publishes over the created head, which is no
//! head to own (FIG-4099), so creation is never refused.

use crate::{SessionId, TurnId};

use super::StoreError;

/// What owns a session's head, refusing a head write outside the shift.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "owner", rename_all = "snake_case")]
pub enum SessionHeadOwner {
    /// An admitted run without terminal evidence.
    Run { run: TurnId },
    /// The follow-on the head owes.
    FollowOn { follow_on: TurnId },
    /// An open session command, the earliest by `enqueue_seq`.
    CommandLane { enqueue_seq: u64 },
}

impl std::fmt::Display for SessionHeadOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run { run } => write!(formatter, "unfinished run `{run}`"),
            Self::FollowOn { follow_on } => write!(formatter, "owed follow-on `{follow_on}`"),
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
    /// The follow-on the head owes, if any.
    pub owed_follow_on: Option<TurnId>,
    /// The `enqueue_seq` of the earliest open session command, if any.
    pub open_command: Option<u64>,
}

/// Whether a commit that presents `shift_fence` onto a head that exists
/// (`head_exists`) must be checked against the head's owners: exactly a
/// write outside every shift onto an existing head.
#[must_use]
pub fn head_write_needs_ownership(shift_fence_presented: bool, head_exists: bool) -> bool {
    !shift_fence_presented && head_exists
}

/// The follow-on that owns the head against a write outside every shift:
/// the one the head owes (`owed`) while the commit keeps owing it
/// (`carried`). A commit that settles the follow-on is the follow-on's own,
/// and the commit plan already refused every other commit that would drop
/// it (ADR 0101 §3).
#[must_use]
pub fn follow_on_owning_the_head(
    owed: Option<&super::PendingFollowOn>,
    carried: Option<&super::PendingFollowOn>,
) -> Option<TurnId> {
    let owed = owed?;
    carried
        .is_some_and(|carried| carried.follow_on_turn_id == owed.follow_on_turn_id)
        .then(|| owed.follow_on_turn_id.clone())
}

/// Refuse a head write outside every shift while `facts` name an owner of
/// the head (FIG-4202). A run outranks a follow-on, which outranks the
/// command lane, so the refusal names the owner a host waits on first.
pub fn require_unowned_head(
    session_id: &SessionId,
    facts: HeadOwnershipFacts,
) -> Result<(), StoreError> {
    let owner = if let Some(run) = facts.unfinished_run {
        SessionHeadOwner::Run { run }
    } else if let Some(follow_on) = facts.owed_follow_on {
        SessionHeadOwner::FollowOn { follow_on }
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
                owed_follow_on: Some(TurnId::from("follow-on")),
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
