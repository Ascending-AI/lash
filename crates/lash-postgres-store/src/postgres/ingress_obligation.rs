//! The PostgreSQL ingress ledger (ADR 0109 §3, FIG-3851): the turn-input
//! table's and the queued-batch table's obligation ledgers, composed.
//!
//! Admission arms the row it inserts inside its own transaction, under the
//! id [`ingress_obligation_id`] derives from the row, so the producer
//! attempts delivery right after its commit.

use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock};

use lash_core_execution::SessionId;
use lash_core_execution::store::ingress_obligation::{
    DueObligationPeek, IngressLedger, IngressTable, ingress_obligation_id,
};
use lash_core_execution::store::{ObligationId, ObligationKey, ObligationKind, ObligationLedger};
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use lash_store_sql::turn_ingress::pending_inputs::PendingTurnInputObligationStatements;
use lash_store_sql::turn_ingress::queued_batches::QueuedBatchObligationStatements;
use lash_store_sql::{Dialect, Rendered};
use sqlx::PgPool;

use crate::StoreError;
use crate::obligation_ledger::{PostgresObligationLedger, arm_table_tx};
use crate::support::store_sqlx_error;

static TURN_INPUTS: LazyLock<PendingTurnInputObligationStatements> =
    LazyLock::new(|| PendingTurnInputObligationStatements::render(Dialect::postgres()));
static QUEUED_BATCHES: LazyLock<QueuedBatchObligationStatements> =
    LazyLock::new(|| QueuedBatchObligationStatements::render(Dialect::postgres()));

/// The turn-input table's shared statements and its locking due read.
pub(crate) fn turn_input_sql() -> (ObligationSql<'static>, &'static str) {
    (
        TURN_INPUTS.obligation_sql(),
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .obligation_select_due_locking
            .sql(),
    )
}

fn queued_batch_sql() -> (ObligationSql<'static>, &'static str) {
    (
        QUEUED_BATCHES.obligation_sql(),
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches_postgres
            .obligation_select_due_locking
            .sql(),
    )
}

/// Arm the turn input `input_id` of `session_id` as its ingress obligation,
/// due at `now_ms`, inside its admission transaction.
pub(crate) async fn arm_turn_input_tx(
    conn: &mut sqlx::PgConnection,
    session_id: &SessionId,
    input_id: &str,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    arm_tx(conn, turn_input_sql().0, session_id, input_id, now_ms).await
}

/// Arm the queued batch `batch_id` of `session_id` as its ingress obligation,
/// due at `now_ms`, inside its admission transaction.
pub(crate) async fn arm_queued_batch_tx(
    conn: &mut sqlx::PgConnection,
    session_id: &SessionId,
    batch_id: &str,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    arm_tx(conn, queued_batch_sql().0, session_id, batch_id, now_ms).await
}

/// Arm the ingress row `key` names — a turn input or a queued batch — as
/// its ingress obligation, due at `now_ms`. `None` when neither table holds
/// the row or it already carries an obligation.
pub(crate) async fn arm_ingress_tx(
    conn: &mut sqlx::PgConnection,
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
    match arm_turn_input_tx(conn, session_id, item_id, now_ms).await? {
        Some(id) => Ok(Some(id)),
        None => arm_queued_batch_tx(conn, session_id, item_id, now_ms).await,
    }
}

async fn arm_tx(
    conn: &mut sqlx::PgConnection,
    sql: ObligationSql<'static>,
    session_id: &SessionId,
    item_id: &str,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    let key = ObligationKey::Ingress {
        session_id: session_id.clone(),
        item_id: item_id.to_owned(),
    };
    arm_table_tx(conn, sql, &key, ingress_obligation_id(item_id), now_ms).await
}

/// One table's due read, through its `obligation_peek_due` statement.
struct PostgresDuePeek {
    sql: &'static Rendered,
    pool: PgPool,
}

#[async_trait::async_trait]
impl DueObligationPeek for PostgresDuePeek {
    async fn peek_due(
        &self,
        now_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<(u64, ObligationId)>, StoreError> {
        let rows: Vec<(i64, String)> = sqlx::query_as(self.sql.sql())
            .bind(i64::try_from(now_ms).unwrap_or(i64::MAX))
            .bind(i64::try_from(limit.get()).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        rows.into_iter()
            .map(|(due_at, id)| {
                Ok((
                    u64::try_from(due_at).map_err(|_| StoreError::StoredDataCorrupt {
                        record_kind: "obligation",
                        message: "a negative due instant".to_owned(),
                    })?,
                    ObligationId::new(id),
                ))
            })
            .collect()
    }
}

/// The ingress ledger over `pool`.
pub(crate) fn ingress_ledger(pool: &PgPool) -> Arc<dyn ObligationLedger> {
    let table = |(sql, locking): (ObligationSql<'static>, &'static str),
                 peek: &'static Rendered| IngressTable {
        ledger: Arc::new(PostgresObligationLedger::over_table(
            ObligationKind::Ingress,
            sql,
            locking,
            pool.clone(),
        )),
        peek: Arc::new(PostgresDuePeek {
            sql: peek,
            pool: pool.clone(),
        }),
    };
    Arc::new(IngressLedger::new(vec![
        table(turn_input_sql(), &TURN_INPUTS.obligation_peek_due),
        table(queued_batch_sql(), &QUEUED_BATCHES.obligation_peek_due),
    ]))
}
