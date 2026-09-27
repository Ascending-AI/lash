//! The PostgreSQL store half of a session's two-phase delete (ADR 0109 §4).
//!
//! The acknowledgement of a session's `CloseSession` intent arms its delete
//! in the transaction that writes it ([`arm_on_close_acknowledged_conn`]).

use std::sync::LazyLock;

use lash_core_execution::store::session_delete::{
    SessionCleanup, SessionDeleteLedger, SessionDeleteObligation,
};
use lash_core_execution::store::{
    ControlIntent, ControlIntentKind, ControlIntentState, ObligationId, ObligationKey,
    ObligationState,
};
use lash_core_execution::{EffectOpener, ScopeId, SessionId};
use lash_store_sql::Dialect;
use lash_store_sql::process::parent_end_plans::ParentEndPlanCleanupStatements;
use lash_store_sql::session::meta::SessionMetaDeleteStatements;
use lash_store_sql::session_roots::roots::SessionRootCleanupStatements;
use sqlx::{PgConnection, PgPool};

use crate::StoreError;
use crate::support::store_sqlx_error;

static META: LazyLock<SessionMetaDeleteStatements> =
    LazyLock::new(|| SessionMetaDeleteStatements::render(Dialect::postgres()));
static ROOTS: LazyLock<SessionRootCleanupStatements> =
    LazyLock::new(|| SessionRootCleanupStatements::render(Dialect::postgres()));
static PLANS: LazyLock<ParentEndPlanCleanupStatements> =
    LazyLock::new(|| ParentEndPlanCleanupStatements::render(Dialect::postgres()));

/// Arm session `next`'s `SessionDelete` obligation when `next` is its
/// `CloseSession` intent's acknowledgement over `prior`, in the transaction
/// that writes it: the close's delivered settle owes the physical delete
/// (ADR 0109 §3). A no-op for any other write, and once the row carries an
/// obligation.
pub(crate) async fn arm_on_close_acknowledged_conn(
    conn: &mut PgConnection,
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
    crate::obligation_ledger::arm_obligation_tx(
        conn,
        &ObligationKey::SessionDelete {
            session_id: next.session_id.clone(),
        },
        at_ms,
    )
    .await?;
    Ok(())
}

fn count(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind: "obligation",
        message: "a negative count".to_owned(),
    })
}

/// The PostgreSQL session-delete ledger over the store's pool.
#[derive(Clone)]
pub(crate) struct PostgresSessionDeleteLedger {
    pool: PgPool,
}

impl PostgresSessionDeleteLedger {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl SessionDeleteLedger for PostgresSessionDeleteLedger {
    async fn delete_obligation(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionDeleteObligation>, StoreError> {
        let row: Option<(String, String)> = sqlx::query_as(META.delete_obligation.sql())
            .bind(session_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
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
        let scope_close: i64 = sqlx::query_scalar(ROOTS.count_undelivered_scope_close.sql())
            .bind(session_id.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        let (turns_from, turns_to) = EffectOpener::session_turn_encoding_range(session_id);
        let (drains_from, drains_to) = EffectOpener::session_queue_drain_encoding_range(session_id);
        let parent_end: i64 = sqlx::query_scalar(PLANS.count_undelivered_for_session.sql())
            .bind(ScopeId::session(session_id.clone()).storage_id())
            .bind(turns_from)
            .bind(turns_to)
            .bind(drains_from)
            .bind(drains_to)
            .fetch_one(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(SessionCleanup {
            scope_close: count(scope_close)?,
            parent_end: count(parent_end)?,
        })
    }

    async fn count_closing(&self) -> Result<u64, StoreError> {
        let closing: i64 = sqlx::query_scalar(META.count_closing.sql())
            .fetch_one(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        count(closing)
    }
}
