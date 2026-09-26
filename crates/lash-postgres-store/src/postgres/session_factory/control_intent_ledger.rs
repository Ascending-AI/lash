//! The PostgreSQL factory's control-intent ledger (FIG-3600 S7, ADR 0104
//! O4): a session's close and the lifecycle of every intent's engine half,
//! each in one transaction. The ledger answers after its session is gone: a
//! deleted session's `CloseSession` intent is kept as its tombstone.

use super::*;
use lash_core_execution::store::{
    ControlIntent, ControlIntentId, ControlIntentStore, IntentApplication,
    decide_intent_acknowledgement, decide_intent_application, decide_intent_failure,
};

use crate::session_roots::{begin_session_close_tx, load_intent_conn, write_intent_state_conn};

/// How many times a lifecycle write re-reads an intent another writer moved
/// between its read and its compare-and-set before it reports contention.
const INTENT_WRITE_ATTEMPTS: usize = 8;

impl PostgresSessionStoreFactory {
    /// Apply `write` to intent `id`'s stored record: read it, decide, and
    /// compare-and-set the decision in one transaction, re-reading when
    /// another writer moved the row first. `write` answers the record to
    /// store, or `None` to leave it.
    async fn rewrite_intent<T>(
        &self,
        id: ControlIntentId,
        write: impl Fn(ControlIntent) -> (Option<ControlIntent>, T) + Send + Sync,
    ) -> Result<T, StoreError> {
        for _ in 0..INTENT_WRITE_ATTEMPTS {
            let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
            let stored = load_intent_conn(&mut tx, id)
                .await?
                .ok_or(StoreError::ControlIntentUnknown { intent: id })?;
            let (rewritten, answer) = write(stored.clone());
            if let Some(rewritten) = rewritten
                && !write_intent_state_conn(&mut tx, &stored, &rewritten).await?
            {
                continue;
            }
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(answer);
        }
        Err(StoreError::Contended)
    }
}

#[async_trait::async_trait]
impl ControlIntentStore for PostgresSessionStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let intent = begin_session_close_tx(&mut tx, session_id, at_ms).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(intent)
    }

    async fn claim_intent_application(
        &self,
        id: ControlIntentId,
    ) -> Result<IntentApplication, StoreError> {
        self.rewrite_intent(id, |stored| {
            let application = decide_intent_application(stored);
            let rewritten = matches!(application, IntentApplication::Apply(_))
                .then(|| application.intent().clone());
            (rewritten, application)
        })
        .await
    }

    async fn acknowledge_intent(&self, id: ControlIntentId, at_ms: u64) -> Result<(), StoreError> {
        self.rewrite_intent(id, |mut stored| {
            let rewritten = decide_intent_acknowledgement(&stored.state, at_ms).map(|state| {
                stored.state = state;
                stored
            });
            (rewritten, ())
        })
        .await
    }

    async fn record_intent_failure(
        &self,
        id: ControlIntentId,
        error: &str,
        retryable: bool,
        _at_ms: u64,
    ) -> Result<ControlIntent, StoreError> {
        self.rewrite_intent(id, |mut stored| {
            match decide_intent_failure(&stored.state, error, retryable) {
                Some(state) => {
                    stored.state = state;
                    (Some(stored.clone()), stored)
                }
                None => (None, stored),
            }
        })
        .await
    }

    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        let mut connection = crate::acquire_runtime_connection(&self.pool).await?;
        load_intent_conn(&mut connection, id).await
    }
}
