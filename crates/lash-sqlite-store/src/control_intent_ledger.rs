//! The SQLite factory's control-intent ledger (FIG-3600 S7, ADR 0104 O4): a
//! session's close and the lifecycle of every intent's engine half, each in
//! one durable-core transaction. The ledger answers after its session is
//! gone: a deleted session's `CloseSession` intent is kept as its tombstone.

use super::*;
use lash_core_execution::store::{
    ControlIntent, ControlIntentId, ControlIntentStore, IntentApplication,
    decide_intent_acknowledgement, decide_intent_application, decide_intent_failure,
};

use crate::session_roots::{begin_session_close_conn, load_intent_conn, write_intent_state_conn};

impl SqliteSessionStoreFactory {
    /// A writer on the durable core, or `None` when the catalog was never
    /// created (it then holds no session and no intent).
    async fn control_ledger(&self) -> Result<Option<SqliteConnection>, StoreError> {
        if !self.core.target().exists() {
            return Ok(None);
        }
        let conn =
            SqliteConnection::open_with_policy(self.core.target(), self.options.connection_policy)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore)
            .await
            .map_err(sqlite_error)?;
        Ok(Some(conn))
    }

    /// Apply `write` to intent `id`'s stored record in one write
    /// transaction: `write` answers the record to store, or `None` to leave
    /// it.
    async fn rewrite_intent<T, F>(&self, id: ControlIntentId, write: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(ControlIntent) -> (Option<ControlIntent>, T) + Send + 'static,
    {
        let Some(conn) = self.control_ledger().await? else {
            return Err(StoreError::ControlIntentUnknown { intent: id });
        };
        conn.write_flow(move |tx| {
            let outcome = (|| {
                let stored = load_intent_conn(tx, id)?
                    .ok_or(StoreError::ControlIntentUnknown { intent: id })?;
                let (rewritten, answer) = write(stored.clone());
                if let Some(rewritten) = rewritten
                    && !write_intent_state_conn(tx, &stored, &rewritten)?
                {
                    // The write transaction is exclusive: nothing else can
                    // move the row between the read and the write.
                    return Err(StoreError::Contended);
                }
                Ok(answer)
            })();
            Ok(match outcome {
                Ok(answer) => TxOutcome::Commit(Ok(answer)),
                Err(error) => TxOutcome::Rollback(Err(error)),
            })
        })
        .await
        .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl ControlIntentStore for SqliteSessionStoreFactory {
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
        self.rewrite_intent(id, move |mut stored| {
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
        let error = error.to_string();
        self.rewrite_intent(id, move |mut stored| {
            match decide_intent_failure(&stored.state, &error, retryable) {
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
        let Some(conn) = self.control_ledger().await? else {
            return Ok(None);
        };
        conn.read(move |conn| Ok(load_intent_conn(conn, id)))
            .await
            .map_err(sqlite_error)?
    }
}
