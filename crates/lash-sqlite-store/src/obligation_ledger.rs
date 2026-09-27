//! The SQLite obligation ledgers (ADR 0109 §1.3): one generic ledger over
//! each table's shared obligation statements.
//!
//! Ingress, intents, roots and the session catalog live in the durable core;
//! parent-end plans and processes in the process registry file. Each ledger
//! holds a connection to its own database. Ingress spans two tables, one
//! ledger each, composed by [`crate::ingress_obligation`]. SQLite has one writer, so a due
//! claim is one `BEGIN IMMEDIATE` transaction that reads the due page and
//! claims each row with its compare-and-set; the recovery leader runs it
//! (ADR 0109 §1.7).

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use lash_core_execution::store::{
    ClaimToken, ClaimedObligation, KeyColumn, KeyColumnType, ObligationId, ObligationKey,
    ObligationKind, ObligationLedger, ObligationSettlement, ObligationState, SettleOutcome,
    StallReason, StalledObligation,
};
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use lash_store_sql::process::parent_end_plans::ParentEndPlanObligationStatements;
use lash_store_sql::process::processes::ProcessObligationStatements;
use lash_store_sql::session::meta::SessionMetaObligationStatements;
use lash_store_sql::session_roots::control_intents::ControlIntentObligationStatements;
use lash_store_sql::session_roots::roots::SessionRootObligationStatements;
use rusqlite::types::Value;
use rusqlite::{Row, params_from_iter};

use crate::conn::SqliteConnection;
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

static INTENTS: LazyLock<ControlIntentObligationStatements> =
    LazyLock::new(|| ControlIntentObligationStatements::render(Schema::Main.dialect()));
static ROOTS: LazyLock<SessionRootObligationStatements> =
    LazyLock::new(|| SessionRootObligationStatements::render(Schema::Main.dialect()));
static META: LazyLock<SessionMetaObligationStatements> =
    LazyLock::new(|| SessionMetaObligationStatements::render(Schema::Main.dialect()));
static PLANS: LazyLock<ParentEndPlanObligationStatements> =
    LazyLock::new(|| ParentEndPlanObligationStatements::render(Schema::Main.dialect()));
static PROCESSES: LazyLock<ProcessObligationStatements> =
    LazyLock::new(|| ProcessObligationStatements::render(Schema::Main.dialect()));

/// `kind`'s statements, rendered for the connection's own database. Ingress
/// names its turn-input table here; its ledger composes that table with the
/// queued-batch table (`crate::ingress_obligation`).
pub(crate) fn obligation_sql(kind: ObligationKind) -> ObligationSql<'static> {
    match kind {
        ObligationKind::Ingress => crate::ingress_obligation::turn_input_sql(),
        ObligationKind::ControlIntent => INTENTS.obligation_sql(),
        ObligationKind::ScopeClose => ROOTS.obligation_sql(),
        ObligationKind::SessionDelete => META.obligation_sql(),
        ObligationKind::ParentEnd => PLANS.obligation_sql(),
        ObligationKind::ProcessTerminal => PROCESSES.obligation_sql(),
    }
}

/// Whether `kind`'s ledger lives in the process registry file rather than
/// the durable core.
pub(crate) const fn in_process_registry(kind: ObligationKind) -> bool {
    matches!(
        kind,
        ObligationKind::ParentEnd | ObligationKind::ProcessTerminal
    )
}

fn sql_i64(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

fn key_values(key: &ObligationKey) -> Vec<Value> {
    key.columns()
        .into_iter()
        .map(|column| match column {
            KeyColumn::Text(text) => Value::Text(text),
            KeyColumn::Integer(integer) => Value::Integer(integer),
        })
        .collect()
}

/// The key columns of `kind` read from `row` starting at column `first`.
fn read_key(
    kind: ObligationKind,
    row: &Row<'_>,
    first: usize,
) -> rusqlite::Result<Result<ObligationKey, lash_core_execution::store::UndecodableObligation>> {
    let mut columns = Vec::with_capacity(kind.key_column_types().len());
    for (offset, column_type) in kind.key_column_types().iter().enumerate() {
        let value: Value = row.get(first + offset)?;
        columns.push(match (column_type, value) {
            (KeyColumnType::Text, Value::Text(text)) => KeyColumn::Text(text),
            (KeyColumnType::Integer, Value::Integer(integer)) => KeyColumn::Integer(integer),
            (_, other) => {
                return Ok(Err(lash_core_execution::store::UndecodableObligation {
                    detail: format!(
                        "{kind} obligation key column {offset} holds {other:?}, not {column_type:?}"
                    ),
                }));
            }
        });
    }
    Ok(ObligationKey::decode(kind, columns))
}

/// A claim's projection: id, attempts, key.
type ClaimRow = (
    String,
    i64,
    Result<ObligationKey, lash_core_execution::store::UndecodableObligation>,
);

fn read_claim(kind: ObligationKind, row: &Row<'_>) -> rusqlite::Result<ClaimRow> {
    Ok((row.get(0)?, row.get(1)?, read_key(kind, row, 2)?))
}

fn claimed(
    (id, attempts, key): ClaimRow,
    token: &ClaimToken,
) -> Result<ClaimedObligation, StoreError> {
    Ok(ClaimedObligation {
        id: ObligationId::new(id),
        token: token.clone(),
        attempts: u32::try_from(attempts)
            .map_err(|_| stored_data_corrupt("obligation", "a negative attempt count"))?,
        key,
    })
}

/// One SQLite ledger: `kind`'s statements over the connection to the
/// database that holds its table.
#[derive(Clone)]
pub(crate) struct SqliteObligationLedger {
    kind: ObligationKind,
    sql: ObligationSql<'static>,
    conn: SqliteConnection,
}

impl SqliteObligationLedger {
    /// `kind`'s ledger on `conn`, which must be open on the database that
    /// holds `kind`'s table.
    pub(crate) fn new(kind: ObligationKind, conn: SqliteConnection) -> Self {
        Self::over_table(kind, obligation_sql(kind), conn)
    }

    /// The ledger of one table of `kind`, through that table's statements.
    pub(crate) fn over_table(
        kind: ObligationKind,
        sql: ObligationSql<'static>,
        conn: SqliteConnection,
    ) -> Self {
        Self { kind, sql, conn }
    }

    fn sql(&self) -> ObligationSql<'static> {
        self.sql
    }
}

/// Arm `key`'s row as a fresh obligation due at `now_ms` inside a producer's
/// own transaction: the helper a slice's producer calls on its transaction.
/// An ingress row's id is derived from its item (`crate::ingress_obligation`);
/// every other kind's is minted. `None` when the row is missing or already
/// carries an obligation.
pub(crate) fn arm_obligation_tx(
    conn: &rusqlite::Connection,
    key: &ObligationKey,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    if key.kind() == ObligationKind::Ingress {
        return crate::ingress_obligation::arm_ingress_tx(conn, key, now_ms);
    }
    arm_obligation_id_tx(conn, key, &ObligationId::mint(key.kind()), now_ms)
}

/// [`arm_obligation_tx`] with the id the row's own transaction derived (ADR
/// 0109 §1.1): a producer that must name its obligation afterwards — the
/// terminal write naming its scope close — arms the id it derived rather
/// than a minted one.
pub(crate) fn arm_obligation_id_tx(
    conn: &rusqlite::Connection,
    key: &ObligationKey,
    id: &ObligationId,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    arm_table_tx(conn, obligation_sql(key.kind()), key, id.clone(), now_ms)
}

/// Arm `key`'s row in the table `sql` addresses as obligation `id`, due at
/// `now_ms`, inside the caller's transaction. `None` when the row is missing
/// or already carries an obligation.
pub(crate) fn arm_table_tx(
    conn: &rusqlite::Connection,
    sql: ObligationSql<'static>,
    key: &ObligationKey,
    id: ObligationId,
    now_ms: u64,
) -> Result<Option<ObligationId>, StoreError> {
    let mut values = key_values(key);
    values.push(Value::Text(id.as_str().to_owned()));
    values.push(Value::Integer(sql_i64("obligation due instant", now_ms)?));
    let changed = conn
        .execute(sql.arm.sql(), params_from_iter(values))
        .map_err(sqlite_error)?;
    Ok((changed == 1).then_some(id))
}

#[async_trait::async_trait]
impl ObligationLedger for SqliteObligationLedger {
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
        let key = key.clone();
        self.conn
            .write(move |tx| Ok(arm_obligation_tx(tx, &key, now_ms)))
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let kind = self.kind;
        let sql = self.sql();
        let now = sql_i64("obligation claim instant", now_ms)?;
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let token = ClaimToken::mint();
        let rows = self
            .conn
            .write(move |tx| {
                let ids = {
                    let mut select = tx.prepare(sql.select_due.sql())?;
                    select
                        .query_map(rusqlite::params![now, limit], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                let mut claimed = Vec::with_capacity(ids.len());
                let mut claim = tx.prepare(sql.claim_due_row.sql())?;
                for id in ids {
                    let mut rows =
                        claim.query(rusqlite::params![id, token.as_str(), until, now])?;
                    if let Some(row) = rows.next()? {
                        claimed.push(read_claim(kind, row)?);
                    }
                }
                Ok((claimed, token))
            })
            .await
            .map_err(sqlite_error)?;
        let (rows, token) = rows;
        rows.into_iter().map(|row| claimed(row, &token)).collect()
    }

    async fn claim(
        &self,
        id: &ObligationId,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let kind = self.kind;
        let sql = self.sql();
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let token = ClaimToken::mint();
        let bound = token.clone();
        let id = id.as_str().to_owned();
        let row = self
            .conn
            .write(move |tx| {
                let mut claim = tx.prepare(sql.claim.sql())?;
                let mut rows = claim.query(rusqlite::params![id, bound.as_str(), until])?;
                rows.next()?.map(|row| read_claim(kind, row)).transpose()
            })
            .await
            .map_err(sqlite_error)?;
        row.map(|row| claimed(row, &token)).transpose()
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        let sql = self.sql();
        let now = sql_i64("obligation settle instant", now_ms)?;
        let id = id.as_str().to_owned();
        let token = token.as_str().to_owned();
        let changed = match settlement {
            ObligationSettlement::Delivered => {
                self.conn
                    .call(move |conn| {
                        conn.execute(
                            sql.settle_delivered.sql(),
                            rusqlite::params![id, token, now],
                        )
                    })
                    .await
            }
            ObligationSettlement::Retry { due_at_ms, error } => {
                let due = sql_i64("obligation due instant", due_at_ms)?;
                self.conn
                    .call(move |conn| {
                        conn.execute(
                            sql.settle_retry.sql(),
                            rusqlite::params![id, token, due, error],
                        )
                    })
                    .await
            }
            ObligationSettlement::Stall { reason, error } => {
                self.conn
                    .call(move |conn| {
                        conn.execute(
                            sql.settle_stall.sql(),
                            rusqlite::params![id, token, reason.as_str(), error, now],
                        )
                    })
                    .await
            }
        }
        .map_err(sqlite_error)?;
        Ok(if changed == 1 {
            SettleOutcome::Applied
        } else {
            SettleOutcome::ClaimLost
        })
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        let sql = self.sql();
        let now = sql_i64("obligation due instant", now_ms)?;
        let id = id.as_str().to_owned();
        let changed = self
            .conn
            .call(move |conn| conn.execute(sql.rearm.sql(), rusqlite::params![id, now]))
            .await
            .map_err(sqlite_error)?;
        Ok(changed == 1)
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        let kind = self.kind;
        let sql = self.sql();
        let after = after.map_or_else(String::new, |id| id.as_str().to_owned());
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        type StalledRow = (
            String,
            i64,
            String,
            Option<String>,
            i64,
            Result<ObligationKey, lash_core_execution::store::UndecodableObligation>,
        );
        let rows: Vec<StalledRow> = self
            .conn
            .call(move |conn| {
                let mut select = conn.prepare(sql.select_stalled.sql())?;
                select
                    .query_map(rusqlite::params![after, limit], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            read_key(kind, row, 5)?,
                        ))
                    })?
                    .collect()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(|(id, attempts, reason, last_error, stalled_at, key)| {
                Ok(StalledObligation {
                    kind,
                    id: ObligationId::new(id),
                    key,
                    reason: StallReason::from_label(&reason)?,
                    attempts: u32::try_from(attempts).map_err(|_| {
                        stored_data_corrupt("obligation", "a negative attempt count")
                    })?,
                    last_error,
                    stalled_at_ms: u64::try_from(stalled_at).map_err(|_| {
                        stored_data_corrupt("obligation", "a negative stall instant")
                    })?,
                })
            })
            .collect()
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        let sql = self.sql();
        let count: i64 = self
            .conn
            .call(move |conn| conn.query_row(sql.count_stalled.sql(), [], |row| row.get(0)))
            .await
            .map_err(sqlite_error)?;
        u64::try_from(count).map_err(|_| stored_data_corrupt("obligation", "a negative count"))
    }

    async fn state(&self, id: &ObligationId) -> Result<Option<ObligationState>, StoreError> {
        let sql = self.sql();
        let id = id.as_str().to_owned();
        let label: Option<Option<String>> = self
            .conn
            .call(move |conn| {
                use rusqlite::OptionalExtension;
                conn.query_row(sql.select_state.sql(), rusqlite::params![id], |row| {
                    row.get(0)
                })
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        label
            .flatten()
            .map(|label| ObligationState::from_label(&label))
            .transpose()
    }
}
