//! Process actors, cancel, terminal and cascade on PostgreSQL: the `processes` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.
//!
//! The actor columns live on the registry's `lash_processes` row; the
//! terminal and the cancel request are the registry's own writes on this
//! commit's connection, so the registry and the actor never disagree.

use crate::guarded_tx::GuardedTx;
use lash_core_execution::runtime::actor::process::{scope_id, scope_index, subtree_roots};
use lash_durable::domain::{
    CANCEL_MAIL, CancelAnswer, CancelRequest, DomainRefusal, ProcessActorRow, ProcessStartRows,
    ProcessWrite, ScopeKey,
};
use lash_durable::{
    ActorKey, DurableError, DurableInstant, Epoch, MailRefusal, StoreFailure, StoreFailureKind,
    Woken,
};
use lash_sansio::{CancelOrigin, ProcessId};
use sqlx::PgConnection;

use super::{Committing, SQL, corrupt, get, integer, sqlx_failure, wake_within};
use crate::process_helpers::{
    CancelRecorded, record_cancel_tx, record_event_tx, record_terminal_tx,
};

/// A registry failure inside a durable commit: the registry row did not
/// decode or refused, which the port reports as a store failure. Transient
/// substrate faults retain their typed classification for retry.
fn registry_failure(error: &lash_core_execution::PluginError) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: match error {
            lash_core_execution::PluginError::StoreUnavailable {
                fault: lash_core_execution::store::StoreFault::Contended,
            } => StoreFailureKind::Contended,
            lash_core_execution::PluginError::StoreUnavailable { .. } => {
                StoreFailureKind::Unavailable
            }
            _ => StoreFailureKind::Corrupt,
        },
        message: error.to_string(),
    })
}

fn millis(at: DurableInstant) -> Result<u64, DurableError> {
    integer(at.0)
}

fn instant(ms: u64) -> Result<DurableInstant, DurableError> {
    integer(ms).map(DurableInstant)
}

fn process_actor(process: &ProcessId) -> Result<ActorKey, DurableError> {
    ActorKey::process(process.as_str()).map_err(|_| corrupt("process actor key", process.as_str()))
}

/// Create `process`'s actor, ready, in the caller's transaction: a process
/// registration's other half.
pub(crate) async fn create_actor_within(
    tx: &mut PgConnection,
    process: &ProcessId,
    formats: &str,
    now: DurableInstant,
) -> Result<(), DurableError> {
    let actor = process_actor(process)?;
    let created = sqlx::query(SQL.actor.create.sql())
        .bind(actor.as_str())
        .bind(actor.kind().as_str())
        .bind(formats)
        .bind(now.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    match created {
        Some(_) => Ok(()),
        None => Err(DurableError::MailRefused(MailRefusal::ActorExists(actor))),
    }
}

/// Lock `process`'s actor row for the rest of the caller's transaction,
/// ahead of its registry row: the order the actor's owner commit takes them
/// in (its fence, then its `advance` or terminal), so a cancel and an owner
/// commit never wait on each other in a cycle (FIG-5855). An actor that
/// does not exist takes no lock.
pub(crate) async fn lock_actor_within(
    tx: &mut PgConnection,
    process: &ProcessId,
) -> Result<(), DurableError> {
    let actor = process_actor(process)?;
    sqlx::query(SQL.postgres.lock_actors.sql())
        .bind(vec![actor.as_str()])
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

/// Control-wake `process` and, for a newly recorded request, append its
/// cancel mail: the registry's cancel record commits with both.
pub(crate) async fn cancel_mail_within(
    tx: &mut PgConnection,
    process: &ProcessId,
    origin: CancelOrigin,
    requester: &str,
    newly_requested: bool,
    now: DurableInstant,
) -> Result<Woken, DurableError> {
    let actor = process_actor(process)?;
    let (woken, seq) = wake_within(tx, &actor, true, now).await?;
    if newly_requested {
        let body = serde_json::json!({ "origin": origin, "requester": requester }).to_string();
        sqlx::query(SQL.mail.append.sql())
            .bind(actor.as_str())
            .bind(seq.0)
            .bind(CANCEL_MAIL)
            .bind(body)
            .bind(now.0)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    }
    Ok(woken)
}

/// Record `origin`'s cancel of `process` unless one stands, append its
/// cancel mail and control-wake it: what a cancel request and each child of
/// a cascade batch write. A terminal process is answered `AlreadyEnded`
/// and written nothing. The process's actor row is locked before its
/// registry row ([`lock_actor_within`]).
pub(crate) async fn cancel_within(
    tx: &mut GuardedTx<'_>,
    process: &ProcessId,
    origin: CancelOrigin,
    requester: &str,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<(CancelAnswer, Option<Woken>), DurableError> {
    lock_actor_within(tx, process).await?;
    let recorded = record_cancel_tx(tx, process, origin, requester, millis(now)?, fleet)
        .await
        .map_err(|error| registry_failure(&error))?;
    let answer = match recorded {
        CancelRecorded::Ended => return Ok((CancelAnswer::AlreadyEnded, None)),
        CancelRecorded::Requested { at_ms } => CancelAnswer::Requested {
            at: instant(at_ms)?,
        },
        CancelRecorded::AlreadyRequested { at_ms } => CancelAnswer::AlreadyRequested {
            at: instant(at_ms)?,
        },
    };
    let woken = cancel_mail_within(
        tx,
        process,
        origin,
        requester,
        matches!(answer, CancelAnswer::Requested { .. }),
        now,
    )
    .await?;
    Ok((answer, Some(woken)))
}

/// Register a store-local start on the commit's connection: its row, its
/// observers and its actor, ready, as the registrar applies them. A start
/// the registrar refuses, or whose key another process holds, refuses the
/// commit.
async fn register_within(
    tx: &mut GuardedTx<'_>,
    commit: &Committing<'_>,
    rows: &ProcessStartRows,
) -> Result<(), DurableError> {
    use crate::process_registry::registration::{AppliedRegistration, apply_registration_tx};
    let Ok(staged) = lash_core_execution::runtime::StagedRegistration::decode(rows) else {
        return Err(corrupt("process start rows", &rows.registration_json));
    };
    let refused = |reason: String| {
        DurableError::Domain(DomainRefusal::ProcessStartRefused {
            process: rows.process.clone(),
            reason,
        })
    };
    match apply_registration_tx(
        tx,
        staged.registration,
        staged.observers,
        staged.process_id,
        false,
        staged.prepared_at_ms,
        commit.fleet,
    )
    .await
    {
        Ok(AppliedRegistration::Created(_)) => Ok(()),
        Ok(AppliedRegistration::Retained { record, .. }) if record.id == rows.process => Ok(()),
        Ok(
            AppliedRegistration::Retained { record, .. }
            | AppliedRegistration::LostRace { winner: record, .. },
        ) => Err(refused(format!(
            "process `{}` already holds its start key",
            record.id
        ))),
        Err(error)
            if matches!(
                error.class(),
                lash_core_execution::PluginErrorClass::Terminal
            ) =>
        {
            Err(refused(error.to_string()))
        }
        Err(error) => Err(DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Unavailable,
            message: error.to_string(),
        })),
    }
}

pub(super) async fn apply(
    tx: &mut GuardedTx<'_>,
    commit: &Committing<'_>,
    write: &ProcessWrite,
) -> Result<(), DurableError> {
    match write {
        ProcessWrite::Register(rows) => register_within(tx, commit, rows).await,
        ProcessWrite::Advance {
            process,
            expected_rev,
            driver_json,
        } => {
            let moved: Option<i64> = sqlx::query_scalar(SQL.process.advance.sql())
                .bind(process.as_str())
                .bind(integer::<i64>(*expected_rev)?)
                .bind(driver_json)
                .bind(commit.epoch.0)
                .fetch_optional(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            if moved.is_some() {
                return Ok(());
            }
            let found = self::process(tx, process)
                .await?
                .filter(|row| !row.terminal)
                .map(|row| row.state_rev);
            Err(DurableError::Domain(DomainRefusal::ProcessRevConflict {
                process: process.clone(),
                expected: *expected_rev,
                found,
            }))
        }
        ProcessWrite::Published { process, through } => {
            sqlx::query(SQL.process.publish.sql())
                .bind(process.as_str())
                .bind(integer::<i64>(*through)?)
                .execute(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        ProcessWrite::AppendEvent {
            process,
            event_type,
            payload_json,
            replay_key,
        } => {
            let payload = serde_json::from_str(payload_json)
                .map_err(|_| corrupt("process event payload", payload_json))?;
            let request = lash_core_execution::ProcessEventAppendRequest::from_stored(
                event_type.as_str(),
                payload,
                replay_key.as_str(),
                commit.fleet,
            )
            .map_err(|error| registry_failure(&error))?;
            record_event_tx(tx, process, request, millis(commit.now)?, commit.fleet)
                .await
                .map_err(|error| registry_failure(&error))
        }
        ProcessWrite::Terminal {
            process,
            outcome_json,
        } => {
            let output: lash_core_execution::ProcessAwaitOutput =
                serde_json::from_str(outcome_json)
                    .map_err(|_| corrupt("process terminal", outcome_json))?;
            let ended = record_terminal_tx(
                tx,
                process,
                &output,
                integer::<u64>(commit.epoch.0)?,
                millis(commit.now)?,
                commit.fleet,
            )
            .await
            .map_err(|error| registry_failure(&error))?;
            if ended {
                set_cursor(tx, process, Some(""), commit.epoch).await?;
            }
            Ok(())
        }
        ProcessWrite::CascadeBatch {
            scope,
            children,
            origin,
            requester,
            cursor,
        } => {
            for child in children {
                cancel_within(tx, child, *origin, requester, commit.now, commit.fleet).await?;
            }
            if let ScopeKey::Process(owner) = scope
                && ActorKey::process(owner.as_str()).is_ok_and(|actor| &actor == commit.actor)
            {
                set_cursor(tx, owner, cursor.as_deref(), commit.epoch).await?;
            }
            Ok(())
        }
        ProcessWrite::ScopeClosed { scope } => close_scope(tx, commit, scope).await,
    }
}

/// Write `scope`'s closure fact under the scope's advisory lock, then read
/// its unmarked `Until` children in this transaction. A registration under
/// the scope holds the same lock until it commits, so a child that
/// committed first is read here (each statement sees what committed before
/// it), and any later one reads the fact and is refused. A turn scope with
/// a child left is recorded as ending.
async fn close_scope(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    scope: &ScopeKey,
) -> Result<(), DurableError> {
    let Some(closed) = scope_id(scope) else {
        return Ok(());
    };
    crate::process_registry::parent_end::record_tx(tx, &closed, millis(commit.now)?, commit.fleet)
        .await
        .map_err(|error| registry_failure(&error))?;
    let ScopeKey::Turn(session, _) = scope else {
        return Ok(());
    };
    if until_children(tx, scope, None, 1).await?.is_empty() {
        return Ok(());
    }
    super::session_close::record_ending(tx, commit, session, scope).await
}

async fn set_cursor(
    tx: &mut PgConnection,
    process: &ProcessId,
    cursor: Option<&str>,
    epoch: Epoch,
) -> Result<(), DurableError> {
    sqlx::query(SQL.process.set_cursor.sql())
        .bind(process.as_str())
        .bind(cursor)
        .bind(epoch.0)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn request_cancel(
    tx: &mut GuardedTx<'_>,
    request: &CancelRequest,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<(CancelAnswer, Option<Woken>), DurableError> {
    cancel_within(
        tx,
        &request.process,
        request.origin,
        &request.requester,
        now,
        fleet,
    )
    .await
}

pub(super) async fn process(
    tx: &mut PgConnection,
    process: &ProcessId,
) -> Result<Option<ProcessActorRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.process.row.sql())
        .bind(process.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let status: String = get(&row, 3)?;
    Ok(Some(ProcessActorRow {
        process: process.clone(),
        state_rev: integer::<u64>(get::<i64>(&row, 0)?)?,
        driver_json: get(&row, 1)?,
        cancel_requested_at: get::<Option<i64>>(&row, 2)?.map(DurableInstant),
        terminal: !matches!(status.as_str(), "running" | "waiting"),
        cascade_cursor: get(&row, 4)?,
        written_epoch: get::<Option<i64>>(&row, 5)?.map(Epoch),
        published_event_sequence: integer::<u64>(get::<i64>(&row, 6)?)?,
    }))
}

pub(super) async fn until_children(
    tx: &mut PgConnection,
    scope: &ScopeKey,
    after: Option<&ProcessId>,
    limit: usize,
) -> Result<Vec<ProcessId>, DurableError> {
    let Some((kind, id)) = scope_index(scope) else {
        return Ok(Vec::new());
    };
    let ids: Vec<String> = sqlx::query_scalar(SQL.process.pending_children.sql())
        .bind(kind)
        .bind(id)
        .bind(after.map_or("", ProcessId::as_str))
        .bind(integer::<i64>(limit)?)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    process_ids(ids)
}

/// Up to `limit` live processes in the `Until` subtree of `scope`, by id:
/// one walk through every process row below it, ended or not, and for a
/// session through its turn and session-operation scopes too. A parent's
/// terminal says nothing about this being empty.
pub(super) async fn live_until_descendants(
    tx: &mut PgConnection,
    scope: &ScopeKey,
    limit: usize,
) -> Result<Vec<ProcessId>, DurableError> {
    let Some(roots) = subtree_roots(scope) else {
        return Ok(Vec::new());
    };
    let ids: Vec<String> = sqlx::query_scalar(SQL.process.live_descendants.sql())
        .bind(roots.kind)
        .bind(roots.id)
        .bind(roots.turns)
        .bind(roots.operations)
        .bind(integer::<i64>(limit)?)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    process_ids(ids)
}

fn process_ids(ids: Vec<String>) -> Result<Vec<ProcessId>, DurableError> {
    ids.into_iter()
        .map(|id| ProcessId::parse(&id).map_err(|_| corrupt("process id", &id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-5235: row decode failures remain corruption across the registry boundary.
    #[test]
    fn a_registry_decode_failure_stays_corrupt() {
        let error = crate::support::plugin_sqlx_error(sqlx::Error::Decode(Box::new(
            std::io::Error::other("invalid registry column"),
        )));
        assert!(
            matches!(
                registry_failure(&error),
                DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Corrupt,
                    ..
                })
            ),
            "decode failure became retryable: {error:?}"
        );
    }
}
