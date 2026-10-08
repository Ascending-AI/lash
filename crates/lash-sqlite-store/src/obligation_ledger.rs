//! The SQLite obligation ledgers (ADR 0109 §1.3): one generic ledger over
//! each table's shared obligation statements.
//!
//! Every ledger's table lives in the deployment's one database, and each
//! ledger holds a connection to it. A due claim is one `BEGIN IMMEDIATE`
//! transaction that reads the due page and claims each row with its
//! compare-and-set: SQLite's write lock serializes it against every other
//! claim, in this process or another on the file, so any node may run it
//! (ADR 0109 §1.7).

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use lash_core_execution::store::{
    ArtifactCleanupLedger, ClaimToken, ClaimedObligation, CleanupUpsert, DeliveryError, KeyColumn,
    KeyColumnType, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, ObligationState, SettleOutcome, StallReason,
    StalledObligation,
};
use lash_core_execution::{ArtifactCleanup, ArtifactReferrer};
use lash_store_sql::artifact::cleanup_obligations::{
    CleanupObligationLedgerStatements, CleanupObligationStatements,
};
use lash_store_sql::obligation::{ObligationSql, ObligationStatementSet};
use rusqlite::Row;
use rusqlite::types::Value;

use crate::conn::SqliteConnection;
use crate::{StoreError, sqlite_conversion_error, sqlite_error, stored_data_corrupt};

static CLEANUPS: LazyLock<CleanupObligationStatements> =
    LazyLock::new(|| CleanupObligationStatements::render(crate::schema_layout::MAIN));
static CLEANUP_LEDGER: LazyLock<CleanupObligationLedgerStatements> =
    LazyLock::new(|| CleanupObligationLedgerStatements::render(crate::schema_layout::MAIN));

/// `kind`'s statements, rendered for the connection's own database.
pub(crate) fn obligation_sql(kind: ObligationKind) -> ObligationSql<'static> {
    match kind {
        ObligationKind::ArtifactCleanup => CLEANUP_LEDGER.obligation_sql(),
    }
}

fn sql_i64(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The stored error of a failed attempt: its message and its code are
/// written together, so one without the other is corrupt.
fn stored_delivery_error(
    message: Option<String>,
    code: Option<String>,
) -> Result<Option<DeliveryError>, StoreError> {
    match (message, code) {
        (Some(message), Some(code)) => Ok(Some(DeliveryError::new(
            lash_core_execution::RuntimeErrorCode::from_wire_code(&code),
            message,
        ))),
        (None, None) => Ok(None),
        _ => Err(stored_data_corrupt(
            "obligation",
            "a last error and its code disagree",
        )),
    }
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
                return Ok(Err(
                    lash_core_execution::store::UndecodableObligation::malformed(format!(
                        "{kind} obligation key column {offset} holds {other:?}, not {column_type:?}"
                    )),
                ));
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

/// One SQLite ledger: `kind`'s statements over a connection to the
/// deployment's database.
#[derive(Clone)]
pub(crate) struct SqliteObligationLedger {
    kind: ObligationKind,
    sql: ObligationSql<'static>,
    conn: SqliteConnection,
}

impl SqliteObligationLedger {
    /// `kind`'s ledger on `conn`.
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

#[async_trait::async_trait]
impl ObligationLedger for SqliteObligationLedger {
    fn kind(&self) -> ObligationKind {
        self.kind
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
                            rusqlite::params![id, token, due, error.message, error.code.as_str()],
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
                            rusqlite::params![
                                id,
                                token,
                                reason.as_str(),
                                error.message,
                                now,
                                error.code.as_str()
                            ],
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
                            row.get(5)?,
                            read_key(kind, row, 6)?,
                        ))
                    })?
                    .collect()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(
                |(id, attempts, reason, last_error, last_error_code, stalled_at, key)| {
                    Ok(StalledObligation {
                        kind,
                        id: ObligationId::new(id),
                        key,
                        reason: StallReason::from_label(&reason)?,
                        attempts: u32::try_from(attempts).map_err(|_| {
                            stored_data_corrupt("obligation", "a negative attempt count")
                        })?,
                        last_error: stored_delivery_error(last_error, last_error_code)?,
                        stalled_at_ms: u64::try_from(stalled_at).map_err(|_| {
                            stored_data_corrupt("obligation", "a negative stall instant")
                        })?,
                    })
                },
            )
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
) -> Result<ObligationId, StoreError> {
    use rusqlite::{OptionalExtension, params};
    let kind = cleanup.referrer().kind().as_str();
    let referrer_id = cleanup.referrer().canonical_id();
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
        })
        .transpose()?;
    let id = ObligationKey::ArtifactCleanup {
        referrer: cleanup.referrer(),
    }
    .id();
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
    if cleanup.is_ended() {
        crate::artifact_store::fence_artifact_referrer_tx(tx, &cleanup.referrer(), now_ms)
            .map_err(sqlite_error)?;
    }
    Ok(id)
}

/// The artifact cleanup ledger: the cleanup table's obligations, each
/// carrying the plan its referrer's end armed.
#[derive(Clone)]
pub(crate) struct SqliteArtifactCleanupLedger {
    ledger: SqliteObligationLedger,
}

impl SqliteArtifactCleanupLedger {
    pub(crate) fn new(conn: SqliteConnection) -> Self {
        Self {
            ledger: SqliteObligationLedger::new(ObligationKind::ArtifactCleanup, conn),
        }
    }
}

#[async_trait::async_trait]
impl ObligationLedger for SqliteArtifactCleanupLedger {
    fn kind(&self) -> ObligationKind {
        ObligationKind::ArtifactCleanup
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        self.ledger.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        self.ledger.claim(id, token, now_ms, claim_ttl_ms).await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        self.ledger.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        self.ledger.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        self.ledger.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        self.ledger.count_stalled().await
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        self.ledger.standing(id).await
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
        self.ledger
            .conn
            .write(move |tx| arm_cleanup_tx(tx, &cleanup, now_ms).map_err(sqlite_conversion_error))
            .await
            .map_err(sqlite_error)
    }

    async fn nudge(&self, referrer: &ArtifactReferrer, now_ms: u64) -> Result<bool, StoreError> {
        let kind = referrer.kind().as_str();
        let id = referrer.canonical_id();
        let due = sql_i64("artifact cleanup due instant", now_ms)?;
        let nudged = self
            .ledger
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
        Ok(nudged > 0)
    }

    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError> {
        use rusqlite::OptionalExtension;
        let id = id.as_str().to_owned();
        let row = self
            .ledger
            .conn
            .call(move |conn| {
                conn.query_row(CLEANUPS.select_by_id.sql(), rusqlite::params![id], |row| {
                    Ok((
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
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
        })
        .transpose()
    }
}

#[cfg(test)]
mod artifact_cleanup_tests {
    use super::*;
    use lash_core_execution::ReferrerGuard;
    use rusqlite::OptionalExtension;

    #[tokio::test]
    async fn cleanup_kind_mismatch_is_stored_corruption() {
        use lash_core_execution::StoreSet as _;
        let memory = crate::SqliteStoreSet::memory().await.expect("memory store");
        let dir = tempfile::tempdir().expect("file root");
        let file = crate::SqliteStoreSet::open(dir.path().join("lash.db"))
            .await
            .expect("file store");
        for set in [&memory, &file] {
            let referrer = ArtifactReferrer::HostPin(lash_core_execution::HostArtifactPin::mint());
            let ledger = set.artifact_cleanup();
            let id = ledger
                .arm_cleanup(&ArtifactCleanup::ended(referrer.clone(), vec![], None), 1)
                .await
                .expect("arm");
            for body in [serde_json::json!({"referrer": referrer, "plan": {"plan": "await_journal"}, "gate": null}).to_string(), r#"{"plan":"await_journal"}"#.to_owned()] {
                let row_id = id.as_str().to_owned();
                set.process_env_store().conn.call(move |conn| conn.execute("UPDATE artifact_cleanup_obligations SET cleanup_json = ?1 WHERE obligation_id = ?2", rusqlite::params![body, row_id])).await.expect("inject mismatched guard");
                let result = ledger.load_cleanup(&id).await;
                assert!(matches!(result, Err(StoreError::StoredDataCorrupt { .. })), "mismatched guard must be corrupt: {result:?}");
            }
        }
    }

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
        let file = crate::SqliteStoreSet::open(dir.path().join("lash.db"))
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
        let guard = ArtifactCleanup::Await(ReferrerGuard::Journal({
            let ArtifactReferrer::Execution(journal) = referrer.clone() else {
                panic!("fixture referrer kind")
            };
            journal
        }));
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
        assert_eq!(
            fresh_id,
            ObligationKey::ArtifactCleanup {
                referrer: fresh_referrer.clone(),
            }
            .id()
        );
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
        let guard = ArtifactCleanup::Await(ReferrerGuard::Journal({
            let ArtifactReferrer::Execution(journal) = referrer.clone() else {
                panic!("fixture referrer kind")
            };
            journal
        }));
        let first = arm_cleanup_tx(&conn, &guard, 100).expect("arm guard");
        let again = arm_cleanup_tx(&conn, &guard, 101).expect("repeat guard");
        assert_eq!(first, again);
        let ended = ArtifactCleanup::ended(referrer.clone(), Vec::new(), None);
        let replaced = arm_cleanup_tx(&conn, &ended, 102).expect("replace guard");
        assert_eq!(first, replaced);
        arm_cleanup_tx(&conn, &guard, 103).expect("guard cannot replace end");
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
