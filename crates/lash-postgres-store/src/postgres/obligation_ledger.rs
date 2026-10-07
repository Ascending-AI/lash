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
    ArtifactCleanupLedger, ClaimToken, ClaimedObligation, CleanupUpsert, DeliveryError, KeyColumn,
    KeyColumnType, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, ObligationState, SettleOutcome, StallReason,
    StalledObligation, UndecodableObligation,
};
use lash_core_execution::{ArtifactCleanup, ArtifactReferrer};
use lash_store_sql::Dialect;
use lash_store_sql::artifact::cleanup_obligations::{
    CleanupObligationLedgerStatements, CleanupObligationStatements,
};
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::StoreError;
use crate::begin_guarded;
use crate::support::store_sqlx_error;

static CLEANUP: LazyLock<CleanupObligationStatements> =
    LazyLock::new(|| CleanupObligationStatements::render(Dialect::postgres()));
static CLEANUP_LEDGER: LazyLock<CleanupObligationLedgerStatements> =
    LazyLock::new(|| CleanupObligationLedgerStatements::render(Dialect::postgres()));
const CLEANUP_DUE_LOCKING: &str = "SELECT obligation_id FROM lash_artifact_cleanup_obligations WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= $1 ORDER BY obligation_due_at_ms, obligation_id LIMIT $2 FOR UPDATE SKIP LOCKED";

/// `kind`'s shared statements and its locking due read.
fn obligation_sql(kind: ObligationKind) -> (ObligationSql<'static>, &'static str) {
    match kind {
        ObligationKind::ArtifactCleanup => (CLEANUP_LEDGER.obligation_sql(), CLEANUP_DUE_LOCKING),
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

/// One cleanup row. A referrer kind a newer build wrote is refused
/// `Incompatible(UnknownVocabulary)`, never read as corrupt or absent.
fn cleanup_row(row: &PgRow) -> Result<(ObligationId, ArtifactCleanup), StoreError> {
    let id = ObligationId::new(row.try_get::<String, _>(0).map_err(store_sqlx_error)?);
    let kind: String = row.try_get(2).map_err(store_sqlx_error)?;
    let referrer_id: String = row.try_get(3).map_err(store_sqlx_error)?;
    let referrer = ArtifactReferrer::decode(&kind, &referrer_id)
        .map_err(|error| error.into_store_error("artifact_cleanup_obligation"))?;
    let json: String = row.try_get(4).map_err(store_sqlx_error)?;
    let cleanup = ArtifactCleanup::from_json(&json, &referrer)?;
    Ok((id, cleanup))
}

pub(crate) async fn arm_cleanup_tx(
    conn: &mut sqlx::PgConnection,
    cleanup: &ArtifactCleanup,
    now_ms: u64,
) -> Result<ObligationId, StoreError> {
    crate::artifact_store::lock_referrer_tx(conn, &cleanup.referrer())
        .await
        .map_err(store_sqlx_error)?;
    let kind = cleanup.referrer().kind().as_str();
    let referrer_id = cleanup.referrer().canonical_id();
    let existing = sqlx::query(CLEANUP.select_by_referrer.sql())
        .bind(kind)
        .bind(&referrer_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    let decoded = existing.as_ref().map(cleanup_row).transpose()?;
    let decision = CleanupUpsert::decide(decoded.as_ref().map(|(_, body)| body), cleanup);
    let id = ObligationKey::ArtifactCleanup {
        referrer: cleanup.referrer(),
    }
    .id();
    let json = cleanup
        .to_json()
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    match decision {
        CleanupUpsert::Insert => {
            sqlx::query(CLEANUP.insert_if_absent.sql())
                .bind(kind)
                .bind(&referrer_id)
                .bind(json)
                .bind(id.as_str())
                .bind(sql_i64("cleanup due instant", now_ms)?)
                .execute(&mut *conn)
                .await
                .map_err(store_sqlx_error)?;
        }
        CleanupUpsert::ReplaceGuard => {
            sqlx::query(CLEANUP.replace_guard_with_ended.sql())
                .bind(kind)
                .bind(&referrer_id)
                .bind(json)
                .bind(sql_i64("cleanup due instant", now_ms)?)
                .execute(&mut *conn)
                .await
                .map_err(store_sqlx_error)?;
        }
        CleanupUpsert::Keep => {}
    }
    if cleanup.is_ended() {
        sqlx::query(
            crate::artifact_store::artifact_sql()
                .fences
                .insert_fence
                .sql(),
        )
        .bind(kind)
        .bind(&referrer_id)
        .bind(sql_i64("referrer end instant", now_ms)?)
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    }
    Ok(id)
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
                    return Ok(Err(UndecodableObligation::malformed(format!(
                        "{kind} obligation key column {offset}: {error}"
                    ))));
                }
            },
            KeyColumnType::Integer => match row.try_get::<i64, _>(index) {
                Ok(integer) => KeyColumn::Integer(integer),
                Err(error) => {
                    return Ok(Err(UndecodableObligation::malformed(format!(
                        "{kind} obligation key column {offset}: {error}"
                    ))));
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

/// One PostgreSQL ledger: `kind`'s statements over the store's pool.
#[derive(Clone)]
pub(crate) struct PostgresObligationLedger {
    kind: ObligationKind,
    sql: ObligationSql<'static>,
    locking: &'static str,
    pool: PgPool,
    fence: crate::guarded_tx::WriterFence,
}

impl PostgresObligationLedger {
    pub(crate) fn new(
        kind: ObligationKind,
        pool: PgPool,
        fence: crate::guarded_tx::WriterFence,
    ) -> Self {
        let (sql, locking) = obligation_sql(kind);
        Self {
            kind,
            sql,
            locking,
            pool,
            fence,
        }
    }
}

#[async_trait::async_trait]
impl ObligationLedger for PostgresObligationLedger {
    fn kind(&self) -> ObligationKind {
        self.kind
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let (sql, locking) = (self.sql, self.locking);
        let now = sql_i64("obligation claim instant", now_ms)?;
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let token = ClaimToken::mint();
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        let ids: Vec<String> = sqlx::query_scalar(locking)
            .bind(now)
            .bind(limit)
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut claimed = Vec::with_capacity(ids.len());
        for id in ids {
            let row = sqlx::query(sql.claim_due_row.sql())
                .bind(&id)
                .bind(token.as_str())
                .bind(until)
                .bind(now)
                .fetch_optional(&mut **tx)
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
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let sql = self.sql;
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        let row = sqlx::query(sql.claim.sql())
            .bind(id.as_str())
            .bind(token.as_str())
            .bind(until)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        row.map(|row| read_claim(self.kind, &row, token))
            .transpose()
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        let sql = self.sql;
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
                .bind(error.message)
                .bind(error.code.as_str().to_owned()),
            ObligationSettlement::Stall { reason, error } => sqlx::query(sql.settle_stall.sql())
                .bind(id.as_str())
                .bind(token.as_str())
                .bind(reason.as_str())
                .bind(error.message)
                .bind(now)
                .bind(error.code.as_str().to_owned()),
            ObligationSettlement::Defer { due_at_ms } => {
                if self.kind != ObligationKind::ArtifactCleanup {
                    return Err(StoreError::Backend(
                        "defer requires an artifact cleanup obligation".into(),
                    ));
                }
                sqlx::query(CLEANUP_LEDGER.obligation_settle_defer.sql())
                    .bind(id.as_str())
                    .bind(token.as_str())
                    .bind(sql_i64("obligation due instant", due_at_ms)?)
            }
        };
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        let changed = query
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if changed == 1 {
            SettleOutcome::Applied
        } else {
            SettleOutcome::ClaimLost
        })
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        let sql = self.sql;
        let due_at = sql_i64("obligation due instant", now_ms)?;
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        let changed = sqlx::query(sql.rearm.sql())
            .bind(id.as_str())
            .bind(due_at)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(changed == 1)
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        let sql = self.sql;
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
                let last_error: Option<String> = row.try_get(3).map_err(store_sqlx_error)?;
                let last_error_code: Option<String> = row.try_get(4).map_err(store_sqlx_error)?;
                let stalled_at: i64 = row.try_get(5).map_err(store_sqlx_error)?;
                Ok(StalledObligation {
                    kind: self.kind,
                    id: ObligationId::new(row.try_get::<String, _>(0).map_err(store_sqlx_error)?),
                    key: read_key(self.kind, row, 6)?,
                    reason: StallReason::from_label(&reason)?,
                    attempts: u32::try_from(attempts)
                        .map_err(|_| corrupt("a negative attempt count"))?,
                    last_error: match (last_error, last_error_code) {
                        (Some(message), Some(code)) => Some(DeliveryError::new(
                            lash_core_execution::RuntimeErrorCode::from_wire_code(&code),
                            message,
                        )),
                        (None, None) => None,
                        _ => return Err(corrupt("a last error and its code disagree")),
                    },
                    stalled_at_ms: u64::try_from(stalled_at)
                        .map_err(|_| corrupt("a negative stall instant"))?,
                })
            })
            .collect()
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        let sql = self.sql;
        let count: i64 = sqlx::query_scalar(sql.count_stalled.sql())
            .fetch_one(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        u64::try_from(count).map_err(|_| corrupt("a negative count"))
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        let sql = self.sql;
        let row: Option<(Option<String>, i32)> = sqlx::query_as(sql.select_standing.sql())
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        let Some((Some(label), attempts)) = row else {
            return Ok(None);
        };
        Ok(Some(ObligationStanding {
            state: ObligationState::from_label(&label)?,
            attempts: u32::try_from(attempts)
                .map_err(|_| corrupt("a negative obligation attempt count"))?,
        }))
    }
}

#[async_trait::async_trait]
impl ArtifactCleanupLedger for PostgresObligationLedger {
    async fn arm_cleanup(
        &self,
        cleanup: &ArtifactCleanup,
        now_ms: u64,
    ) -> Result<ObligationId, StoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        let id = arm_cleanup_tx(&mut tx, cleanup, now_ms).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(id)
    }

    async fn nudge(&self, referrer: &ArtifactReferrer, now_ms: u64) -> Result<bool, StoreError> {
        let due_at = sql_i64("cleanup due instant", now_ms)?;
        let mut tx = crate::begin_guarded(&self.pool, &self.fence).await?;
        let changed = sqlx::query(CLEANUP.nudge.sql())
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .bind(due_at)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(changed == 1)
    }

    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError> {
        sqlx::query(CLEANUP.select_by_id.sql())
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?
            .as_ref()
            .map(cleanup_row)
            .transpose()
            .map(|entry| entry.map(|(_, cleanup)| cleanup))
    }
}
