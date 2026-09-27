//! The store half of a session's two-phase delete (ADR 0109 §4).
//!
//! A deletion first closes the session: its `CloseSession` intent commits,
//! the session refuses new work, and the intent's engine half releases the
//! session's roots and closes its scopes. The intent's acknowledgement arms
//! the session's `SessionDelete` obligation on its `session_meta` row, in the
//! same transaction. That obligation's delivery is the physical delete, and
//! it runs only once every cleanup obligation the close left behind — the
//! scope close of each root, the parent-end plan of each scope the session
//! owns — has been delivered: a finalizer.
//!
//! The ledger here answers the two reads the delete's relay makes that the
//! kind-generic [`ObligationLedger`](super::ObligationLedger) cannot: which
//! obligation a session's delete is, and how much of its cleanup is still
//! owed.

use crate::SessionId;

use super::StoreError;
use super::obligation::{ObligationId, ObligationState};

/// The cleanup obligations of one session still owed: armed and not
/// delivered (due, claimed, or stalled).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCleanup {
    /// Scope-close obligations on the session's roots.
    pub scope_close: u64,
    /// Parent-end obligations on the plans of scopes the session owns: the
    /// session's own scope, its turns' and its queue drains'.
    pub parent_end: u64,
}

impl SessionCleanup {
    /// Whether nothing is owed: the physical delete may run.
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.scope_close == 0 && self.parent_end == 0
    }
}

impl std::fmt::Display for SessionCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} scope-close and {} parent-end obligation(s) undelivered",
            self.scope_close, self.parent_end
        )
    }
}

/// A session's `SessionDelete` obligation, as its `session_meta` row
/// carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDeleteObligation {
    pub id: ObligationId,
    pub state: ObligationState,
}

/// The reads a session delete's relay makes (ADR 0109 §4).
#[async_trait::async_trait]
pub trait SessionDeleteLedger: Send + Sync {
    /// The `SessionDelete` obligation armed on `session_id`'s `session_meta`
    /// row. `None` when the row owes nothing yet (its close is not
    /// acknowledged) or is gone (the physical delete ran).
    async fn delete_obligation(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionDeleteObligation>, StoreError>;

    /// The cleanup obligations `session_id`'s delete still waits on.
    async fn undelivered_cleanup(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionCleanup, StoreError>;
}
