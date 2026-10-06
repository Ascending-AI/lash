//! The SQLite reads of the `SessionDelete` obligation kind (ADR 0109 §4).
//! Nothing arms the kind any more: a session's delete is its own mail on the
//! durable substrate (ADR 0132 §12), and L10b deletes the kind.

use std::sync::LazyLock;

use lash_core_execution::store::session_delete::{
    SessionCleanup, SessionDeleteLedger, SessionDeleteObligation,
};
use lash_core_execution::store::{ObligationId, ObligationState};
use lash_core_execution::{EffectOpener, ScopeId, SessionId};
use lash_store_sql::process::parent_end_plans::ParentEndPlanCleanupStatements;
use lash_store_sql::session::meta::SessionMetaDeleteStatements;
use lash_store_sql::session_runs::runs::SessionRunCleanupStatements;
use rusqlite::OptionalExtension;

use crate::conn::SqliteConnection;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

static META: LazyLock<SessionMetaDeleteStatements> =
    LazyLock::new(|| SessionMetaDeleteStatements::render(crate::schema_layout::MAIN));
static RUNS: LazyLock<SessionRunCleanupStatements> =
    LazyLock::new(|| SessionRunCleanupStatements::render(crate::schema_layout::MAIN));
static PLANS: LazyLock<ParentEndPlanCleanupStatements> =
    LazyLock::new(|| ParentEndPlanCleanupStatements::render(crate::schema_layout::MAIN));

/// The SQLite session-delete ledger over the deployment's database.
#[derive(Clone)]
pub(crate) struct SqliteSessionDeleteLedger {
    conn: SqliteConnection,
}

impl SqliteSessionDeleteLedger {
    pub(crate) fn new(conn: SqliteConnection) -> Self {
        Self { conn }
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
            .conn
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
            .conn
            .call(move |conn| {
                conn.query_row(
                    RUNS.count_undelivered_scope_close.sql(),
                    rusqlite::params![session],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)?;
        let own = ScopeId::session(session_id.clone()).storage_id();
        let (turns_from, turns_to) = EffectOpener::session_turn_encoding_range(session_id);
        let (drains_from, drains_to) = EffectOpener::session_operation_encoding_range(session_id);
        let parent_end: i64 = self
            .conn
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
            .conn
            .call(|conn| conn.query_row(META.count_closing.sql(), [], |row| row.get(0)))
            .await
            .map_err(sqlite_error)?;
        count(closing)
    }
}
