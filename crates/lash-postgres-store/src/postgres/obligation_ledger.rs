//! The PostgreSQL obligation ledgers (ADR 0109 §1.3): one generic ledger over
//! each table's shared obligation statements.
//!
//! Every deployment claims due obligations here (ADR 0109 §1.7). A due claim
//! is one transaction that reads the due page `FOR UPDATE SKIP LOCKED` — so
//! two deployments' relays take disjoint rows — and claims each row with its
//! compare-and-set.

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use lash_core_execution::store::{
    ClaimToken, ClaimedObligation, KeyColumn, KeyColumnType, ObligationId, ObligationKey,
    ObligationKind, ObligationLedger, ObligationSettlement, ObligationState, SettleOutcome,
    StallReason, StalledObligation, UndecodableObligation,
};
use lash_store_sql::Dialect;
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use lash_store_sql::process::parent_end_plans::ParentEndPlanObligationStatements;
use lash_store_sql::process::processes::ProcessObligationStatements;
use lash_store_sql::session::meta::SessionMetaObligationStatements;
use lash_store_sql::session_ingress::SessionIngressObligationStatements;
use lash_store_sql::session_roots::control_intents::ControlIntentObligationStatements;
use lash_store_sql::session_roots::roots::SessionRootObligationStatements;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row};

use crate::StoreError;
use crate::process_sql::{
    ParentEndPlanObligationPostgresStatements, ProcessObligationPostgresStatements,
};
use crate::session_ingress::SessionIngressObligationPostgresStatements;
use crate::session_roots::{
    ControlIntentObligationPostgresStatements, SessionRootObligationPostgresStatements,
};
use crate::session_sql::SessionMetaObligationPostgresStatements;
use crate::support::store_sqlx_error;

/// One ledger's statements: the shared set and PostgreSQL's locking due read.
struct LedgerSql<S, P> {
    shared: S,
    locking: P,
}

static INGRESS: LazyLock<
    LedgerSql<SessionIngressObligationStatements, SessionIngressObligationPostgresStatements>,
> = LazyLock::new(|| LedgerSql {
    shared: SessionIngressObligationStatements::render(Dialect::postgres()),
    locking: SessionIngressObligationPostgresStatements::render(Dialect::postgres()),
});
static INTENTS: LazyLock<
    LedgerSql<ControlIntentObligationStatements, ControlIntentObligationPostgresStatements>,
> = LazyLock::new(|| LedgerSql {
    shared: ControlIntentObligationStatements::render(Dialect::postgres()),
    locking: ControlIntentObligationPostgresStatements::render(Dialect::postgres()),
});
static ROOTS: LazyLock<
    LedgerSql<SessionRootObligationStatements, SessionRootObligationPostgresStatements>,
> = LazyLock::new(|| LedgerSql {
    shared: SessionRootObligationStatements::render(Dialect::postgres()),
    locking: SessionRootObligationPostgresStatements::render(Dialect::postgres()),
});
static META: LazyLock<
    LedgerSql<SessionMetaObligationStatements, SessionMetaObligationPostgresStatements>,
> = LazyLock::new(|| LedgerSql {
    shared: SessionMetaObligationStatements::render(Dialect::postgres()),
    locking: SessionMetaObligationPostgresStatements::render(Dialect::postgres()),
});
static PLANS: LazyLock<
    LedgerSql<ParentEndPlanObligationStatements, ParentEndPlanObligationPostgresStatements>,
> = LazyLock::new(|| LedgerSql {
    shared: ParentEndPlanObligationStatements::render(Dialect::postgres()),
    locking: ParentEndPlanObligationPostgresStatements::render(Dialect::postgres()),
});
static PROCESSES: LazyLock<
    LedgerSql<ProcessObligationStatements, ProcessObligationPostgresStatements>,
> = LazyLock::new(|| LedgerSql {
    shared: ProcessObligationStatements::render(Dialect::postgres()),
    locking: ProcessObligationPostgresStatements::render(Dialect::postgres()),
});

/// `kind`'s shared statements and its locking due read.
fn obligation_sql(kind: ObligationKind) -> (ObligationSql<'static>, &'static str) {
    match kind {
        ObligationKind::Ingress => (
            INGRESS.shared.obligation_sql(),
            INGRESS.locking.obligation_select_due_locking.sql(),
        ),
        ObligationKind::ControlIntent => (
            INTENTS.shared.obligation_sql(),
            INTENTS.locking.obligation_select_due_locking.sql(),
        ),
        ObligationKind::ScopeClose => (
            ROOTS.shared.obligation_sql(),
            ROOTS.locking.obligation_select_due_locking.sql(),
        ),
        ObligationKind::SessionDelete => (
            META.shared.obligation_sql(),
            META.locking.obligation_select_due_locking.sql(),
        ),
        ObligationKind::ParentEnd => (
            PLANS.shared.obligation_sql(),
            PLANS.locking.obligation_select_due_locking.sql(),
        ),
        ObligationKind::ProcessTerminal => (
            PROCESSES.shared.obligation_sql(),
            PROCESSES.locking.obligation_select_due_locking.sql(),
        ),
    }
}

fn sql_i64(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

fn corrupt(message: &'static str) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: "obligation",
        message: message.to_owned(),
    }
}

/// The key columns of `kind` read from `row` starting at column `first`.
fn read_key(
    kind: ObligationKind,
    row: &PgRow,
    first: usize,
) -> Result<Result<ObligationKey, UndecodableObligation>, StoreError> {
    let mut columns = Vec::with_capacity(kind.key_column_types().len());
    for (offset, column_type) in kind.key_column_types().iter().enumerate() {
        let index = first + offset;
        columns.push(match column_type {
            KeyColumnType::Text => match row.try_get::<String, _>(index) {
                Ok(text) => KeyColumn::Text(text),
                Err(error) => {
                    return Ok(Err(UndecodableObligation {
                        detail: format!("{kind} obligation key column {offset}: {error}"),
                    }));
                }
            },
            KeyColumnType::Integer => match row.try_get::<i64, _>(index) {
                Ok(integer) => KeyColumn::Integer(integer),
                Err(error) => {
                    return Ok(Err(UndecodableObligation {
                        detail: format!("{kind} obligation key column {offset}: {error}"),
                    }));
                }
            },
        });
    }
    Ok(ObligationKey::decode(kind, columns))
}

fn read_claim(
    kind: ObligationKind,
    row: &PgRow,
    token: &ClaimToken,
) -> Result<ClaimedObligation, StoreError> {
    let attempts: i32 = row.try_get(1).map_err(store_sqlx_error)?;
    Ok(ClaimedObligation {
        id: ObligationId::new(row.try_get::<String, _>(0).map_err(store_sqlx_error)?),
        token: token.clone(),
        attempts: u32::try_from(attempts).map_err(|_| corrupt("a negative attempt count"))?,
        key: read_key(kind, row, 2)?,
    })
}

fn key_query<'q>(
    mut query: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    key: &ObligationKey,
) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
    for column in key.columns() {
        query = match column {
            KeyColumn::Text(text) => query.bind(text),
            KeyColumn::Integer(integer) => query.bind(integer),
        };
    }
    query
}

/// Arm `key`'s row as a fresh obligation due at `now_ms` inside a producer's
/// own transaction: the helper a slice's producer calls on its transaction.
/// `None` when the row is missing or already carries an obligation.
pub(crate) async fn arm_obligation_tx(
    conn: &mut sqlx::PgConnection,
    key: &ObligationKey,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    let id = ObligationId::mint(key.kind());
    arm_obligation_id_tx(conn, key, &id, now_ms)
        .await
        .map(|armed| armed.map(|_| id))
}

/// [`arm_obligation_tx`] with the id the row's own transaction derived (ADR
/// 0109 §1.1): a producer that must name its obligation afterwards — the
/// terminal write naming its scope close — arms the id it derived rather
/// than a minted one.
pub(crate) async fn arm_obligation_id_tx(
    conn: &mut sqlx::PgConnection,
    key: &ObligationKey,
    id: &ObligationId,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    let kind = key.kind();
    let (sql, _) = obligation_sql(kind);
    let changed = key_query(sqlx::query(sql.arm.sql()), key)
        .bind(id.as_str())
        .bind(sql_i64("obligation due instant", now_ms)?)
        .execute(conn)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    Ok((changed == 1).then(|| id.clone()))
}

/// One PostgreSQL ledger: `kind`'s statements over the store's pool.
#[derive(Clone)]
pub(crate) struct PostgresObligationLedger {
    kind: ObligationKind,
    pool: PgPool,
}

impl PostgresObligationLedger {
    pub(crate) fn new(kind: ObligationKind, pool: PgPool) -> Self {
        Self { kind, pool }
    }
}

#[async_trait::async_trait]
impl ObligationLedger for PostgresObligationLedger {
    fn kind(&self) -> ObligationKind {
        self.kind
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        if key.kind() != self.kind {
            return Err(StoreError::Backend(format!(
                "a {} key cannot arm the {} ledger",
                key.kind(),
                self.kind
            )));
        }
        let mut conn = crate::acquire_runtime_connection(&self.pool).await?;
        arm_obligation_tx(&mut conn, key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let (sql, locking) = obligation_sql(self.kind);
        let now = sql_i64("obligation claim instant", now_ms)?;
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let token = ClaimToken::mint();
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let ids: Vec<String> = sqlx::query_scalar(locking)
            .bind(now)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut claimed = Vec::with_capacity(ids.len());
        for id in ids {
            let row = sqlx::query(sql.claim_due_row.sql())
                .bind(&id)
                .bind(token.as_str())
                .bind(until)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            if let Some(row) = row {
                claimed.push(read_claim(self.kind, &row, &token)?);
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(claimed)
    }

    async fn claim(
        &self,
        id: &ObligationId,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let (sql, _) = obligation_sql(self.kind);
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let token = ClaimToken::mint();
        let row = sqlx::query(sql.claim.sql())
            .bind(id.as_str())
            .bind(token.as_str())
            .bind(until)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        row.map(|row| read_claim(self.kind, &row, &token))
            .transpose()
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        let (sql, _) = obligation_sql(self.kind);
        let now = sql_i64("obligation settle instant", now_ms)?;
        let query = match settlement {
            ObligationSettlement::Delivered => sqlx::query(sql.settle_delivered.sql())
                .bind(id.as_str())
                .bind(token.as_str())
                .bind(now),
            ObligationSettlement::Retry { due_at_ms, error } => sqlx::query(sql.settle_retry.sql())
                .bind(id.as_str())
                .bind(token.as_str())
                .bind(sql_i64("obligation due instant", due_at_ms)?)
                .bind(error),
            ObligationSettlement::Stall { reason, error } => sqlx::query(sql.settle_stall.sql())
                .bind(id.as_str())
                .bind(token.as_str())
                .bind(reason.as_str())
                .bind(error)
                .bind(now),
        };
        let changed = query
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        Ok(if changed == 1 {
            SettleOutcome::Applied
        } else {
            SettleOutcome::ClaimLost
        })
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        let (sql, _) = obligation_sql(self.kind);
        let changed = sqlx::query(sql.rearm.sql())
            .bind(id.as_str())
            .bind(sql_i64("obligation due instant", now_ms)?)
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        Ok(changed == 1)
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        let (sql, _) = obligation_sql(self.kind);
        let rows = sqlx::query(sql.select_stalled.sql())
            .bind(after.map_or("", ObligationId::as_str))
            .bind(i64::try_from(limit.get()).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let attempts: i32 = row.try_get(1).map_err(store_sqlx_error)?;
                let reason: String = row.try_get(2).map_err(store_sqlx_error)?;
                let stalled_at: i64 = row.try_get(4).map_err(store_sqlx_error)?;
                Ok(StalledObligation {
                    kind: self.kind,
                    id: ObligationId::new(row.try_get::<String, _>(0).map_err(store_sqlx_error)?),
                    key: read_key(self.kind, row, 5)?,
                    reason: StallReason::from_label(&reason)?,
                    attempts: u32::try_from(attempts)
                        .map_err(|_| corrupt("a negative attempt count"))?,
                    last_error: row.try_get(3).map_err(store_sqlx_error)?,
                    stalled_at_ms: u64::try_from(stalled_at)
                        .map_err(|_| corrupt("a negative stall instant"))?,
                })
            })
            .collect()
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        let (sql, _) = obligation_sql(self.kind);
        let count: i64 = sqlx::query_scalar(sql.count_stalled.sql())
            .fetch_one(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        u64::try_from(count).map_err(|_| corrupt("a negative count"))
    }

    async fn state(&self, id: &ObligationId) -> Result<Option<ObligationState>, StoreError> {
        let (sql, _) = obligation_sql(self.kind);
        let label: Option<Option<String>> = sqlx::query_scalar(sql.select_state.sql())
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        label
            .flatten()
            .map(|label| ObligationState::from_label(&label))
            .transpose()
    }
}
