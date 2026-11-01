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

use super::steps::{Entry, Shape, Step, Ty, Value};
use super::{Committing, corrupt, get, sqlx_failure};

static SQL: LazyLock<SessionCloseStatements> =
    LazyLock::new(|| SessionCloseStatements::render(Dialect::postgres()));

/// Record a scope of a session as ending: it has an `Until` child left.
pub(super) static SCOPE_ENDING: Entry = Entry::new(
    || &SQL.scope_ending,
    &[Ty::Text, Ty::Text, Ty::Now, Ty::Int],
    Shape::Exec,
);

lash_store_sql::statements! {
    /// The close's guard only PostgreSQL's one-call commit issues.
    struct SessionClosePostgresStatements @ "durable_session_close_postgres" {
        /// Whether session `?1` is closing with `?2` its last step done
        /// (none: no step yet).
        step_ready = "SELECT done_step IS NOT DISTINCT FROM ?2 AS ready FROM session_close
             WHERE session_id = ?1";
    }
}

static POSTGRES: LazyLock<SessionClosePostgresStatements> =
    LazyLock::new(|| SessionClosePostgresStatements::render(Dialect::postgres()));

pub(super) static BEGIN: Entry =
    Entry::new(|| &SQL.begin, &[Ty::Text, Ty::Now, Ty::Int], Shape::Exec);
pub(super) static STEP_READY: Entry = Entry::new(
    || &POSTGRES.step_ready,
    &[Ty::Text, Ty::Text],
    Shape::Select,
)
.flagged("ready");
pub(super) static RECORD_STEP: Entry = Entry::new(
    || &SQL.record_step,
    &[Ty::Text, Ty::Text, Ty::Int],
    Shape::Exec,
);
pub(super) static SCOPE_ENDED: Entry =
    Entry::new(|| &SQL.scope_ended, &[Ty::Text, Ty::Text], Shape::Exec);

/// The step done before `step`: none before the first.
fn previous(step: SessionCloseStep) -> Option<SessionCloseStep> {
    [
        SessionCloseStep::Cancel,
        SessionCloseStep::Revoke,
        SessionCloseStep::EndScope,
        SessionCloseStep::Artifacts,
        SessionCloseStep::Tombstone,
    ]
    .into_iter()
    .find(|done| done.next() == Some(step))
}

/// `write`'s step program at `epoch`, committed by `actor`: the artifacts
/// step deletes the session's rows ([`super::session_delete`]) between
/// its guard and its record. None for an artifacts step another actor
/// commits, which the explicit form refuses.
pub(super) fn steps(
    actor: &lash_durable::ActorKey,
    epoch: Epoch,
    write: &SessionCloseWrite,
) -> Result<Option<Vec<Step>>, DurableError> {
    Ok(Some(match write {
        SessionCloseWrite::Begin { session } => vec![Step::any(
            &BEGIN,
            vec![
                Value::text(session.as_str()),
                Value::Now,
                Value::int(epoch.0),
            ],
        )],
        SessionCloseWrite::Step { session, step } => {
            let deletes = if *step == SessionCloseStep::Artifacts {
                if !own_actor(actor, session)? {
                    return Ok(None);
                }
                super::session_delete::steps(session)?
            } else {
                Vec::new()
            };
            let mut program = vec![Step::one(
                &STEP_READY,
                vec![
                    Value::text(session.as_str()),
                    Value::Text(previous(*step).map(|done| done.as_str().to_owned())),
                ],
            )];
            program.extend(deletes);
            program.push(Step::one(
                &RECORD_STEP,
                vec![
                    Value::text(session.as_str()),
                    Value::text(step.as_str()),
                    Value::int(epoch.0),
                ],
            ));
            program
        }
        SessionCloseWrite::ScopeEnded { session, scope } => vec![Step::any(
            &SCOPE_ENDED,
            vec![Value::text(session.as_str()), Value::text(scope.stored())],
        )],
    }))
}

/// Whether `actor` is `session`'s own: the only actor that deletes it.
fn own_actor(actor: &lash_durable::ActorKey, session: &SessionId) -> Result<bool, DurableError> {
    let own = lash_durable::ActorKey::session(session.as_str())
        .map_err(|error| corrupt("session actor key", &error.to_string()))?;
    Ok(actor == &own)
}

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
    let SessionCloseWrite::Step { session, step } = write else {
        let program = steps(commit.actor, commit.epoch, write)?
            .ok_or_else(|| corrupt("session close", "a write without a program"))?;
        return match super::steps::run(tx, &program, &commit.steps()).await? {
            None => Ok(()),
            Some(_) => Err(corrupt("session close", "an unguarded step refused")),
        };
    };
    let Some(program) = steps(commit.actor, commit.epoch, write)? else {
        // Another actor's artifacts step, refused once its order holds.
        let Some((done, _, _)) = stored(&mut *tx, session).await? else {
            return Err(not_closing(session));
        };
        if previous(*step) != done {
            return Err(out_of_order(tx, session, *step).await?);
        }
        return Err(corrupt(
            "session close authority",
            "Artifacts requires the session actor",
        ));
    };
    match super::steps::run_capturing(tx, &program, &commit.steps())
        .await
        .map_err(DurableError::from)?
    {
        Ok(slots) => {
            if super::session_delete::deleted_materialized(&slots) {
                tx.staged_turn_change();
            }
            Ok(())
        }
        Err(refused) => Err(
            match super::session_delete::unreadable(&program, &refused) {
                Some(unreadable) => super::store_failure(unreadable),
                None => out_of_order(tx, session, *step).await?,
            },
        ),
    }
}

fn not_closing(session: &SessionId) -> DurableError {
    DurableError::Domain(DomainRefusal::SessionNotClosing {
        session: session.clone(),
    })
}

/// Why `session`'s close refused `step`, read in the refused transaction.
async fn out_of_order(
    tx: &mut PgConnection,
    session: &SessionId,
    step: SessionCloseStep,
) -> Result<DurableError, DurableError> {
    Ok(match stored(tx, session).await? {
        None => not_closing(session),
        Some((done, _, _)) => DurableError::Domain(DomainRefusal::SessionCloseOutOfOrder {
            session: session.clone(),
            step,
            done,
        }),
    })
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
