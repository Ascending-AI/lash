use super::LashCore;
use crate::Result;
use lash_core::store::{ObligationKey, ObligationKind, SessionLookup, StalledObligation};
use lash_core::{EffectOpener, ScopeId, SessionId};
use std::num::NonZeroUsize;
use std::time::Duration;

/// Where a state-based wait for an accepted session deletion ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionDeleteCompletion {
    /// Physical deletion committed. The session id cannot be reused.
    Deleted,
    /// This id has never materialized a session.
    Absent,
    /// The session has no accepted close. This wait starts no deletion.
    NotClosing,
    /// A close, cleanup or physical-delete obligation requires operator re-arm.
    Stalled(Box<StalledObligation>),
}

const POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_secs(1);
const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(63);

impl LashCore {
    /// Await consumption of the session's persisted turn-cancel closure pins.
    ///
    /// After `TurnCancelClosureLifecyclePinned`, wait here before another
    /// close attempt. Reads do not repeat deletion or change the session.
    /// A new closure can race a later close; every close still checks its pins.
    /// This does not wait for effect-group lifecycle pins.
    ///
    /// # Errors
    ///
    /// The catalog's typed store error if it cannot read the pins or lifetime.
    pub async fn await_turn_cancel_closures(&self, session_id: &SessionId) -> Result<()> {
        let mut poll = POLL;
        loop {
            if !matches!(
                self.store_factory.lookup_session(session_id).await?,
                SessionLookup::Live(_)
            ) {
                return Ok(());
            }
            // An accepted close retires any pin its final commit overtook.
            if self
                .store_factory
                .drive_epoch(session_id)
                .await?
                .closing
                .is_some()
                || self
                    .store_factory
                    .pending_turn_cancel_closure_pins(session_id)
                    .await?
                    .is_empty()
            {
                return Ok(());
            }
            tokio::time::sleep(poll).await;
            poll = (poll * 2).min(MAX_POLL);
        }
    }

    /// Await the physical deletion owed by an accepted close (ADR 0109 §4).
    ///
    /// The permanent tombstone proves completion. An absent obligation alone
    /// does not: the close may still be unacknowledged. This reads the store
    /// and its ledgers, makes no engine request, and never retries deletion.
    /// Recovery owns delivery. A stalled dependency returns its typed record
    /// for explicit re-arm instead of keeping the caller waiting.
    ///
    /// There is no elapsed-time completion rule. Dropping the future stops
    /// observation and leaves accepted deletion owed. A host using this inside
    /// a Restate handler must journal the returned observation in its own step.
    ///
    /// # Errors
    ///
    /// A typed store error if the catalog or a dependency ledger cannot answer.
    pub async fn await_session_deletion(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionDeleteCompletion> {
        let mut poll = POLL;
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
            poll = (poll * 2).min(MAX_POLL);
        }
    }

    async fn deletion_completion(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionDeleteCompletion>> {
        match self.store_factory.lookup_session(session_id).await? {
            SessionLookup::Deleted => return Ok(Some(SessionDeleteCompletion::Deleted)),
            SessionLookup::Absent => return Ok(Some(SessionDeleteCompletion::Absent)),
            SessionLookup::Live(_) => {}
        }
        let Some(close_id) = self.store_factory.drive_epoch(session_id).await?.closing else {
            return Ok(Some(SessionDeleteCompletion::NotClosing));
        };
        let own = ScopeId::session(session_id.clone()).storage_id();
        let turns = EffectOpener::session_turn_encoding_range(session_id);
        let drains = EffectOpener::session_operation_encoding_range(session_id);
        for kind in [
            ObligationKind::ControlIntent,
            ObligationKind::ScopeClose,
            ObligationKind::ParentEnd,
            ObligationKind::SessionDelete,
        ] {
            let ledger = self.backend.obligation_ledger(kind);
            let mut after = None;
            loop {
                let page = ledger.list_stalled(after.as_ref(), PAGE).await?;
                let last = page.last().map(|row| row.id.clone());
                let full = page.len() == PAGE.get();
                for row in page {
                    let owned = match &row.key {
                        Ok(ObligationKey::ControlIntent { intent_id }) => *intent_id == close_id,
                        Ok(
                            ObligationKey::ScopeClose {
                                session_id: owner, ..
                            }
                            | ObligationKey::SessionDelete { session_id: owner },
                        ) => owner == session_id,
                        Ok(ObligationKey::ParentEnd {
                            parent_kind,
                            parent_id,
                        }) => match parent_kind.as_str() {
                            "session" => *parent_id == own,
                            "turn" => turns.0 <= *parent_id && *parent_id < turns.1,
                            "session_operation" => drains.0 <= *parent_id && *parent_id < drains.1,
                            _ => false,
                        },
                        _ => false,
                    };
                    if owned {
                        return Ok(Some(SessionDeleteCompletion::Stalled(Box::new(row))));
                    }
                }
                if !full {
                    break;
                }
                after = last;
            }
        }
        Ok(None)
    }
}
