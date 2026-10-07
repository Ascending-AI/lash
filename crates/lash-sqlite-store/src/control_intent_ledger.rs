//! The SQLite factory's control-intent ledger (FIG-3600 S7): a session's
//! close, in one durable-core transaction that also wakes the session actor.
//! The ledger answers after its session is gone: a deleted session's
//! `CloseSession` intent is kept as its tombstone.

use super::*;
use lash_core_execution::store::{ControlIntent, ControlIntentId, ControlIntentStore};

use crate::session_runs::{begin_session_close_conn, close_session_intent_conn, load_intent_conn};

impl SqliteStore {
    /// A writer on the durable core, or `None` when the catalog was never
    /// created (it then holds no session and no intent).
    pub(crate) async fn control_ledger(&self) -> Result<Option<SqliteConnection>, StoreError> {
        Ok(Some(self.conn.clone()))
    }
}

#[async_trait::async_trait]
impl ControlIntentStore for SqliteStore {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let Some(conn) = self.control_ledger().await? else {
            return Ok(None);
        };
        let session_id = session_id.clone();
        conn.write_flow(move |tx| {
            Ok(match begin_session_close_conn(tx, &session_id, at_ms) {
                Ok(intent) => TxOutcome::Commit(Ok(intent)),
                Err(error) => TxOutcome::Rollback(Err(error)),
            })
        })
        .await
        .map_err(sqlite_error)?
    }

    async fn session_close_intent(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ControlIntent>, StoreError> {
        let Some(conn) = self.control_ledger().await? else {
            return Ok(None);
        };
        let session_id = session_id.clone();
        conn.read(move |conn| Ok(close_session_intent_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        let Some(conn) = self.control_ledger().await? else {
            return Ok(None);
        };
        conn.read(move |conn| Ok(load_intent_conn(conn, id)))
            .await
            .map_err(sqlite_error)?
    }
}
