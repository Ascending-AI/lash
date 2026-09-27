//! The PostgreSQL factory's control-intent ledger (FIG-3600 S7, ADR 0104
//! O4): a session's close and the lifecycle of every intent's engine half,
//! each in one transaction. The ledger answers after its session is gone: a
//! deleted session's `CloseSession` intent is kept as its tombstone.

use super::*;
use lash_core_execution::store::{
    ClaimToken, ControlIntent, ControlIntentId, ControlIntentState, ControlIntentStore,
    IntentApplication, IntentSettle, decide_intent_acknowledgement, decide_intent_application,
    decide_intent_failure,
};

use crate::session_roots::{
    begin_session_close_tx, load_intent_conn, settle_intent_claimed_conn, write_intent_state_conn,
};

/// How many times a lifecycle write re-reads an intent another writer moved
/// between its read and its compare-and-set before it reports contention.
const INTENT_WRITE_ATTEMPTS: usize = 8;

impl PostgresSessionStoreFactory {
    /// Settle intent `id`'s engine half under obligation claim `claim`:
    /// read it, compare the claim, decide, and compare-and-set the decision
    /// in one transaction, re-reading when another writer moved the row
    /// first. `decide` answers the state to write over the stored one, or
    /// `None` to leave it.
    async fn settle_intent_claimed(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        decide: impl Fn(&ControlIntentState) -> Option<ControlIntentState> + Send + Sync,
    ) -> Result<IntentSettle, StoreError> {
        for _ in 0..INTENT_WRITE_ATTEMPTS {
            let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
            let Some(answer) = settle_intent_claimed_conn(&mut tx, id, claim, &decide).await?
            else {
                continue;
            };
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(answer);
        }
        Err(StoreError::Contended)
    }
}

#[async_trait::async_trait]
impl ControlIntentStore for PostgresSessionStoreFactory {
    async fn open_root_intent(
        &self,
        request: &lash_core_execution::store::RootIntentRequest,
        at_ms: u64,
    ) -> Result<ControlIntent, lash_core_execution::store::RootIntentRefused> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let intent = crate::root_verbs::open_root_intent_tx(&mut tx, request, at_ms).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(intent)
    }

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
        at_ms: u64,
    ) -> Result<IntentApplication, StoreError> {
        for _ in 0..INTENT_WRITE_ATTEMPTS {
            let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
            let stored = load_intent_conn(&mut tx, id)
                .await?
                .ok_or(StoreError::ControlIntentUnknown { intent: id })?;
            // A redrive is decided against the park it would resume, read
            // under its row lock in this transaction.
            let park = match stored.kind {
                lash_core_execution::store::ControlIntentKind::Redrive { .. } => {
                    crate::runtime_persistence::turn_park::turn_park_for_update(
                        &mut tx,
                        &stored.session_id,
                    )
                    .await?
                }
                _ => None,
            };
            let application = decide_intent_application(stored.clone(), park.as_ref(), at_ms);
            if application.intent() != &stored
                && !write_intent_state_conn(&mut tx, &stored, application.intent()).await?
            {
                continue;
            }
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(application);
        }
        Err(StoreError::Contended)
    }

    async fn acknowledge_intent(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        self.settle_intent_claimed(id, claim, |stored| {
            decide_intent_acknowledgement(stored, at_ms)
        })
        .await
    }

    async fn record_intent_failure(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        error: &str,
        retryable: bool,
        _at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        self.settle_intent_claimed(id, claim, |stored| {
            decide_intent_failure(stored, error, retryable)
        })
        .await
    }

    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        let mut connection = crate::acquire_runtime_connection(&self.pool).await?;
        load_intent_conn(&mut connection, id).await
    }
}
