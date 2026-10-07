//! Process actors, cancel, terminal and cascade on SQLite: the `processes` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.
//!
//! The actor columns live on the registry's `processes` row; the terminal
//! and the cancel request are the registry's own writes on this commit's
//! connection, so the registry and the actor never disagree.

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
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, SQL, wake_within};
use crate::SqliteProcessRegistry;
use crate::conn::cached_execute;
use crate::process_registry::actor::CancelRecorded;

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

/// Create `process`'s actor, ready, in the caller's transaction: a process
/// registration's other half.
pub(crate) fn create_actor_within(
    tx: &Connection,
    process: &ProcessId,
    formats: &str,
    now: DurableInstant,
) -> Answer<()> {
    let actor = match ActorKey::process(process.as_str()) {
        Ok(actor) => actor,
        Err(_) => return Ok(Err(super::corrupt("process actor key", process.as_str()))),
    };
    let created = tx
        .prepare_cached(SQL.actor.create.sql())?
        .query_row(
            rusqlite::params![actor.as_str(), actor.kind().as_str(), formats, now.0],
            |_| Ok(()),
        )
        .optional()?;
    Ok(match created {
        Some(()) => Ok(()),
        None => Err(DurableError::MailRefused(MailRefusal::ActorExists(actor))),
    })
}

/// Record `origin`'s cancel of `process` unless one stands, append its
/// cancel mail and control-wake it: what a cancel request and each child of
/// a cascade batch write. A terminal process is answered `AlreadyEnded`
/// and written nothing.
pub(crate) fn cancel_within(
    tx: &Connection,
    process: &ProcessId,
    origin: CancelOrigin,
    requester: &str,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Answer<(CancelAnswer, Option<Woken>)> {
    let recorded = match SqliteProcessRegistry::record_cancel_conn(
        tx,
        process,
        origin,
        requester,
        millis(now),
        fleet,
    ) {
        Ok(recorded) => recorded,
        Err(error) => return Ok(Err(registry_failure(&error))),
    };
    let answer = match recorded {
        CancelRecorded::Ended => return Ok(Ok((CancelAnswer::AlreadyEnded, None))),
        CancelRecorded::Requested { at_ms } => CancelAnswer::Requested { at: instant(at_ms) },
        CancelRecorded::AlreadyRequested { at_ms } => {
            CancelAnswer::AlreadyRequested { at: instant(at_ms) }
        }
    };
    let woken = match cancel_mail_within(
        tx,
        process,
        origin,
        requester,
        matches!(answer, CancelAnswer::Requested { .. }),
        now,
    )? {
        Ok(woken) => woken,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok((answer, Some(woken))))
}

/// Control-wake `process` and, for a newly recorded request, append its
/// cancel mail: the registry's cancel record commits with both.
pub(crate) fn cancel_mail_within(
    tx: &Connection,
    process: &ProcessId,
    origin: CancelOrigin,
    requester: &str,
    newly_requested: bool,
    now: DurableInstant,
) -> Answer<Woken> {
    let actor = match ActorKey::process(process.as_str()) {
        Ok(actor) => actor,
        Err(_) => return Ok(Err(super::corrupt("process actor key", process.as_str()))),
    };
    let (woken, seq) = match wake_within(tx, &actor, true, now)? {
        Ok(woken) => woken,
        Err(error) => return Ok(Err(error)),
    };
    if newly_requested {
        let body = serde_json::json!({ "origin": origin, "requester": requester }).to_string();
        cached_execute(
            tx,
            SQL.mail.append.sql(),
            rusqlite::params![actor.as_str(), seq.0, CANCEL_MAIL, body, now.0],
        )?;
    }
    Ok(Ok(woken))
}

/// Append `signal` to its process actor's mailbox and wake it, inside the
/// registry transaction that admits it: what a new signal append writes.
pub(crate) fn signal_mail_within(
    tx: &Connection,
    process: &ProcessId,
    signal: &lash_core_execution::ProcessSignal,
    now: DurableInstant,
) -> Result<(), lash_core_execution::PluginError> {
    let failure = |message: String| lash_core_execution::PluginError::Session(message);
    let actor = ActorKey::process(process.as_str()).map_err(|error| failure(error.to_string()))?;
    let body = serde_json::to_string(signal).map_err(|error| failure(error.to_string()))?;
    let (_, seq) = wake_within(tx, &actor, false, now)
        .map_err(|error| failure(error.to_string()))?
        .map_err(|error| failure(error.to_string()))?;
    cached_execute(
        tx,
        SQL.mail.append.sql(),
        rusqlite::params![actor.as_str(), seq.0, SIGNAL_MAIL, body, now.0],
    )
    .map_err(|error| failure(error.to_string()))?;
    Ok(())
}

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &ProcessWrite) -> Answer<()> {
    match write {
        ProcessWrite::Register(rows) => {
            create_actor_within(tx, &rows.process, PROCESS_FORMATS, commit.now)
        }
        ProcessWrite::Advance {
            process,
            expected_rev,
            driver_json,
        } => {
            let moved = tx
                .prepare_cached(SQL.process.advance.sql())?
                .query_row(
                    rusqlite::params![
                        process.as_str(),
                        i64::try_from(*expected_rev).unwrap_or(i64::MAX),
                        driver_json,
                        commit.epoch.0
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if moved.is_some() {
                return Ok(Ok(()));
            }
            let found = match self::process(tx, process)? {
                Ok(row) => row.filter(|row| !row.terminal).map(|row| row.state_rev),
                Err(error) => return Ok(Err(error)),
            };
            Ok(Err(DurableError::Domain(
                DomainRefusal::ProcessRevConflict {
                    process: process.clone(),
                    expected: *expected_rev,
                    found,
                },
            )))
        }
        ProcessWrite::Emit {
            process,
            event_type,
            payload_json,
            replay_key,
        } => {
            let Ok(payload) = serde_json::from_str(payload_json) else {
                return Ok(Err(super::corrupt("process event payload", payload_json)));
            };
            Ok(SqliteProcessRegistry::record_event_conn(
                tx,
                process,
                event_type,
                payload,
                replay_key,
                millis(commit.now),
                commit.fleet,
            )
            .map_err(|error| registry_failure(&error)))
        }
        ProcessWrite::Terminal {
            process,
            outcome_json,
        } => {
            let output: lash_core_execution::ProcessAwaitOutput =
                match serde_json::from_str(outcome_json) {
                    Ok(output) => output,
                    Err(_) => return Ok(Err(super::corrupt("process terminal", outcome_json))),
                };
            let ended = match SqliteProcessRegistry::record_terminal_conn(
                tx,
                process,
                &output,
                u64::try_from(commit.epoch.0).unwrap_or_default(),
                millis(commit.now),
                commit.fleet,
            ) {
                Ok(ended) => ended,
                Err(error) => return Ok(Err(registry_failure(&error))),
            };
            if ended {
                cached_execute(
                    tx,
                    SQL.process.set_cursor.sql(),
                    rusqlite::params![process.as_str(), "", commit.epoch.0],
                )?;
            }
            Ok(Ok(()))
        }
        ProcessWrite::CascadeBatch {
            scope,
            children,
            origin,
            requester,
            cursor,
        } => {
            for child in children {
                if let Err(error) =
                    cancel_within(tx, child, *origin, requester, commit.now, commit.fleet)?
                {
                    return Ok(Err(error));
                }
            }
            if let ScopeKey::Process(owner) = scope
                && ActorKey::process(owner.as_str()).is_ok_and(|actor| &actor == commit.actor)
            {
                cached_execute(
                    tx,
                    SQL.process.set_cursor.sql(),
                    rusqlite::params![owner.as_str(), cursor.as_deref(), commit.epoch.0],
                )?;
            }
            Ok(Ok(()))
        }
    }
}

pub(super) fn request_cancel(
    tx: &Connection,
    request: &CancelRequest,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Answer<(CancelAnswer, Option<Woken>)> {
    cancel_within(
        tx,
        &request.process,
        request.origin,
        &request.requester,
        now,
        fleet,
    )
}

pub(super) fn process(tx: &Connection, process: &ProcessId) -> Answer<Option<ProcessActorRow>> {
    let row = tx
        .prepare_cached(SQL.process.row.sql())?
        .query_row([process.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .optional()?;
    Ok(Ok(row.map(
        |(state_rev, driver_json, cancel, status, cascade_cursor, written_epoch)| ProcessActorRow {
            process: process.clone(),
            state_rev: u64::try_from(state_rev).unwrap_or_default(),
            driver_json,
            cancel_requested_at: cancel.map(DurableInstant),
            terminal: !matches!(status.as_str(), "running" | "waiting"),
            cascade_cursor,
            written_epoch: written_epoch.map(Epoch),
        },
    )))
}

fn children(
    tx: &Connection,
    statement: &str,
    scope: &ScopeKey,
    after: Option<&ProcessId>,
    limit: usize,
) -> Answer<Vec<ProcessId>> {
    let Some((kind, id)) = scope_index(scope) else {
        return Ok(Ok(Vec::new()));
    };
    let after = after.map_or("", ProcessId::as_str);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let ids = tx
        .prepare_cached(statement)?
        .query_map(rusqlite::params![kind, id, after, limit], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .map(|id| ProcessId::parse(&id).map_err(|_| super::corrupt("process id", &id)))
        .collect())
}

pub(super) fn until_children(
    tx: &Connection,
    scope: &ScopeKey,
    after: Option<&ProcessId>,
    limit: usize,
) -> Answer<Vec<ProcessId>> {
    children(tx, SQL.process.pending_children.sql(), scope, after, limit)
}

/// The live `Until` subtree of `scope`, breadth first, one indexed page per
/// scope: each live child's own `Until` children follow it. Bounded by
/// `limit`; a parent's terminal says nothing about this being empty.
pub(super) fn live_until_descendants(
    tx: &Connection,
    scope: &ScopeKey,
    limit: usize,
) -> Answer<Vec<ProcessId>> {
    let mut found = Vec::new();
    let mut frontier = std::collections::VecDeque::from([scope.clone()]);
    while let Some(scope) = frontier.pop_front() {
        if found.len() >= limit {
            break;
        }
        let page = match children(
            tx,
            SQL.process.live_children.sql(),
            &scope,
            None,
            limit - found.len(),
        )? {
            Ok(page) => page,
            Err(error) => return Ok(Err(error)),
        };
        for child in page {
            frontier.push_back(ScopeKey::Process(child.clone()));
            found.push(child);
        }
    }
    Ok(Ok(found))
}
