//! The SQLite ingress ledger (ADR 0109 §3, FIG-3851): the turn-input table's
//! and the queued-batch table's obligation ledgers, composed.
//!
//! Admission arms the row it inserts inside its own `BEGIN IMMEDIATE`
//! transaction, under the id [`ingress_obligation_id`] derives from the row,
//! so the producer attempts delivery right after its commit.

use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock};

use lash_core_execution::store::ingress_obligation::{
    DueObligationPeek, IngressLedger, IngressTable, ingress_obligation_id,
};
use lash_core_execution::store::{ObligationId, ObligationKey, ObligationKind, ObligationLedger};
use lash_store_sql::Rendered;
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use lash_store_sql::turn_ingress::pending_inputs::PendingTurnInputObligationStatements;
use lash_store_sql::turn_ingress::queued_batches::QueuedBatchObligationStatements;

use crate::conn::SqliteConnection;
use crate::obligation_ledger::{SqliteObligationLedger, arm_table_tx};
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

static TURN_INPUTS: LazyLock<PendingTurnInputObligationStatements> =
    LazyLock::new(|| PendingTurnInputObligationStatements::render(Schema::Main.dialect()));
static QUEUED_BATCHES: LazyLock<QueuedBatchObligationStatements> =
    LazyLock::new(|| QueuedBatchObligationStatements::render(Schema::Main.dialect()));

/// The turn-input table's obligation statements.
pub(crate) fn turn_input_sql() -> ObligationSql<'static> {
    TURN_INPUTS.obligation_sql()
}

fn queued_batch_sql() -> ObligationSql<'static> {
    QUEUED_BATCHES.obligation_sql()
}

/// Arm the turn input `input_id` of `session_id` as its ingress obligation,
/// due at `now_ms`, inside its admission transaction.
pub(crate) fn arm_turn_input_tx(
    tx: &rusqlite::Connection,
    session_id: &lash_core_execution::SessionId,
    input_id: &str,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    arm_tx(tx, turn_input_sql(), session_id, input_id, now_ms)
}

/// Arm the queued batch `batch_id` of `session_id` as its ingress obligation,
/// due at `now_ms`, inside its admission transaction.
pub(crate) fn arm_queued_batch_tx(
    tx: &rusqlite::Connection,
    session_id: &lash_core_execution::SessionId,
    batch_id: &str,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    arm_tx(tx, queued_batch_sql(), session_id, batch_id, now_ms)
}

/// Arm the ingress row `key` names — a turn input or a queued batch — as
/// its ingress obligation, due at `now_ms`. `None` when neither table holds
/// the row or it already carries an obligation.
pub(crate) fn arm_ingress_tx(
    tx: &rusqlite::Connection,
    key: &ObligationKey,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    let ObligationKey::Ingress {
        session_id,
        item_id,
    } = key
    else {
        return Ok(None);
    };
    match arm_turn_input_tx(tx, session_id, item_id, now_ms)? {
        Some(id) => Ok(Some(id)),
        None => arm_queued_batch_tx(tx, session_id, item_id, now_ms),
    }
}

fn arm_tx(
    tx: &rusqlite::Connection,
    sql: ObligationSql<'static>,
    session_id: &lash_core_execution::SessionId,
    item_id: &str,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    let key = ObligationKey::Ingress {
        session_id: session_id.clone(),
        item_id: item_id.to_owned(),
    };
    arm_table_tx(tx, sql, &key, ingress_obligation_id(item_id), now_ms)
}

/// One table's due read, through its `obligation_peek_due` statement.
struct SqliteDuePeek {
    sql: &'static Rendered,
    conn: SqliteConnection,
}

#[async_trait::async_trait]
impl DueObligationPeek for SqliteDuePeek {
    async fn peek_due(
        &self,
        now_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<(u64, ObligationId)>, StoreError> {
        let sql = self.sql;
        let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let rows: Vec<(i64, String)> = self
            .conn
            .call(move |conn| {
                let mut select = conn.prepare(sql.sql())?;
                select
                    .query_map(rusqlite::params![now, limit], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(|(due_at, id)| {
                Ok((
                    u64::try_from(due_at)
                        .map_err(|_| stored_data_corrupt("obligation", "a negative due instant"))?,
                    ObligationId::new(id),
                ))
            })
            .collect()
    }
}

/// The ingress ledger over the durable core `conn` is open on.
pub(crate) fn ingress_ledger(conn: &SqliteConnection) -> Arc<dyn ObligationLedger> {
    let table = |sql: ObligationSql<'static>, peek: &'static Rendered| IngressTable {
        ledger: Arc::new(SqliteObligationLedger::over_table(
            ObligationKind::Ingress,
            sql,
            conn.clone(),
        )),
        peek: Arc::new(SqliteDuePeek {
            sql: peek,
            conn: conn.clone(),
        }),
    };
    Arc::new(IngressLedger::new(vec![
        table(turn_input_sql(), &TURN_INPUTS.obligation_peek_due),
        table(queued_batch_sql(), &QUEUED_BATCHES.obligation_peek_due),
    ]))
}
