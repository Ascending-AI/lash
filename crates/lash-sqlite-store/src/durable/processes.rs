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
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, SQL, integer, wake_within};
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

fn millis(at: DurableInstant) -> rusqlite::Result<u64> {
    integer(at.0)
}

fn instant(ms: u64) -> rusqlite::Result<DurableInstant> {
    integer(ms).map(DurableInstant)
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
        millis(now)?,
        fleet,
    ) {
        Ok(recorded) => recorded,
        Err(error) => return Ok(Err(registry_failure(&error))),
    };
    let answer = match recorded {
        CancelRecorded::Ended => return Ok(Ok((CancelAnswer::AlreadyEnded, None))),
        CancelRecorded::Requested { at_ms } => CancelAnswer::Requested {
            at: instant(at_ms)?,
        },
        CancelRecorded::AlreadyRequested { at_ms } => CancelAnswer::AlreadyRequested {
            at: instant(at_ms)?,
        },
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

/// Register a store-local start on the commit's connection: its row, its
/// observers and its actor, ready, as the registrar applies them. A start
/// the registrar refuses, or whose key another process holds, refuses the
/// commit.
fn register_within(
    tx: &Connection,
    commit: &Committing<'_>,
    rows: &ProcessStartRows,
) -> Result<(), DurableError> {
    let Ok(staged) = lash_core_execution::runtime::StagedRegistration::decode(rows) else {
        return Err(super::corrupt(
            "process start rows",
            &rows.registration_json,
        ));
    };
    let refused = |reason: String| {
        DurableError::Domain(DomainRefusal::ProcessStartRefused {
            process: rows.process.clone(),
            reason,
        })
    };
    match SqliteProcessRegistry::apply_registration_conn(
        tx,
        staged.registration,
        staged.observers,
        staged.process_id,
        false,
        staged.prepared_at_ms,
        commit.fleet,
    ) {
        Ok(receipt) if receipt.record.id == rows.process => Ok(()),
        Ok(receipt) => Err(refused(format!(
            "process `{}` already holds its start key",
            receipt.record.id
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

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &ProcessWrite) -> Answer<()> {
    match write {
        ProcessWrite::Register(rows) => Ok(register_within(tx, commit, rows)),
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
                        integer::<i64>(*expected_rev)?,
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
        ProcessWrite::Published { process, through } => {
            cached_execute(
                tx,
                SQL.process.publish.sql(),
                rusqlite::params![process.as_str(), integer::<i64>(*through)?],
            )?;
            Ok(Ok(()))
        }
        ProcessWrite::AppendEvent {
            process,
            event_type,
            payload_json,
            replay_key,
        } => {
            let Ok(payload) = serde_json::from_str(payload_json) else {
                return Ok(Err(super::corrupt("process event payload", payload_json)));
            };
            let request = match lash_core_execution::ProcessEventAppendRequest::from_stored(
                event_type.as_str(),
                payload,
                replay_key.as_str(),
            ) {
                Ok(request) => request,
                Err(error) => return Ok(Err(registry_failure(&error))),
            };
            Ok(SqliteProcessRegistry::record_event_conn(
                tx,
                process,
                request,
                millis(commit.now)?,
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
                integer::<u64>(commit.epoch.0)?,
                millis(commit.now)?,
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
        ProcessWrite::ScopeClosed { scope } => close_scope(tx, commit, scope),
    }
}

/// Write `scope`'s closure fact, then read its unmarked `Until` children in
/// this transaction: the one writer serializes it with every registration,
/// so a child that committed first is read here, and any later one is
/// refused. A turn scope with one left is recorded as ending.
fn close_scope(tx: &Connection, commit: &Committing<'_>, scope: &ScopeKey) -> Answer<()> {
    let Some(closed) = scope_id(scope) else {
        return Ok(Ok(()));
    };
    if let Err(error) = crate::process_registry::parent_end::record_conn(
        tx,
        &closed,
        millis(commit.now)?,
        commit.fleet,
    ) {
        return Ok(Err(registry_failure(&error)));
    }
    let ScopeKey::Turn(session, _) = scope else {
        return Ok(Ok(()));
    };
    match until_children(tx, scope, None, 1)? {
        Ok(left) if left.is_empty() => Ok(Ok(())),
        Ok(_) => super::session_close::record_ending(tx, commit, session, scope),
        Err(error) => Ok(Err(error)),
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
                row.get::<_, i64>(6)?,
            ))
        })
        .optional()?;
    Ok(Ok(row
        .map(
            |(state_rev, driver_json, cancel, status, cascade_cursor, written_epoch, published)| {
                Ok::<_, rusqlite::Error>(ProcessActorRow {
                    process: process.clone(),
                    state_rev: integer::<u64>(state_rev)?,
                    driver_json,
                    cancel_requested_at: cancel.map(DurableInstant),
                    terminal: !matches!(status.as_str(), "running" | "waiting"),
                    cascade_cursor,
                    written_epoch: written_epoch.map(Epoch),
                    published_event_sequence: integer::<u64>(published)?,
                })
            },
        )
        .transpose()?))
}

pub(super) fn until_children(
    tx: &Connection,
    scope: &ScopeKey,
    after: Option<&ProcessId>,
    limit: usize,
) -> Answer<Vec<ProcessId>> {
    let Some((kind, id)) = scope_index(scope) else {
        return Ok(Ok(Vec::new()));
    };
    let after = after.map_or("", ProcessId::as_str);
    let limit = integer::<i64>(limit)?;
    let ids = tx
        .prepare_cached(SQL.process.pending_children.sql())?
        .query_map(rusqlite::params![kind, id, after, limit], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .map(|id| ProcessId::parse(&id).map_err(|_| super::corrupt("process id", &id)))
        .collect())
}

/// Up to `limit` live processes in the `Until` subtree of `scope`, by id:
/// one walk through every process row below it, ended or not, and for a
/// session through its turn and session-operation scopes too. A parent's
/// terminal says nothing about this being empty.
pub(super) fn live_until_descendants(
    tx: &Connection,
    scope: &ScopeKey,
    limit: usize,
) -> Answer<Vec<ProcessId>> {
    let Some(roots) = subtree_roots(scope) else {
        return Ok(Ok(Vec::new()));
    };
    let ids = tx
        .prepare_cached(SQL.process.live_descendants.sql())?
        .query_map(
            rusqlite::params![
                roots.kind,
                roots.id,
                roots.turns,
                roots.operations,
                integer::<i64>(limit)?
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .map(|id| ProcessId::parse(&id).map_err(|_| super::corrupt("process id", &id)))
        .collect())
}
