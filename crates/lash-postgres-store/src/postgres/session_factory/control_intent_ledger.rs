//! The PostgreSQL factory's control-intent ledger (FIG-3600 S7): a session's
//! close, in one transaction that also wakes the session actor. The ledger
//! answers after its session is gone: a deleted session's `CloseSession`
//! intent is kept as its tombstone.

use super::*;
use lash_core_execution::store::{ControlIntent, ControlIntentId, ControlIntentStore};

use crate::session_runs::{begin_session_close_tx, close_session_intent_conn, load_intent_conn};

#[async_trait::async_trait]
impl ControlIntentStore for PostgresStore {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        let intent = begin_session_close_tx(&mut tx, session_id, at_ms).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(intent)
    }

    async fn session_close_intent(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ControlIntent>, StoreError> {
        let mut connection = crate::acquire_runtime_connection(&self.pool, &self.observer).await?;
        close_session_intent_conn(&mut connection, session_id).await
    }

    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        let mut connection = crate::acquire_runtime_connection(&self.pool, &self.observer).await?;
        load_intent_conn(&mut connection, id).await
    }
}
