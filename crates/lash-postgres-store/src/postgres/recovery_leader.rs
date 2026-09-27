//! The PostgreSQL recovery leader lease (ADR 0109 §1.6).
//!
//! Each operation reads `clock_timestamp()` and applies its statement in one
//! transaction; the acquire's upsert takes the row lock, so concurrent
//! claimants serialize on it. Every deployment claims due obligations on
//! PostgreSQL (`SKIP LOCKED`), so the lease gates only the leader-only duties.

use std::sync::LazyLock;

use lash_core_execution::store::{
    HolderId, LeaseAnswer, LeaseClaim, LeaseName, LeaseRow, RecoveryLeaderStore,
};
use lash_store_sql::Dialect;
use lash_store_sql::recovery_leader::RecoveryLeaderStatements;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::StoreError;
use crate::support::store_sqlx_error;

static SQL: LazyLock<RecoveryLeaderStatements> =
    LazyLock::new(|| RecoveryLeaderStatements::render(Dialect::postgres()));

fn lease_row(row: &PgRow) -> Result<LeaseRow, StoreError> {
    Ok(LeaseRow {
        holder: HolderId::new(row.try_get::<String, _>(0).map_err(store_sqlx_error)?),
        generation_rank: row.try_get(1).map_err(store_sqlx_error)?,
        term: row.try_get(2).map_err(store_sqlx_error)?,
        elected_at_ms: row.try_get(3).map_err(store_sqlx_error)?,
        expires_at_ms: row.try_get(4).map_err(store_sqlx_error)?,
    })
}

async fn db_now(tx: &mut Transaction<'_, Postgres>) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        crate::connection_sql::connection_sql()
            .select_statement_epoch_ms
            .sql(),
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)
}

async fn current(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
) -> Result<Option<LeaseRow>, StoreError> {
    sqlx::query(SQL.select.sql())
        .bind(name)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(lease_row)
        .transpose()
}

fn millis(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The lease over one PostgreSQL catalog.
#[derive(Clone)]
pub(crate) struct PostgresRecoveryLeader {
    pool: PgPool,
}

impl PostgresRecoveryLeader {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl RecoveryLeaderStore for PostgresRecoveryLeader {
    async fn acquire(&self, claim: &LeaseClaim) -> Result<LeaseAnswer, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = db_now(&mut tx).await?;
        let taken = sqlx::query(SQL.acquire.sql())
            .bind(claim.name.as_str())
            .bind(claim.holder.as_str())
            .bind(claim.generation_rank)
            .bind(now)
            .bind(millis("lease ttl", claim.ttl_ms)?)
            .bind(millis("lease minimum tenure", claim.min_tenure_ms)?)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let row = match taken {
            Some(row) => Some(lease_row(&row)?),
            None => current(&mut tx, claim.name.as_str()).await?,
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(LeaseAnswer {
            leader: row.as_ref().is_some_and(|row| row.holder == claim.holder),
            row,
            db_now_ms: now,
        })
    }

    async fn renew(&self, claim: &LeaseClaim, term: i64) -> Result<LeaseAnswer, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = db_now(&mut tx).await?;
        let renewed = sqlx::query(SQL.renew.sql())
            .bind(claim.name.as_str())
            .bind(claim.holder.as_str())
            .bind(term)
            .bind(now)
            .bind(millis("lease ttl", claim.ttl_ms)?)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let leader = renewed.is_some();
        let row = match renewed {
            Some(row) => Some(lease_row(&row)?),
            None => current(&mut tx, claim.name.as_str()).await?,
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(LeaseAnswer {
            leader,
            row,
            db_now_ms: now,
        })
    }

    async fn resign(
        &self,
        name: &LeaseName,
        holder: &HolderId,
        term: i64,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = db_now(&mut tx).await?;
        let changed = sqlx::query(SQL.resign.sql())
            .bind(name.as_str())
            .bind(holder.as_str())
            .bind(term)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(changed == 1)
    }

    fn due_claims_need_leader(&self) -> bool {
        false
    }
}
