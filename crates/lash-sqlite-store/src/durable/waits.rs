//! Waits, keyed promises and timers on SQLite: the `waits` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L5 (FIG-5173). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. SQLite serializes writers, so reading the
//! wait row before its conditional update needs no row lock: the wait row is
//! read and written before the owner's actor row is woken.

use std::sync::LazyLock;

use lash_durable::domain::{
    ResolveAnswer, ScopeKey, TIMER_DIGEST, WaitId, WaitKind, WaitLifecycle, WaitPurpose,
    WaitResolution, WaitRow, WaitState, WaitWrite,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, MailRefusal, Woken};
use lash_store_sql::durable::waits::WaitStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, actor_key, corrupt, wake_within};
use crate::conn::cached_execute;

/// `waits`, created by L5 (FIG-5173).
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS waits (
    wait_id TEXT PRIMARY KEY,
    owner_actor TEXT NOT NULL,
    owner_scope TEXT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_waits_kind CHECK (kind IN
        ('tool_completion', 'custom', 'process_terminal', 'signal', 'timer', 'child_session')),
    host_resolvable INTEGER NOT NULL,
    target_process TEXT,
    state TEXT NOT NULL CONSTRAINT ck_waits_state
        CHECK (state IN ('pending', 'resolved', 'timed_out', 'revoked')),
    deadline_ms INTEGER,
    resolution_digest TEXT,
    resolution_ref TEXT,
    resolved_at_ms INTEGER,
    created_epoch INTEGER NOT NULL,
    CONSTRAINT ck_waits_host CHECK (host_resolvable = (kind IN ('tool_completion', 'custom'))),
    CONSTRAINT ck_waits_target
        CHECK ((target_process IS NOT NULL) = (kind IN ('process_terminal', 'child_session'))),
    CONSTRAINT ck_waits_resolved CHECK ((state = 'resolved') = (resolution_digest IS NOT NULL)),
    CONSTRAINT ck_waits_timer_deadline CHECK (kind <> 'timer' OR deadline_ms IS NOT NULL),
    CONSTRAINT ck_waits_settled_at CHECK ((state <> 'pending') = (resolved_at_ms IS NOT NULL)),
    CONSTRAINT ck_waits_resolution_ref
        CHECK ((state = 'resolved' AND kind <> 'timer') = (resolution_ref IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS ix_waits_owner ON waits (owner_actor) WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS ix_waits_scope ON waits (owner_scope);
CREATE INDEX IF NOT EXISTS ix_waits_target ON waits (target_process)
    WHERE state = 'pending' AND kind = 'process_terminal';
CREATE INDEX IF NOT EXISTS ix_waits_child_target ON waits (target_process)
    WHERE state = 'pending' AND kind = 'child_session';
";

static SQL: LazyLock<WaitStatements> =
    LazyLock::new(|| WaitStatements::render(crate::schema_layout::MAIN));

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &WaitWrite) -> Answer<()> {
    match write {
        WaitWrite::Pin { id, scope, purpose } => {
            cached_execute(
                tx,
                SQL.pin.sql(),
                rusqlite::params![
                    id.to_hex(),
                    commit.actor.as_str(),
                    scope.stored(),
                    purpose.kind().as_str(),
                    purpose.kind().host_resolvable(),
                    purpose
                        .target_process()
                        .as_ref()
                        .map(|process| process.as_str().to_owned()),
                    purpose.deadline().map(|deadline| deadline.0),
                    commit.epoch.0
                ],
            )?;
            Ok(Ok(()))
        }
        WaitWrite::Due { id } => {
            cached_execute(
                tx,
                SQL.due.sql(),
                rusqlite::params![
                    id.to_hex(),
                    commit.actor.as_str(),
                    TIMER_DIGEST,
                    commit.now.0
                ],
            )?;
            Ok(Ok(()))
        }
        WaitWrite::RevokeScope(scope) => {
            let owners = owners_of(
                tx,
                SQL.revoke_scope.sql(),
                rusqlite::params![scope.stored(), commit.now.0],
            )?;
            wake_owners(tx, commit.actor, owners, commit.now)
        }
        WaitWrite::ResolveProcessTerminal {
            process,
            digest,
            resolution_ref,
        } => {
            let owners = owners_of(
                tx,
                SQL.resolve_process_terminal.sql(),
                rusqlite::params![process.as_str(), digest, resolution_ref, commit.now.0],
            )?;
            wake_owners(tx, commit.actor, owners, commit.now)
        }
        WaitWrite::ResolveChildSession {
            process,
            digest,
            resolution_ref,
        } => {
            let owners = owners_of(
                tx,
                SQL.resolve_child_session.sql(),
                rusqlite::params![process.as_str(), digest, resolution_ref, commit.now.0],
            )?;
            wake_owners(tx, commit.actor, owners, commit.now)
        }
    }
}

fn owners_of(
    tx: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<String>> {
    let mut owners = tx
        .prepare_cached(sql)?
        .query_map(params, |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    owners.sort();
    owners.dedup();
    Ok(owners)
}

/// Wake each owner but the committing actor. An owner that has ended or is
/// gone has nothing to wake, and never rolls the commit back.
fn wake_owners(
    tx: &Connection,
    committing: &ActorKey,
    owners: Vec<String>,
    now: DurableInstant,
) -> Answer<()> {
    for owner in owners {
        let owner = match actor_key(&owner) {
            Ok(owner) => owner,
            Err(error) => return Ok(Err(error)),
        };
        if &owner == committing {
            continue;
        }
        match wake_within(tx, &owner, false, now)? {
            Ok(_)
            | Err(DurableError::MailRefused(
                MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
            )) => {}
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(()))
}

pub(super) fn resolve(
    tx: &Connection,
    resolution: &WaitResolution,
    now: DurableInstant,
) -> Answer<(ResolveAnswer, Option<Woken>)> {
    let Some(row) = read_one(tx, &resolution.id)? else {
        return Ok(Ok((ResolveAnswer::Unknown, None)));
    };
    let row = match row {
        Ok(row) => row,
        Err(error) => return Ok(Err(error)),
    };
    if let Some(answer) = settled_answer(&row, resolution) {
        return Ok(Ok((answer, None)));
    }
    let owner: Option<String> = tx
        .prepare_cached(SQL.resolve.sql())?
        .query_row(
            rusqlite::params![
                resolution.id.to_hex(),
                resolution.digest,
                resolution.resolution_ref,
                now.0
            ],
            |row| row.get(0),
        )
        .optional()?;
    if owner.is_none() {
        return Ok(Ok((ResolveAnswer::Revoked, None)));
    }
    match wake_within(tx, &row.owner, false, now)? {
        Ok((woken, _)) => Ok(Ok((ResolveAnswer::Resolved, Some(woken)))),
        Err(DurableError::MailRefused(
            MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
        )) => Ok(Ok((ResolveAnswer::Resolved, None))),
        Err(error) => Ok(Err(error)),
    }
}

/// The answer a resolution gets without writing: a reserved kind for a host,
/// or a wait that is no longer pending. `None` when it may resolve.
pub(super) fn settled_answer(row: &WaitRow, resolution: &WaitResolution) -> Option<ResolveAnswer> {
    if resolution.by_host && !row.purpose.kind().host_resolvable() {
        return Some(ResolveAnswer::ReservedKind);
    }
    match &row.lifecycle {
        WaitLifecycle::Pending => None,
        WaitLifecycle::Resolved { digest, .. } if digest == &resolution.digest => {
            Some(ResolveAnswer::AlreadyResolved)
        }
        WaitLifecycle::TimerElapsed { .. } if resolution.digest == TIMER_DIGEST => {
            Some(ResolveAnswer::AlreadyResolved)
        }
        WaitLifecycle::Resolved { .. } | WaitLifecycle::TimerElapsed { .. } => {
            Some(ResolveAnswer::Conflict)
        }
        WaitLifecycle::TimedOut { .. } | WaitLifecycle::Revoked { .. } => {
            Some(ResolveAnswer::Revoked)
        }
    }
}

pub(super) fn pending(tx: &Connection, owner: &ActorKey) -> Answer<Vec<WaitRow>> {
    let rows = tx
        .prepare_cached(SQL.pending.sql())?
        .query_map([owner.as_str()], stored_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(decode).collect())
}

pub(super) fn wait(tx: &Connection, id: &WaitId) -> Answer<Option<WaitRow>> {
    Ok(read_one(tx, id)?.transpose())
}

fn read_one(
    tx: &Connection,
    id: &WaitId,
) -> rusqlite::Result<Option<Result<WaitRow, DurableError>>> {
    Ok(tx
        .prepare_cached(SQL.one.sql())?
        .query_row([id.to_hex()], stored_row)
        .optional()?
        .map(decode))
}

/// One row as stored, before it is decoded.
struct Stored {
    id: String,
    owner: String,
    scope: String,
    kind: String,
    target: Option<String>,
    state: String,
    deadline: Option<i64>,
    digest: Option<String>,
    resolution_ref: Option<String>,
    resolved_at: Option<i64>,
    created_epoch: i64,
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    Ok(Stored {
        id: row.get(0)?,
        owner: row.get(1)?,
        scope: row.get(2)?,
        kind: row.get(3)?,
        target: row.get(4)?,
        state: row.get(5)?,
        deadline: row.get(6)?,
        digest: row.get(7)?,
        resolution_ref: row.get(8)?,
        resolved_at: row.get(9)?,
        created_epoch: row.get(10)?,
    })
}

fn decode(stored: Stored) -> Result<WaitRow, DurableError> {
    let kind = WaitKind::parse(&stored.kind).ok_or_else(|| corrupt("wait kind", &stored.kind))?;
    let state =
        WaitState::parse(&stored.state).ok_or_else(|| corrupt("wait state", &stored.state))?;
    let purpose = WaitPurpose::decode(
        kind,
        stored
            .target
            .map(|process| {
                lash_sansio::ProcessId::parse(&process)
                    .map_err(|_| corrupt("wait target process", &process))
            })
            .transpose()?,
        stored.deadline.map(DurableInstant),
    )
    .ok_or_else(|| corrupt("wait purpose", &stored.id))?;
    let lifecycle = WaitLifecycle::decode(
        kind,
        state,
        stored.resolved_at.map(DurableInstant),
        stored.digest,
        stored.resolution_ref,
    )
    .ok_or_else(|| corrupt("wait lifecycle", &stored.id))?;
    Ok(WaitRow {
        id: WaitId::parse_hex(&stored.id).ok_or_else(|| corrupt("wait id", &stored.id))?,
        owner: actor_key(&stored.owner)?,
        scope: ScopeKey::parse(&stored.scope).map_err(|_| corrupt("wait scope", &stored.scope))?,
        purpose,
        lifecycle,
        created_epoch: Epoch(stored.created_epoch),
    })
}
