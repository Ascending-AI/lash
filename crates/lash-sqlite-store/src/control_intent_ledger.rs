//! The SQLite factory's control-intent ledger (FIG-3600 S7, ADR 0104 O4): a
//! session's close and the lifecycle of every intent's engine half, each in
//! one durable-core transaction. The ledger answers after its session is
//! gone: a deleted session's `CloseSession` intent is kept as its tombstone.

use super::*;
use lash_core_execution::store::{
    ClaimToken, ControlIntent, ControlIntentId, ControlIntentState, ControlIntentStore,
    IntentApplication, IntentSettle, decide_intent_acknowledgement, decide_intent_application,
    decide_intent_failure,
};

use crate::session_roots::{
    begin_session_close_conn, load_intent_conn, settle_intent_claimed_conn, write_intent_state_conn,
};

impl SqliteStore {
    /// A writer on the durable core, or `None` when the catalog was never
    /// created (it then holds no session and no intent).
    pub(crate) async fn control_ledger(&self) -> Result<Option<SqliteConnection>, StoreError> {
        Ok(Some(self.conn.clone()))
    }

    /// Settle intent `id`'s engine half under obligation claim `claim` in
    /// one write transaction: `decide` answers the state to write over the
    /// stored one, or `None` to leave it.
    async fn settle_intent_claimed<F>(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        decide: F,
    ) -> Result<IntentSettle, StoreError>
    where
        F: FnOnce(&ControlIntentState) -> Option<ControlIntentState> + Send + 'static,
    {
        let Some(conn) = self.control_ledger().await? else {
            return Err(StoreError::ControlIntentUnknown { intent: id });
        };
        let claim = claim.clone();
        conn.write_flow(move |tx| {
            Ok(match settle_intent_claimed_conn(tx, id, &claim, decide) {
                Ok(answer) => TxOutcome::Commit(Ok(answer)),
                Err(error) => TxOutcome::Rollback(Err(error)),
            })
        })
        .await
        .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl ControlIntentStore for SqliteStore {
    async fn open_root_intent(
        &self,
        request: &lash_core_execution::store::RootIntentRequest,
        at_ms: u64,
    ) -> Result<ControlIntent, lash_core_execution::store::RootIntentRefused> {
        let Some(conn) = self.control_ledger().await? else {
            return Err(lash_core_execution::store::RootIntentRefused::NotParked);
        };
        let request = request.clone();
        conn.write_flow(move |tx| {
            Ok(
                match crate::root_verbs::open_root_intent_conn(tx, &request, at_ms) {
                    Ok(intent) => TxOutcome::Commit(Ok(intent)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                },
            )
        })
        .await
        .map_err(sqlite_error)?
    }

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
        at_ms: u64,
    ) -> Result<IntentApplication, StoreError> {
        let Some(conn) = self.control_ledger().await? else {
            return Err(StoreError::ControlIntentUnknown { intent: id });
        };
        conn.write_flow(move |tx| {
            let outcome = (|| {
                let stored = load_intent_conn(tx, id)?
                    .ok_or(StoreError::ControlIntentUnknown { intent: id })?;
                // A redrive is decided against the park it would resume, read
                // in this transaction.
                let park = match stored.kind {
                    lash_core_execution::store::ControlIntentKind::Redrive { .. } => {
                        crate::persistence::turn_park::turn_park_conn(tx, &stored.session_id)?
                    }
                    _ => None,
                };
                let application = decide_intent_application(stored.clone(), park.as_ref(), at_ms);
                if application.intent() != &stored
                    && !write_intent_state_conn(tx, &stored, application.intent())?
                {
                    // The write transaction is exclusive: nothing else can
                    // move the row between the read and the write.
                    return Err(StoreError::Contended);
                }
                Ok(application)
            })();
            Ok(match outcome {
                Ok(answer) => TxOutcome::Commit(Ok(answer)),
                Err(error) => TxOutcome::Rollback(Err(error)),
            })
        })
        .await
        .map_err(sqlite_error)?
    }

    async fn acknowledge_intent(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        self.settle_intent_claimed(id, claim, move |stored| {
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
        let error = error.to_string();
        self.settle_intent_claimed(id, claim, move |stored| {
            decide_intent_failure(stored, &error, retryable)
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
