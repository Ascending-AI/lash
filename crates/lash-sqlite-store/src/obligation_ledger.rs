//! The SQLite obligation ledgers (ADR 0109 §1.3): one generic ledger over
//! each table's shared obligation statements.
//!
//! Ingress, intents, roots and the session catalog live in the durable core;
//! parent-end plans and processes in the process registry file; trigger
//! deliveries in the trigger store's file. Each ledger
//! holds a connection to its own database. Ingress spans two tables, one
//! ledger each, composed by [`crate::ingress_obligation`]. SQLite has one writer, so a due
//! claim is one `BEGIN IMMEDIATE` transaction that reads the due page and
//! claims each row with its compare-and-set; the recovery leader runs it
//! (ADR 0109 §1.7).

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core_execution::store::{
    ArtifactCleanupLedger, ClaimToken, ClaimedObligation, CleanupUpsert, KeyColumn, KeyColumnType,
    ObligationId, ObligationKey, ObligationKind, ObligationLedger, ObligationSettlement,
    ObligationStanding, ObligationState, SettleOutcome, StallReason, StalledObligation,
};
use lash_core_execution::{ArtifactCleanup, ArtifactReferrer};
use lash_store_sql::artifact::cleanup_obligations::{
    CleanupObligationLedgerStatements, CleanupObligationStatements,
};
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use lash_store_sql::process::parent_end_plans::ParentEndPlanObligationStatements;
use lash_store_sql::process::processes::{
    ProcessObligationStatements, ProcessStartObligationStatements,
};
use lash_store_sql::session::meta::SessionMetaObligationStatements;
use lash_store_sql::session_roots::control_intents::ControlIntentObligationStatements;
use lash_store_sql::session_roots::roots::SessionRootObligationStatements;
use lash_store_sql::trigger::deliveries::DeliveryObligationStatements;
use rusqlite::types::Value;
use rusqlite::{Row, params_from_iter};

use crate::conn::SqliteConnection;
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_conversion_error, sqlite_error, stored_data_corrupt};

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
static PROCESS_STARTS: LazyLock<ProcessStartObligationStatements> =
    LazyLock::new(|| ProcessStartObligationStatements::render(Schema::Main.dialect()));
static DELIVERIES: LazyLock<DeliveryObligationStatements> =
    LazyLock::new(|| DeliveryObligationStatements::render(Schema::Main.dialect()));
static CLEANUPS: LazyLock<CleanupObligationStatements> =
    LazyLock::new(|| CleanupObligationStatements::render(Schema::Main.dialect()));
static CLEANUP_LEDGER: LazyLock<CleanupObligationLedgerStatements> =
    LazyLock::new(|| CleanupObligationLedgerStatements::render(Schema::Main.dialect()));

/// The due instant a producer's own transaction arms at: `due` from the
/// commit, so a claim never waits on a clock edge (ADR 0109 §1.5).
pub(crate) const DUE_AT_ONCE_MS: u64 = 0;

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
        ObligationKind::TriggerDelivery => DELIVERIES.obligation_sql(),
        ObligationKind::ProcessStart => PROCESS_STARTS.obligation_sql(),
        ObligationKind::ProcessTerminal => PROCESSES.obligation_sql(),
        ObligationKind::ArtifactCleanup => CLEANUP_LEDGER.obligation_sql(),
    }
}

/// Whether `kind`'s ledger lives in the process registry file rather than
/// the durable core.
pub(crate) const fn in_process_registry(kind: ObligationKind) -> bool {
    matches!(
        kind,
        ObligationKind::ParentEnd | ObligationKind::ProcessStart | ObligationKind::ProcessTerminal
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

pub(crate) fn read_claim(kind: ObligationKind, row: &Row<'_>) -> rusqlite::Result<ClaimRow> {
    Ok((row.get(0)?, row.get(1)?, read_key(kind, row, 2)?))
}

pub(crate) fn claimed(
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

/// [`SqliteObligationLedger::claim`] inside a transaction the caller already
/// holds (FIG-3975): the admission that arms the row claims its obligation
/// in the same commit, so the producer's immediate ask owes no second store
/// round-trip. `token` is the claimant's token and `until_ms` its expiry.
/// `None` when `id` is neither due nor claimed under `token` — as `claim`
/// answers.
pub(crate) fn claim_obligation_tx(
    conn: &rusqlite::Connection,
    sql: ObligationSql<'static>,
    kind: ObligationKind,
    id: &ObligationId,
    token: &ClaimToken,
    until_ms: u64,
) -> Result<Option<ClaimedObligation>, StoreError> {
    let until = sql_i64("obligation claim expiry", until_ms)?;
    let mut claim = conn.prepare_cached(sql.claim.sql()).map_err(sqlite_error)?;
    let mut rows = claim
        .query(rusqlite::params![id.as_str(), token.as_str(), until])
        .map_err(sqlite_error)?;
    let row = rows
        .next()
        .map_err(sqlite_error)?
        .map(|row| read_claim(kind, row))
        .transpose()
        .map_err(sqlite_error)?;
    row.map(|row| claimed(row, token)).transpose()
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
                    let mut select = tx.prepare_cached(sql.select_due.sql())?;
                    select
                        .query_map(rusqlite::params![now, limit], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                let mut claimed = Vec::with_capacity(ids.len());
                let mut claim = tx.prepare_cached(sql.claim_due_row.sql())?;
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
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let kind = self.kind;
        let sql = self.sql();
        let until = sql_i64(
            "obligation claim expiry",
            now_ms.saturating_add(claim_ttl_ms),
        )?;
        let token = token.clone();
        let bound = token.clone();
        let id = id.as_str().to_owned();
        let row = self
            .conn
            .write(move |tx| {
                let mut claim = tx.prepare_cached(sql.claim.sql())?;
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
                    .write(move |tx| {
                        crate::conn::cached_execute(
                            tx,
                            sql.settle_delivered.sql(),
                            rusqlite::params![id, token, now],
                        )
                    })
                    .await
            }
            ObligationSettlement::Retry { due_at_ms, error } => {
                let due = sql_i64("obligation due instant", due_at_ms)?;
                self.conn
                    .write(move |tx| {
                        crate::conn::cached_execute(
                            tx,
                            sql.settle_retry.sql(),
                            rusqlite::params![id, token, due, error],
                        )
                    })
                    .await
            }
            ObligationSettlement::Stall { reason, error } => {
                self.conn
                    .write(move |tx| {
                        crate::conn::cached_execute(
                            tx,
                            sql.settle_stall.sql(),
                            rusqlite::params![id, token, reason.as_str(), error, now],
                        )
                    })
                    .await
            }
            ObligationSettlement::Defer { due_at_ms } => {
                if self.kind != ObligationKind::ArtifactCleanup {
                    return Err(StoreError::Backend(format!(
                        "a {} obligation cannot be deferred",
                        self.kind
                    )));
                }
                let due = sql_i64("obligation due instant", due_at_ms)?;
                self.conn
                    .write(move |tx| {
                        crate::conn::cached_execute(
                            tx,
                            CLEANUP_LEDGER.obligation_settle_defer.sql(),
                            rusqlite::params![id, token, due],
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
            .write(move |tx| {
                crate::conn::cached_execute(tx, sql.rearm.sql(), rusqlite::params![id, now])
            })
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
                let mut select = conn.prepare_cached(sql.select_stalled.sql())?;
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

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        let sql = self.sql();
        let id = id.as_str().to_owned();
        let row: Option<(Option<String>, i64)> = self
            .conn
            .call(move |conn| {
                use rusqlite::OptionalExtension;
                conn.query_row(sql.select_standing.sql(), rusqlite::params![id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        let Some((Some(label), attempts)) = row else {
            return Ok(None);
        };
        Ok(Some(ObligationStanding {
            state: ObligationState::from_label(&label)?,
            attempts: u32::try_from(attempts).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "Obligation",
                message: format!("obligation attempt count {attempts} is out of range"),
            })?,
        }))
    }
}

/// Arm a cleanup beside the end fact or first guarded edge. The caller owns
/// the write transaction, so a competing end cannot interleave this decision.
pub(crate) fn arm_cleanup_tx(
    tx: &rusqlite::Connection,
    cleanup: &ArtifactCleanup,
    now_ms: u64,
    prefix: &str,
) -> Result<ObligationId, StoreError> {
    use rusqlite::{OptionalExtension, params};
    let kind = cleanup.referrer.kind().as_str();
    let referrer_id = cleanup.referrer.canonical_id();
    let row: Option<(String, String, String, String, String)> = tx
        .query_row(
            CLEANUPS.select_by_referrer.sql(),
            params![kind, referrer_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    let existing = row
        .as_ref()
        .map(|(_, _, row_kind, row_id, body)| {
            let stored = ArtifactReferrer::decode(row_kind, row_id)
                .map_err(|error| error.into_store_error("artifact cleanup"))?;
            ArtifactCleanup::from_json(body, &stored)
                .map_err(|error| stored_data_corrupt("artifact cleanup", error))
        })
        .transpose()?;
    let id = row.as_ref().map_or_else(
        || ObligationId::new(format!("{prefix}:{}", uuid::Uuid::new_v4().simple())),
        |row| ObligationId::new(row.0.clone()),
    );
    let body = cleanup
        .to_json()
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    let due = sql_i64("artifact cleanup due instant", now_ms)?;
    match CleanupUpsert::decide(existing.as_ref(), cleanup) {
        CleanupUpsert::Insert => {
            tx.execute(
                CLEANUPS.insert_if_absent.sql(),
                params![kind, referrer_id, body, id.as_str(), due],
            )
            .map_err(sqlite_error)?;
        }
        CleanupUpsert::ReplaceGuard => {
            tx.execute(
                CLEANUPS.replace_guard_with_ended.sql(),
                params![kind, referrer_id, body, due],
            )
            .map_err(sqlite_error)?;
        }
        CleanupUpsert::Keep => {}
    }
    if prefix == "core" && cleanup.plan.is_ended() {
        crate::artifact_store::fence_artifact_referrer_tx(tx, &cleanup.referrer, now_ms)
            .map_err(sqlite_error)?;
    }
    Ok(id)
}

/// The durable-core and process-registry cleanup tables form one ledger. IDs
/// carry their database prefix, so a claimed row is always settled in place.
#[derive(Clone)]
pub(crate) struct SqliteArtifactCleanupLedger {
    core: SqliteObligationLedger,
    registry: SqliteObligationLedger,
    registry_first: Arc<AtomicBool>,
}

impl SqliteArtifactCleanupLedger {
    pub(crate) fn new(core: SqliteConnection, registry: SqliteConnection) -> Self {
        Self {
            core: SqliteObligationLedger::new(ObligationKind::ArtifactCleanup, core),
            registry: SqliteObligationLedger::new(ObligationKind::ArtifactCleanup, registry),
            registry_first: Arc::new(AtomicBool::new(false)),
        }
    }

    fn for_id(&self, id: &ObligationId) -> Result<&SqliteObligationLedger, StoreError> {
        if id.as_str().starts_with("core:") {
            Ok(&self.core)
        } else if id.as_str().starts_with("registry:") {
            Ok(&self.registry)
        } else {
            Err(stored_data_corrupt(
                "artifact cleanup obligation id",
                id.as_str(),
            ))
        }
    }
}

#[async_trait::async_trait]
impl ObligationLedger for SqliteArtifactCleanupLedger {
    fn kind(&self) -> ObligationKind {
        ObligationKind::ArtifactCleanup
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        _now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        if key.kind() != ObligationKind::ArtifactCleanup {
            return Err(StoreError::Backend(format!(
                "a {} key cannot arm the artifact cleanup ledger",
                key.kind()
            )));
        }
        // A cleanup must carry a plan; use ArtifactCleanupLedger::arm_cleanup.
        Ok(None)
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let (first, second) = if self.registry_first.fetch_xor(true, Ordering::Relaxed) {
            (&self.registry, &self.core)
        } else {
            (&self.core, &self.registry)
        };
        let first_quota = NonZeroUsize::new(limit.get().div_ceil(2))
            .ok_or_else(|| StoreError::Backend("zero cleanup claim limit".into()))?;
        let mut rows = first.claim_due(now_ms, claim_ttl_ms, first_quota).await?;
        if let Some(remaining) = NonZeroUsize::new(limit.get() - rows.len()) {
            rows.extend(second.claim_due(now_ms, claim_ttl_ms, remaining).await?);
        }
        if let Some(remaining) = NonZeroUsize::new(limit.get() - rows.len()) {
            rows.extend(first.claim_due(now_ms, claim_ttl_ms, remaining).await?);
        }
        Ok(rows)
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        self.for_id(id)?
            .claim(id, token, now_ms, claim_ttl_ms)
            .await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        self.for_id(id)?.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        self.for_id(id)?.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        let mut rows = self.core.list_stalled(after, limit).await?;
        rows.extend(self.registry.list_stalled(after, limit).await?);
        rows.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        rows.truncate(limit.get());
        Ok(rows)
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        Ok(self.core.count_stalled().await? + self.registry.count_stalled().await?)
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        self.for_id(id)?.standing(id).await
    }
}

#[async_trait::async_trait]
impl ArtifactCleanupLedger for SqliteArtifactCleanupLedger {
    async fn arm_cleanup(
        &self,
        cleanup: &ArtifactCleanup,
        now_ms: u64,
    ) -> Result<ObligationId, StoreError> {
        let cleanup = cleanup.clone();
        self.core
            .conn
            .write(move |tx| {
                arm_cleanup_tx(tx, &cleanup, now_ms, "core").map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }

    async fn nudge(&self, referrer: &ArtifactReferrer, now_ms: u64) -> Result<bool, StoreError> {
        let kind = referrer.kind().as_str();
        let id = referrer.canonical_id();
        let due = sql_i64("artifact cleanup due instant", now_ms)?;
        let core = self
            .core
            .conn
            .write({
                let id = id.clone();
                move |tx| {
                    crate::conn::cached_execute(
                        tx,
                        CLEANUPS.nudge.sql(),
                        rusqlite::params![kind, id, due],
                    )
                }
            })
            .await
            .map_err(sqlite_error)?;
        let registry = self
            .registry
            .conn
            .write(move |tx| {
                crate::conn::cached_execute(
                    tx,
                    CLEANUPS.nudge.sql(),
                    rusqlite::params![kind, id, due],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(core + registry > 0)
    }

    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError> {
        use rusqlite::OptionalExtension;
        let conn = self.for_id(id)?.conn.clone();
        let id = id.as_str().to_owned();
        let row: Option<(String, String, String)> = conn
            .call(move |conn| {
                conn.query_row(CLEANUPS.select_by_id.sql(), rusqlite::params![id], |row| {
                    Ok((row.get(2)?, row.get(3)?, row.get(4)?))
                })
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        // A referrer kind a newer build wrote is refused
        // `Incompatible(UnknownVocabulary)`, never read as corrupt or absent.
        row.map(|(kind, id, body)| {
            let referrer = ArtifactReferrer::decode(&kind, &id)
                .map_err(|error| error.into_store_error("artifact cleanup"))?;
            ArtifactCleanup::from_json(&body, &referrer)
                .map_err(|error| stored_data_corrupt("artifact cleanup", error))
        })
        .transpose()
    }
}

#[cfg(test)]
mod artifact_cleanup_tests {
    use super::*;
    use lash_core_execution::ArtifactCleanupPlan;
    use rusqlite::OptionalExtension;

    /// The stored row of one referrer's cleanup obligation, compared whole
    /// across an aborted arm.
    #[derive(Debug, PartialEq)]
    struct CleanupRow {
        id: String,
        body: String,
        state: String,
        attempts: i64,
        due_at_ms: Option<i64>,
        claim_token: Option<String>,
        stall_reason: Option<String>,
        last_error: Option<String>,
        settled_at_ms: Option<i64>,
    }

    async fn cleanup_row(
        core: &crate::SqliteStore,
        referrer: &ArtifactReferrer,
    ) -> Option<CleanupRow> {
        let kind = referrer.kind().as_str().to_owned();
        let id = referrer.canonical_id();
        core.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT obligation_id, cleanup_json, obligation_state, obligation_attempts, \
                            obligation_due_at_ms, obligation_claim_token, obligation_stall_reason, \
                            obligation_last_error, obligation_settled_at_ms \
                     FROM artifact_cleanup_obligations \
                     WHERE referrer_kind = ?1 AND referrer_id = ?2",
                    rusqlite::params![kind, id],
                    |row| {
                        Ok(CleanupRow {
                            id: row.get(0)?,
                            body: row.get(1)?,
                            state: row.get(2)?,
                            attempts: row.get(3)?,
                            due_at_ms: row.get(4)?,
                            claim_token: row.get(5)?,
                            stall_reason: row.get(6)?,
                            last_error: row.get(7)?,
                            settled_at_ms: row.get(8)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .expect("read the cleanup row")
    }

    async fn fenced(core: &crate::SqliteStore, referrer: &ArtifactReferrer) -> bool {
        let referrer = referrer.clone();
        core.conn
            .call(move |conn| crate::artifact_store::artifact_fenced_tx(conn, &referrer))
            .await
            .expect("read the referrer fence")
    }

    /// FIG-4180: an `Ended` arm whose fence insert fails aborts the whole
    /// transaction — the cleanup row the same call inserted or rewrote
    /// rolls back with it, so `arm_cleanup`'s `Err` never leaves a
    /// committed mutation behind. File and memory store sets alike.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cleanup_arm_rolls_back_when_the_end_fence_fails() {
        let memory = crate::SqliteStoreSet::memory()
            .await
            .expect("open the memory store set");
        let dir = tempfile::tempdir().expect("file store set root");
        let file = crate::SqliteStoreSet::open(dir.path())
            .await
            .expect("open the file store set");
        for set in [&memory, &file] {
            cleanup_arm_rolls_back_when_the_end_fence_fails_on(set).await;
        }
    }

    async fn cleanup_arm_rolls_back_when_the_end_fence_fails_on(set: &crate::SqliteStoreSet) {
        use lash_core_execution::StoreSet as _;
        let ledger = set.artifact_cleanup();
        let core = set.process_env_store();
        let execution = |name: &str| {
            ArtifactReferrer::Execution(
                lash_sansio::ExecutionScope::runtime_operation(name)
                    .journal_identity()
                    .expect("journal identity"),
            )
        };

        // A claimed guard: the state an `Ended` arm replaces, and the
        // claim, state and attempts a partial commit would reset.
        let referrer = execution("guarded");
        let guard = ArtifactCleanup {
            referrer: referrer.clone(),
            plan: ArtifactCleanupPlan::AwaitJournal,
            gate: None,
        };
        let guard_id = ledger
            .arm_cleanup(&guard, 100)
            .await
            .expect("arm the guard");
        let claimed = ledger
            .claim(&guard_id, &ClaimToken::mint(), 110, 60_000)
            .await
            .expect("claim the guard")
            .expect("the due guard claims");
        assert_eq!(claimed.attempts, 1);
        let claimed_row = cleanup_row(&core, &referrer)
            .await
            .expect("the claimed guard's row");
        assert_eq!(claimed_row.state, "claimed");

        // Every fence insert aborts its statement.
        core.conn
            .write(|tx| {
                tx.execute_batch(
                    "CREATE TRIGGER fig_4180_fence_fault BEFORE INSERT \
                     ON referrer_fences \
                     BEGIN SELECT RAISE(ABORT, 'fig-4180 fence fault'); END",
                )
            })
            .await
            .expect("install the fence fault");

        // A fresh `Ended` arm fails — and must leave no cleanup row at all.
        let fresh_referrer = execution("fresh");
        let fresh_ended = ArtifactCleanup::ended(fresh_referrer.clone(), Vec::new(), None);
        let error = ledger
            .arm_cleanup(&fresh_ended, 200)
            .await
            .expect_err("the fence fault fails the arm");
        assert!(error.to_string().contains("fig-4180"), "{error}");
        assert!(
            cleanup_row(&core, &fresh_referrer).await.is_none(),
            "the aborted arm committed no cleanup row"
        );
        assert!(
            !fenced(&core, &fresh_referrer).await,
            "the aborted arm committed no fence"
        );

        // `Ended` over the claimed guard fails — and the guard's claim,
        // state, attempts, id and body all stand.
        let ended = ArtifactCleanup::ended(referrer.clone(), Vec::new(), None);
        let error = ledger
            .arm_cleanup(&ended, 210)
            .await
            .expect_err("the fence fault fails the replacement");
        assert!(error.to_string().contains("fig-4180"), "{error}");
        assert_eq!(
            cleanup_row(&core, &referrer).await,
            Some(claimed_row),
            "the aborted replacement left the claimed guard untouched"
        );
        assert!(
            !fenced(&core, &referrer).await,
            "the aborted replacement committed no fence"
        );

        // Lift the fault: the retry commits the replacement and the fence,
        // keeping the row's obligation id, and replays idempotently.
        core.conn
            .write(|tx| tx.execute_batch("DROP TRIGGER fig_4180_fence_fault"))
            .await
            .expect("drop the fence fault");
        let retried = ledger
            .arm_cleanup(&ended, 220)
            .await
            .expect("the cleared fault admits the retry");
        assert_eq!(retried, guard_id);
        assert_eq!(
            ledger
                .arm_cleanup(&ended, 230)
                .await
                .expect("an `Ended` arm replays"),
            guard_id
        );
        assert!(fenced(&core, &referrer).await, "the retry fenced");
        let replaced = cleanup_row(&core, &referrer)
            .await
            .expect("the replaced row");
        assert_eq!(
            (
                replaced.state.as_str(),
                replaced.attempts,
                replaced.due_at_ms,
                replaced.claim_token.as_deref()
            ),
            ("due", 0, Some(220), None)
        );
        assert_eq!(
            ArtifactCleanup::from_json(&replaced.body, &referrer).expect("decode the body"),
            ended
        );
        let fresh_id = ledger
            .arm_cleanup(&fresh_ended, 240)
            .await
            .expect("the fresh arm commits");
        assert!(fresh_id.as_str().starts_with("core:"));
        assert!(fenced(&core, &fresh_referrer).await);
    }

    #[test]
    fn guard_upsert_and_ended_replacement_share_one_obligation() {
        let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
        conn.execute_batch(crate::schema::SCHEMA)
            .expect("create durable core");
        let journal = lash_sansio::ExecutionScope::runtime_operation("cleanup-test")
            .journal_identity()
            .expect("journal identity");
        let referrer = ArtifactReferrer::Execution(journal);
        let guard = ArtifactCleanup {
            referrer: referrer.clone(),
            plan: ArtifactCleanupPlan::AwaitJournal,
            gate: None,
        };
        let first = arm_cleanup_tx(&conn, &guard, 100, "core").expect("arm guard");
        let again = arm_cleanup_tx(&conn, &guard, 101, "core").expect("repeat guard");
        assert_eq!(first, again);
        let ended = ArtifactCleanup::ended(referrer.clone(), Vec::new(), None);
        let replaced = arm_cleanup_tx(&conn, &ended, 102, "core").expect("replace guard");
        assert_eq!(first, replaced);
        arm_cleanup_tx(&conn, &guard, 103, "core").expect("guard cannot replace end");
        let (body, state, due): (String, String, i64) = conn.query_row(
            "SELECT cleanup_json, obligation_state, obligation_due_at_ms FROM artifact_cleanup_obligations",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).expect("read obligation");
        assert_eq!(state, "due");
        assert_eq!(due, 102);
        assert_eq!(
            ArtifactCleanup::from_json(&body, &referrer).expect("decode cleanup"),
            ended
        );
        assert!(crate::artifact_store::artifact_fenced_tx(&conn, &referrer).expect("read fence"));
    }
}
