//! How a fork target resolves to a head revision.
//!
//! A target's state is computed by query and stored nowhere. Each backend
//! reads the rows a target resolves through, and these functions decide what
//! the rows say, so both backends answer one target the same way.

use crate::session_store_factory_types::Target;
use crate::store::RunTerminal;
use crate::turn_input_vocabulary::TurnInputStateKind;
use crate::{SessionId, StoreError};

/// What the rows a target resolves through say about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetResolution {
    /// The target names this head revision.
    Revision(u64),
    /// The target's run has not finished, or nothing has recorded the
    /// target yet.
    Pending,
    /// The target names no state: its run ended without a commit, or its
    /// input was withdrawn.
    Unavailable,
}

impl TargetResolution {
    /// A revision target against the session's head revision: a revision the
    /// session has not published yet is pending.
    #[must_use]
    pub fn of_revision(revision: u64, head_revision: u64) -> Self {
        if revision > head_revision {
            Self::Pending
        } else {
            Self::Revision(revision)
        }
    }

    /// A turn target against its run's terminal evidence: no evidence is a
    /// run that has not finished, and evidence without a head revision is a
    /// run that ended without a commit.
    #[must_use]
    pub fn of_run(terminal: Option<&RunTerminal>) -> Self {
        match terminal {
            None => Self::Pending,
            Some(terminal) => match terminal.head_revision {
                Some(revision) => Self::Revision(revision),
                None => Self::Unavailable,
            },
        }
    }

    /// An input target no run is bound to, against the input's stored
    /// lifecycle state: a settled input that reached no run was withdrawn,
    /// and an open or unrecorded one may still be applied.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`] for a state no build spells.
    pub fn of_unbound_input(state: Option<&str>) -> Result<Self, StoreError> {
        let Some(state) = state else {
            return Ok(Self::Pending);
        };
        let kind = TurnInputStateKind::from_wire_str(state).ok_or_else(|| {
            StoreError::StoredDataCorrupt {
                record_kind: "PendingTurnInput",
                message: format!("unknown turn input state `{state}`"),
            }
        })?;
        Ok(if kind.is_terminal() {
            Self::Unavailable
        } else {
            Self::Pending
        })
    }

    /// The revision the target names, or its typed refusal. A refusal never
    /// answers another revision in the target's place.
    ///
    /// # Errors
    ///
    /// [`StoreError::ForkTargetPending`] or
    /// [`StoreError::ForkTargetUnavailable`].
    pub fn revision(self, session_id: &SessionId, target: &Target) -> Result<u64, StoreError> {
        match self {
            Self::Revision(revision) => Ok(revision),
            Self::Pending => Err(StoreError::ForkTargetPending {
                session_id: session_id.clone(),
                target: target.clone(),
            }),
            Self::Unavailable => Err(StoreError::ForkTargetUnavailable {
                session_id: session_id.clone(),
                target: target.clone(),
            }),
        }
    }
}
