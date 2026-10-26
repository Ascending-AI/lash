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
use lash_kernel_value::identity::SessionId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::session_close::SessionCloseStatements;
use sqlx::PgConnection;

use super::{Committing, corrupt, get, sqlx_failure};

static SQL: LazyLock<SessionCloseStatements> =
    LazyLock::new(|| SessionCloseStatements::render(Dialect::postgres()));

/// Record a scope of a session as ending: it has an `Until` child left.
pub(super) static SCOPE_ENDING: super::steps::Entry = super::steps::Entry::new(
    || &SQL.scope_ending,
    &[
        super::steps::Ty::Text,
        super::steps::Ty::Text,
        super::steps::Ty::Now,
        super::steps::Ty::Int,
    ],
    super::steps::Shape::Exec,
);

/// The stored row: its last step done, when it began, its writer's epoch.
type Stored = (Option<SessionCloseStep>, i64, i64);

async fn stored(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Option<Stored>, DurableError> {
    let Some(row) = crate::observed_sql::query(SQL.row.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(tx))
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
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    write: &SessionCloseWrite,
) -> Result<(), DurableError> {
    match write {
        SessionCloseWrite::Begin { session } => {
            crate::observed_sql::query(SQL.begin.sql())
                .bind(session.as_str())
                .bind(commit.now.0)
                .bind(commit.epoch.0)
                .execute(crate::observed_sql::executor(&mut ***tx))
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
            if *step == SessionCloseStep::Artifacts {
                let own = lash_durable::ActorKey::session(session.as_str())
                    .map_err(|error| corrupt("session actor key", &error.to_string()))?;
                if commit.actor != &own {
                    return Err(corrupt(
                        "session close authority",
                        "Artifacts requires the session actor",
                    ));
                }
                let terminal = crate::session_factory::delete_session_tx(
                    tx,
                    session,
                    &mut Default::default(),
                    commit.fleet,
                    u64::try_from(commit.now.0).unwrap_or(0),
                )
                .await
                .map_err(super::store_failure)?;
                if let Some(terminal) = terminal {
                    tx.stage_turn_change(terminal).await.map_err(sqlx_failure)?;
                }
            }
            crate::observed_sql::query(SQL.record_step.sql())
                .bind(session.as_str())
                .bind(step.as_str())
                .bind(commit.epoch.0)
                .execute(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        SessionCloseWrite::ScopeEnded { session, scope } => {
            crate::observed_sql::query(SQL.scope_ended.sql())
                .bind(session.as_str())
                .bind(scope.stored())
                .execute(crate::observed_sql::executor(&mut ***tx))
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
    crate::observed_sql::query(SQL.scope_ending.sql())
        .bind(session.as_str())
        .bind(scope.stored())
        .bind(commit.now.0)
        .bind(commit.epoch.0)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn ending_scopes(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Vec<ScopeKey>, DurableError> {
    let stored: Vec<String> = crate::observed_sql::query_scalar(SQL.ending_scopes.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(tx))
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
