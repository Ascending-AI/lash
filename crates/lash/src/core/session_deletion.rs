use super::LashCore;
use crate::EmbedError;
use crate::Result;
use lash_core::SessionId;
use lash_core::store::SessionLookup;

/// Where a state-based wait for a requested session deletion ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionDeleteCompletion {
    /// The session's close wrote its tombstone. The session id cannot be
    /// reused.
    Deleted,
    /// This id has never materialized a session.
    Absent,
    /// The session has no close. This wait starts no deletion.
    NotClosing,
}

impl LashCore {
    /// Await the tombstone of a requested deletion (ADR 0132 §12).
    ///
    /// The session actor closes itself one durable step at a time; its
    /// tombstone proves completion. This reads the store, makes no request,
    /// and never retries the deletion: the session's own close resumes after
    /// any crash.
    ///
    /// There is no elapsed-time completion rule. Dropping the future stops
    /// observation and leaves the requested close running.
    ///
    /// # Errors
    ///
    /// A typed store error if the catalog or the durable store cannot answer.
    pub async fn await_session_deletion(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionDeleteCompletion> {
        let pacing = self.observer_pacing.deletion;
        let mut poll = pacing.initial();
        loop {
            let observed = self.deletion_completion(session_id).await;
            match observed {
                Ok(Some(completion)) => return Ok(completion),
                Ok(None) => {}
                Err(error) => {
                    // Physical deletion can remove metadata between reads.
                    // Only its retained tombstone turns that race into success.
                    if matches!(
                        self.store_factory.lookup_session(session_id).await?,
                        SessionLookup::Deleted
                    ) {
                        return Ok(SessionDeleteCompletion::Deleted);
                    }
                    return Err(error);
                }
            }
            tokio::time::sleep(poll).await;
            poll = pacing.next(poll);
        }
    }

    async fn deletion_completion(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionDeleteCompletion>> {
        let lookup = self.store_factory.lookup_session(session_id).await?;
        if matches!(lookup, SessionLookup::Deleted) {
            return Ok(Some(SessionDeleteCompletion::Deleted));
        }
        let close = lash_core::runtime::durable::session_close::session_close_state(
            &self.backend,
            session_id,
        )
        .await
        .map_err(EmbedError::from)?;
        Ok(match close {
            Some(close) if close.is_tombstone() => Some(SessionDeleteCompletion::Deleted),
            // The session actor is still closing itself.
            Some(_) => None,
            // A close request is mail until the session actor drains it: while
            // the actor holds undrained mail, a close may still begin.
            None if self.has_undrained_mail(session_id).await? => None,
            None => Some(match lookup {
                SessionLookup::Absent => SessionDeleteCompletion::Absent,
                SessionLookup::Live(_) | SessionLookup::Deleted => {
                    SessionDeleteCompletion::NotClosing
                }
            }),
        })
    }

    /// Whether `session_id`'s actor holds mail it has not drained.
    async fn has_undrained_mail(&self, session_id: &SessionId) -> Result<bool> {
        let Ok(actor) = lash_core::durable_port::ActorKey::session(session_id.as_str()) else {
            return Ok(false);
        };
        let snapshot = self
            .backend
            .durable()
            .actor(&actor)
            .await
            .map_err(EmbedError::from)?;
        Ok(snapshot.is_some_and(|snapshot| snapshot.pending_mail > 0))
    }
}
