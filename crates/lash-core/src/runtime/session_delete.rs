//! A session's delete on the durable substrate (ADR 0132 §12).
//!
//! A deletion is the session's mail: [`delete_session`] appends a close
//! request to the session actor and returns. The session actor closes
//! itself in its closing state
//! ([`session_close`](crate::runtime::durable::session_close)): it cancels
//! its open turn, revokes its waits, ends its `Until` processes and waits
//! for each to be terminal, deletes its storage (whose
//! transaction arms the `ArtifactCleanup` of what it referred to), deletes
//! its process state and writes its tombstone. Each step is its own fenced
//! transaction, so a crash resumes at the step it interrupted, and nothing
//! about the delete is owed by the caller or by a relay.
//!
//! **No close, no delete.** An id with no session has nothing to close, so
//! its deletion is a no-op (ADR 0049): nothing is written and the id stays
//! creatable. A created session whose first work was never sent has no
//! actor yet: its close request creates the actor with its mail.

use crate::runtime::durable::session_close::{
    SessionCloseRequested, request_first_session_close, request_session_close,
};
use crate::{SessionDeleteContext, SessionId};
use lash_durable::DurableError;

/// What [`delete_session`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionDeletion {
    /// The close request is the session's mail: the session closes itself,
    /// and its tombstone is written when the close's last step commits.
    Requested { session_id: SessionId },
    /// The session's close already finished: a repeated deletion.
    AlreadyDeleted { session_id: SessionId },
    /// The id has no session (ADR 0049): nothing was requested, and the id
    /// stays creatable.
    Absent { session_id: SessionId },
}

/// Delete the session `context` deletes: its close request as session mail.
///
/// # Errors
///
/// The store's refusal; nothing was requested.
pub async fn delete_session(
    context: &SessionDeleteContext<'_>,
) -> Result<SessionDeletion, DurableError> {
    let session_id = context.session_id().clone();
    let backend = context.controller().backend();
    let requested = match request_session_close(backend, &session_id).await? {
        // No actor: a live session whose first work was never sent has
        // none yet, and its close creates it.
        SessionCloseRequested::Absent => match backend
            .session_store_factory()
            .lookup_session(&session_id)
            .await
            .map_err(|error| {
                DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: format!("the session catalog's lookup of {session_id}: {error}"),
                })
            })? {
            crate::store::SessionLookup::Live(_) => {
                request_first_session_close(backend, &session_id).await?
            }
            crate::store::SessionLookup::Absent | crate::store::SessionLookup::Deleted => {
                SessionCloseRequested::Absent
            }
        },
        requested => requested,
    };
    Ok(match requested {
        SessionCloseRequested::Requested => SessionDeletion::Requested { session_id },
        SessionCloseRequested::AlreadyClosed => SessionDeletion::AlreadyDeleted { session_id },
        SessionCloseRequested::Absent => SessionDeletion::Absent { session_id },
    })
}
