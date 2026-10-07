//! The session's closing state on PostgreSQL: the `session_close` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6b (FIG-5176). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ScopeKey, SessionCloseRow, SessionCloseStep, SessionCloseWrite,
};
use lash_durable::{DurableError, DurableInstant, Epoch};
use lash_sansio::SessionId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::session_close::SessionCloseStatements;
use sqlx::PgConnection;

use super::{Committing, corrupt, get, sqlx_failure};

static SQL: LazyLock<SessionCloseStatements> =
    LazyLock::new(|| SessionCloseStatements::render(Dialect::postgres()));

/// The stored row: its last step done, when it began, its writer's epoch.
type Stored = (Option<SessionCloseStep>, i64, i64);

async fn stored(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Option<Stored>, DurableError> {
    let Some(row) = sqlx::query(SQL.row.sql())
        .bind(session.as_str())
        .fetch_optional(tx)
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let done = get::<Option<String>>(&row, 0)?
        .map(|step| {
            SessionCloseStep::parse(&step).ok_or_else(|| corrupt("session close step", &step))
        })
        .transpose()?;
    Ok(Some((done, get(&row, 1)?, get(&row, 2)?)))
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &SessionCloseWrite,
) -> Result<(), DurableError> {
    match write {
        SessionCloseWrite::Begin { session } => {
            sqlx::query(SQL.begin.sql())
                .bind(session.as_str())
                .bind(commit.now.0)
                .bind(commit.epoch.0)
                .execute(tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        SessionCloseWrite::Step { session, step } => {
            let Some((done, _, _)) = stored(&mut *tx, session).await? else {
                return Err(DurableError::Domain(DomainRefusal::SessionNotClosing {
                    session: session.clone(),
                }));
            };
            let expected = done.map_or(Some(SessionCloseStep::Cancel), SessionCloseStep::next);
            if expected != Some(*step) {
                return Err(DurableError::Domain(
                    DomainRefusal::SessionCloseOutOfOrder {
                        session: session.clone(),
                        step: *step,
                        done,
                    },
                ));
            }
            sqlx::query(SQL.record_step.sql())
                .bind(session.as_str())
                .bind(step.as_str())
                .bind(commit.epoch.0)
                .execute(tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        SessionCloseWrite::ScopeEnded { session, scope } => {
            sqlx::query(SQL.scope_ended.sql())
                .bind(session.as_str())
                .bind(scope.stored())
                .execute(tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
    }
}

/// Record `scope` of `session` as ending: its closure left a child to mark.
/// Recording a scope already ending changes nothing.
pub(super) async fn record_ending(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    session: &SessionId,
    scope: &ScopeKey,
) -> Result<(), DurableError> {
    sqlx::query(SQL.scope_ending.sql())
        .bind(session.as_str())
        .bind(scope.stored())
        .bind(commit.now.0)
        .bind(commit.epoch.0)
        .execute(tx)
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn ending_scopes(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Vec<ScopeKey>, DurableError> {
    let stored: Vec<String> = sqlx::query_scalar(SQL.ending_scopes.sql())
        .bind(session.as_str())
        .fetch_all(tx)
        .await
        .map_err(sqlx_failure)?;
    stored
        .iter()
        .map(|key| ScopeKey::parse(key).map_err(|_| corrupt("scope key", key)))
        .collect()
}

pub(super) async fn read(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Option<SessionCloseRow>, DurableError> {
    Ok(stored(tx, session)
        .await?
        .map(|(done, begun_at, written_epoch)| SessionCloseRow {
            session: session.clone(),
            done,
            begun_at: DurableInstant(begun_at),
            written_epoch: Epoch(written_epoch),
        }))
}
