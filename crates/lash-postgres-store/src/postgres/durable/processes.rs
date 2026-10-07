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

use lash_core_execution::runtime::actor::process::scope_index;
use lash_durable::domain::{
    CANCEL_MAIL, CancelAnswer, CancelRequest, DomainRefusal, PROCESS_FORMATS, ProcessActorRow,
    ProcessWrite, SIGNAL_MAIL, ScopeKey,
};
use lash_durable::{
    ActorKey, DurableError, DurableInstant, Epoch, MailRefusal, StoreFailure, StoreFailureKind,
    Woken,
};
use lash_sansio::{CancelOrigin, ProcessId};
use sqlx::PgConnection;

use super::{Committing, SQL, corrupt, get, sqlx_failure, wake_within};
use crate::process_helpers::{
    CancelRecorded, record_cancel_tx, record_event_tx, record_terminal_tx,
};

/// A registry failure inside a durable commit: the registry row did not
/// decode or refused, which the port reports as a store failure.
fn registry_failure(error: &lash_core_execution::PluginError) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
}

fn millis(at: DurableInstant) -> u64 {
    u64::try_from(at.0).unwrap_or_default()
}

fn instant(ms: u64) -> DurableInstant {
    DurableInstant(i64::try_from(ms).unwrap_or(i64::MAX))
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
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    match created {
        Some(_) => Ok(()),
        None => Err(DurableError::MailRefused(MailRefusal::ActorExists(actor))),
    }
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
            .execute(&mut *tx)
            .await
            .map_err(sqlx_failure)?;
    }
    Ok(woken)
}

/// Append `signal` to its process actor's mailbox and wake it, inside the
/// registry transaction that admits it: what a new signal append writes.
pub(crate) async fn signal_mail_within(
    tx: &mut PgConnection,
    process: &ProcessId,
    signal: &lash_core_execution::ProcessSignal,
    now: DurableInstant,
) -> Result<(), lash_core_execution::PluginError> {
    let failure = |message: String| lash_core_execution::PluginError::Session(message);
    let actor = process_actor(process).map_err(|error| failure(error.to_string()))?;
    let body = serde_json::to_string(signal).map_err(|error| failure(error.to_string()))?;
    let (_, seq) = wake_within(tx, &actor, false, now)
        .await
        .map_err(|error| failure(error.to_string()))?;
    sqlx::query(SQL.mail.append.sql())
        .bind(actor.as_str())
        .bind(seq.0)
        .bind(SIGNAL_MAIL)
        .bind(body)
        .bind(now.0)
        .execute(&mut *tx)
        .await
        .map_err(|error| failure(error.to_string()))?;
    Ok(())
}

/// Record `origin`'s cancel of `process` unless one stands, append its
/// cancel mail and control-wake it: what a cancel request and each child of
/// a cascade batch write. A terminal process is answered `AlreadyEnded`
/// and written nothing.
pub(crate) async fn cancel_within(
    tx: &mut PgConnection,
    process: &ProcessId,
    origin: CancelOrigin,
    requester: &str,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<(CancelAnswer, Option<Woken>), DurableError> {
    let recorded = record_cancel_tx(tx, process, origin, requester, millis(now), fleet)
        .await
        .map_err(|error| registry_failure(&error))?;
    let answer = match recorded {
        CancelRecorded::Ended => return Ok((CancelAnswer::AlreadyEnded, None)),
        CancelRecorded::Requested { at_ms } => CancelAnswer::Requested { at: instant(at_ms) },
        CancelRecorded::AlreadyRequested { at_ms } => {
            CancelAnswer::AlreadyRequested { at: instant(at_ms) }
        }
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

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ProcessWrite,
) -> Result<(), DurableError> {
    match write {
        ProcessWrite::Register(rows) => {
            create_actor_within(tx, &rows.process, PROCESS_FORMATS, commit.now).await
        }
        ProcessWrite::Advance {
            process,
            expected_rev,
            driver_json,
        } => {
            let moved: Option<i64> = sqlx::query_scalar(SQL.process.advance.sql())
                .bind(process.as_str())
                .bind(i64::try_from(*expected_rev).unwrap_or(i64::MAX))
                .bind(driver_json)
                .bind(commit.epoch.0)
                .fetch_optional(&mut *tx)
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
        ProcessWrite::Emit {
            process,
            event_type,
            payload_json,
            replay_key,
        } => {
            let payload = serde_json::from_str(payload_json)
                .map_err(|_| corrupt("process event payload", payload_json))?;
            record_event_tx(
                tx,
                process,
                event_type,
                payload,
                replay_key,
                millis(commit.now),
                commit.fleet,
            )
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
                u64::try_from(commit.epoch.0).unwrap_or_default(),
                millis(commit.now),
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
    }
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
        .execute(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn request_cancel(
    tx: &mut PgConnection,
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
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let status: String = get(&row, 3)?;
    Ok(Some(ProcessActorRow {
        process: process.clone(),
        state_rev: u64::try_from(get::<i64>(&row, 0)?).unwrap_or_default(),
        driver_json: get(&row, 1)?,
        cancel_requested_at: get::<Option<i64>>(&row, 2)?.map(DurableInstant),
        terminal: !matches!(status.as_str(), "running" | "waiting"),
        cascade_cursor: get(&row, 4)?,
        written_epoch: get::<Option<i64>>(&row, 5)?.map(Epoch),
    }))
}

async fn children(
    tx: &mut PgConnection,
    statement: &str,
    scope: &ScopeKey,
    after: Option<&ProcessId>,
    limit: usize,
) -> Result<Vec<ProcessId>, DurableError> {
    let Some((kind, id)) = scope_index(scope) else {
        return Ok(Vec::new());
    };
    let ids: Vec<String> = sqlx::query_scalar(statement)
        .bind(kind)
        .bind(id)
        .bind(after.map_or("", ProcessId::as_str))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    ids.into_iter()
        .map(|id| ProcessId::parse(&id).map_err(|_| corrupt("process id", &id)))
        .collect()
}

pub(super) async fn until_children(
    tx: &mut PgConnection,
    scope: &ScopeKey,
    after: Option<&ProcessId>,
    limit: usize,
) -> Result<Vec<ProcessId>, DurableError> {
    children(tx, SQL.process.pending_children.sql(), scope, after, limit).await
}

/// The live `Until` subtree of `scope`, breadth first, one indexed page per
/// scope: each live child's own `Until` children follow it. Bounded by
/// `limit`; a parent's terminal says nothing about this being empty.
pub(super) async fn live_until_descendants(
    tx: &mut PgConnection,
    scope: &ScopeKey,
    limit: usize,
) -> Result<Vec<ProcessId>, DurableError> {
    let mut found = Vec::new();
    let mut frontier = std::collections::VecDeque::from([scope.clone()]);
    while let Some(scope) = frontier.pop_front() {
        if found.len() >= limit {
            break;
        }
        let page = children(
            tx,
            SQL.process.live_children.sql(),
            &scope,
            None,
            limit - found.len(),
        )
        .await?;
        for child in page {
            frontier.push_back(ScopeKey::Process(child.clone()));
            found.push(child);
        }
    }
    Ok(found)
}
