//! The SQLite recovery leader lease (ADR 0109 §1.6).
//!
//! The row lives in the durable core. Each operation reads the database clock
//! and applies its statement inside one `BEGIN IMMEDIATE` transaction, so the
//! comparison and the write see one instant and no second writer between
//! them. SQLite has one writer, so due-obligation claims are leader-only here.

use std::sync::LazyLock;

use lash_core_execution::store::{
    HolderId, LeaseAnswer, LeaseClaim, LeaseName, LeaseRow, RecoveryLeaderStore,
};
use lash_store_sql::recovery_leader::RecoveryLeaderStatements;
use rusqlite::{OptionalExtension, Row, Transaction};

use crate::conn::SqliteConnection;
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_error};

static SQL: LazyLock<RecoveryLeaderStatements> =
    LazyLock::new(|| RecoveryLeaderStatements::render(Schema::Main.dialect()));

fn lease_row(row: &Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        holder: HolderId::new(row.get::<_, String>(0)?),
        generation_rank: row.get(1)?,
        term: row.get(2)?,
        elected_at_ms: row.get(3)?,
        expires_at_ms: row.get(4)?,
    })
}

fn db_now(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
    tx.query_row(crate::connection_sql::SELECT_DATABASE_EPOCH_MS, [], |row| {
        row.get(0)
    })
}

fn current(tx: &Transaction<'_>, name: &str) -> rusqlite::Result<Option<LeaseRow>> {
    tx.query_row(SQL.select.sql(), rusqlite::params![name], lease_row)
        .optional()
}

fn millis(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The lease over one durable core.
#[derive(Clone)]
pub(crate) struct SqliteRecoveryLeader {
    conn: SqliteConnection,
}

impl SqliteRecoveryLeader {
    pub(crate) fn new(conn: SqliteConnection) -> Self {
        Self { conn }
    }
}

#[async_trait::async_trait]
impl RecoveryLeaderStore for SqliteRecoveryLeader {
    async fn acquire(&self, claim: &LeaseClaim) -> Result<LeaseAnswer, StoreError> {
        let name = claim.name.as_str().to_owned();
        let holder = claim.holder.as_str().to_owned();
        let rank = claim.generation_rank;
        let ttl = millis("lease ttl", claim.ttl_ms)?;
        let tenure = millis("lease minimum tenure", claim.min_tenure_ms)?;
        let me = claim.holder.clone();
        self.conn
            .write(move |tx| {
                let now = db_now(tx)?;
                let taken = tx
                    .query_row(
                        SQL.acquire.sql(),
                        rusqlite::params![name, holder, rank, now, ttl, tenure],
                        lease_row,
                    )
                    .optional()?;
                let row = match taken {
                    Some(row) => Some(row),
                    None => current(tx, &name)?,
                };
                Ok(LeaseAnswer {
                    leader: row.as_ref().is_some_and(|row| row.holder == me),
                    row,
                    db_now_ms: now,
                })
            })
            .await
            .map_err(sqlite_error)
    }

    async fn renew(&self, claim: &LeaseClaim, term: i64) -> Result<LeaseAnswer, StoreError> {
        let name = claim.name.as_str().to_owned();
        let holder = claim.holder.as_str().to_owned();
        let ttl = millis("lease ttl", claim.ttl_ms)?;
        self.conn
            .write(move |tx| {
                let now = db_now(tx)?;
                let renewed = tx
                    .query_row(
                        SQL.renew.sql(),
                        rusqlite::params![name, holder, term, now, ttl],
                        lease_row,
                    )
                    .optional()?;
                let leader = renewed.is_some();
                let row = match renewed {
                    Some(row) => Some(row),
                    None => current(tx, &name)?,
                };
                Ok(LeaseAnswer {
                    leader,
                    row,
                    db_now_ms: now,
                })
            })
            .await
            .map_err(sqlite_error)
    }

    async fn resign(
        &self,
        name: &LeaseName,
        holder: &HolderId,
        term: i64,
    ) -> Result<bool, StoreError> {
        let name = name.as_str().to_owned();
        let holder = holder.as_str().to_owned();
        self.conn
            .write(move |tx| {
                let now = db_now(tx)?;
                Ok(crate::conn::cached_execute(
                    tx,
                    SQL.resign.sql(),
                    rusqlite::params![name, holder, term, now],
                )? == 1)
            })
            .await
            .map_err(sqlite_error)
    }

    fn due_claims_need_leader(&self) -> bool {
        true
    }
}
