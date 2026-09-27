//! The SQLite store half of a session's two-phase delete (ADR 0109 §4).
//!
//! A session's delete obligation and its roots' scope closes live in the
//! durable core; the parent-end plans of the scopes it owns live in the
//! process registry file. The acknowledgement of a session's `CloseSession`
//! intent arms its delete in the durable core's transaction
//! ([`arm_on_close_acknowledged_conn`]).

use std::sync::LazyLock;

use lash_core_execution::store::session_delete::{
    SessionCleanup, SessionDeleteLedger, SessionDeleteObligation,
};
use lash_core_execution::store::{
    ControlIntent, ControlIntentKind, ControlIntentState, ObligationId, ObligationKind,
    ObligationState,
};
use lash_core_execution::{EffectOpener, ScopeId, SessionId};
use lash_store_sql::process::parent_end_plans::ParentEndPlanCleanupStatements;
use lash_store_sql::session::meta::SessionMetaDeleteStatements;
use lash_store_sql::session_roots::roots::SessionRootCleanupStatements;
use rusqlite::OptionalExtension;

use crate::conn::SqliteConnection;
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

static META: LazyLock<SessionMetaDeleteStatements> =
    LazyLock::new(|| SessionMetaDeleteStatements::render(Schema::Main.dialect()));
static ROOTS: LazyLock<SessionRootCleanupStatements> =
    LazyLock::new(|| SessionRootCleanupStatements::render(Schema::Main.dialect()));
static PLANS: LazyLock<ParentEndPlanCleanupStatements> =
    LazyLock::new(|| ParentEndPlanCleanupStatements::render(Schema::Main.dialect()));

/// Arm session `next`'s `SessionDelete` obligation when `next` is its
/// `CloseSession` intent's acknowledgement over `prior`, in the transaction
/// that writes it: the close's delivered settle owes the physical delete
/// (ADR 0109 §3). A no-op for any other write, and once the row carries an
/// obligation.
pub(crate) fn arm_on_close_acknowledged_conn(
    tx: &rusqlite::Connection,
    prior: &ControlIntent,
    next: &ControlIntent,
) -> Result<(), StoreError> {
    let ControlIntentState::Acknowledged { at_ms } = next.state else {
        return Ok(());
    };
    if !matches!(next.kind, ControlIntentKind::CloseSession { .. })
        || matches!(prior.state, ControlIntentState::Acknowledged { .. })
    {
        return Ok(());
    }
    let id = ObligationId::mint(ObligationKind::SessionDelete);
    let due = i64::try_from(at_ms).map_err(|_| {
        StoreError::Backend(format!(
            "obligation due instant {at_ms} exceeds the stored range"
        ))
    })?;
    tx.execute(
        crate::obligation_ledger::obligation_sql(ObligationKind::SessionDelete)
            .arm
            .sql(),
        rusqlite::params![next.session_id.as_str(), id.as_str(), due],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// The SQLite session-delete ledger: the durable core and the process
/// registry file, each through its own connection.
#[derive(Clone)]
pub(crate) struct SqliteSessionDeleteLedger {
    core: SqliteConnection,
    registry: SqliteConnection,
}

impl SqliteSessionDeleteLedger {
    pub(crate) fn new(core: SqliteConnection, registry: SqliteConnection) -> Self {
        Self { core, registry }
    }
}

fn count(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| stored_data_corrupt("obligation", "a negative count"))
}

#[async_trait::async_trait]
impl SessionDeleteLedger for SqliteSessionDeleteLedger {
    async fn delete_obligation(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionDeleteObligation>, StoreError> {
        let session = session_id.as_str().to_owned();
        let row: Option<(String, String)> = self
            .core
            .call(move |conn| {
                conn.query_row(
                    META.delete_obligation.sql(),
                    rusqlite::params![session],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        row.map(|(id, state)| {
            Ok(SessionDeleteObligation {
                id: ObligationId::new(id),
                state: ObligationState::from_label(&state)?,
            })
        })
        .transpose()
    }

    async fn undelivered_cleanup(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionCleanup, StoreError> {
        let session = session_id.as_str().to_owned();
        let scope_close: i64 = self
            .core
            .call(move |conn| {
                conn.query_row(
                    ROOTS.count_undelivered_scope_close.sql(),
                    rusqlite::params![session],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)?;
        let own = ScopeId::session(session_id.clone()).storage_id();
        let (turns_from, turns_to) = EffectOpener::session_turn_encoding_range(session_id);
        let (drains_from, drains_to) = EffectOpener::session_queue_drain_encoding_range(session_id);
        let parent_end: i64 = self
            .registry
            .call(move |conn| {
                conn.query_row(
                    PLANS.count_undelivered_for_session.sql(),
                    rusqlite::params![own, turns_from, turns_to, drains_from, drains_to],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(SessionCleanup {
            scope_close: count(scope_close)?,
            parent_end: count(parent_end)?,
        })
    }

    async fn count_closing(&self) -> Result<u64, StoreError> {
        let closing: i64 = self
            .core
            .call(|conn| conn.query_row(META.count_closing.sql(), [], |row| row.get(0)))
            .await
            .map_err(sqlite_error)?;
        count(closing)
    }
}
