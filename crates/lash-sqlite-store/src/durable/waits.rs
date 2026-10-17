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
    ResolveAnswer, ScopeKey, StoredWait, TIMER_DIGEST, WaitId, WaitKind, WaitLifecycle,
    WaitPurpose, WaitPurposeColumns, WaitResolution, WaitRow, WaitSettlement, WaitState,
    WaitTombstone, WaitWrite,
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
    -- NULL exactly on a tombstone: its turn ended, and it wakes nobody.
    owner_actor TEXT,
    owner_scope TEXT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_waits_kind CHECK (kind IN
        ('tool_completion', 'engine_key', 'process_terminal', 'timer', 'child_session')),
    host_resolvable INTEGER NOT NULL,
    target_process TEXT,
    state TEXT NOT NULL CONSTRAINT ck_waits_state
        CHECK (state IN ('pending', 'resolved', 'timed_out', 'revoked')),
    deadline_ms INTEGER,
    resolution_digest TEXT,
    resolution_ref TEXT,
    resolved_at_ms INTEGER,
    created_epoch INTEGER NOT NULL,
    call_id TEXT,
    tool_id TEXT,
    key_name TEXT,
    -- A tombstone: the id, kind, state, digest and settlement time a late
    -- resolution is answered from, once its turn ended. Never pending.
    compacted INTEGER NOT NULL DEFAULT FALSE CONSTRAINT ck_waits_compacted_bool
        CHECK (compacted IN (FALSE, TRUE)),
    CONSTRAINT ck_waits_compacted CHECK (NOT compacted OR (state <> 'pending'
        AND owner_actor IS NULL AND target_process IS NULL AND deadline_ms IS NULL
        AND resolution_ref IS NULL AND call_id IS NULL AND tool_id IS NULL
        AND key_name IS NULL)),
    CONSTRAINT ck_waits_owner CHECK (compacted OR owner_actor IS NOT NULL),
    CONSTRAINT ck_waits_host CHECK (host_resolvable = (kind IN ('tool_completion', 'engine_key'))),
    CONSTRAINT ck_waits_target CHECK (compacted
        OR (target_process IS NOT NULL) = (kind IN ('process_terminal', 'child_session'))),
    CONSTRAINT ck_waits_resolved CHECK ((state = 'resolved') = (resolution_digest IS NOT NULL)),
    CONSTRAINT ck_waits_timer_deadline
        CHECK (compacted OR kind <> 'timer' OR deadline_ms IS NOT NULL),
    CONSTRAINT ck_waits_settled_at CHECK ((state <> 'pending') = (resolved_at_ms IS NOT NULL)),
    CONSTRAINT ck_waits_resolution_ref CHECK (compacted
        OR (state = 'resolved' AND kind <> 'timer') = (resolution_ref IS NOT NULL)),
    CONSTRAINT ck_waits_call CHECK (compacted OR (call_id IS NOT NULL) = (kind = 'tool_completion')
        AND (tool_id IS NOT NULL) = (kind = 'tool_completion')),
    CONSTRAINT ck_waits_key_name
        CHECK (compacted OR (key_name IS NOT NULL) = (kind = 'engine_key'))
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
            let columns = purpose.columns();
            cached_execute(
                tx,
                SQL.pin.sql(),
                rusqlite::params![
                    id.to_hex(),
                    commit.actor.as_str(),
                    scope.stored(),
                    purpose.kind().as_str(),
                    purpose.kind().host_resolvable(),
                    columns
                        .target
                        .as_ref()
                        .map(lash_protocol_types::ProcessId::as_str),
                    columns.deadline.map(|deadline| deadline.0),
                    commit.epoch.0,
                    columns
                        .call
                        .as_ref()
                        .map(lash_protocol_types::ToolCallId::as_str),
                    columns
                        .tool
                        .as_ref()
                        .map(lash_protocol_types::ToolId::as_str),
                    columns.key_name
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
        WaitWrite::RetireScope(scope) => {
            let owners = owners_of(
                tx,
                SQL.revoke_scope.sql(),
                rusqlite::params![scope.stored(), commit.now.0],
            )?;
            match wake_owners(tx, commit.actor, owners, commit.now)? {
                Ok(()) => {}
                Err(error) => return Ok(Err(error)),
            }
            cached_execute(tx, SQL.compact_scope.sql(), [scope.stored()])?;
            Ok(Ok(()))
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
    let Some(row) = tx
        .prepare_cached(SQL.one.sql())?
        .query_row([resolution.id.to_hex()], stored_row)
        .optional()?
    else {
        return Ok(Ok((ResolveAnswer::Unknown, None)));
    };
    let row = match decode_stored(row) {
        Ok(row) => row,
        Err(error) => return Ok(Err(error)),
    };
    if let Some(answer) = row.settled_answer(resolution) {
        return Ok(Ok((answer, None)));
    }
    // A wait that may resolve is pending, so never a tombstone.
    let StoredWait::Full(row) = row else {
        return Ok(Err(corrupt("pending wait", &resolution.id.to_hex())));
    };
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

/// Delete every wait `owner` owns.
pub(super) fn delete_owned(tx: &Connection, owner: &ActorKey) -> rusqlite::Result<()> {
    cached_execute(tx, SQL.delete_owned.sql(), [owner.as_str()]).map(drop)
}

/// Delete every wait whose scope starts with `prefix`.
pub(super) fn delete_under(tx: &Connection, prefix: &str) -> rusqlite::Result<()> {
    cached_execute(tx, SQL.delete_under.sql(), [prefix]).map(drop)
}

pub(super) fn pending(tx: &Connection, owner: &ActorKey) -> Answer<Vec<WaitRow>> {
    let rows = tx
        .prepare_cached(SQL.pending.sql())?
        .query_map([owner.as_str()], stored_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(decode).collect())
}

pub(super) fn wait(tx: &Connection, id: &WaitId) -> Answer<Option<WaitRow>> {
    Ok(tx
        .prepare_cached(SQL.one.sql())?
        .query_row([id.to_hex()], stored_row)
        .optional()?
        .map(decode)
        .transpose())
}

/// One row as stored, before it is decoded.
struct Stored {
    id: String,
    owner: Option<String>,
    scope: String,
    kind: String,
    target: Option<String>,
    state: String,
    deadline: Option<i64>,
    digest: Option<String>,
    resolution_ref: Option<String>,
    resolved_at: Option<i64>,
    created_epoch: i64,
    call: Option<String>,
    tool: Option<String>,
    key_name: Option<String>,
    compacted: bool,
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
        call: row.get(11)?,
        tool: row.get(12)?,
        key_name: row.get(13)?,
        compacted: row.get(14)?,
    })
}

/// A full read of a wait: a tombstone is refused, since nothing folds an
/// ended turn's waits.
fn decode(stored: Stored) -> Result<WaitRow, DurableError> {
    match decode_stored(stored)? {
        StoredWait::Full(row) => Ok(row),
        StoredWait::Compacted(tombstone) => Err(corrupt(
            "wait read in full after its turn ended",
            &tombstone.id.to_hex(),
        )),
    }
}

/// A wait as a resolver reads it: in full, or its tombstone.
fn decode_stored(stored: Stored) -> Result<StoredWait, DurableError> {
    let kind = WaitKind::parse(&stored.kind).ok_or_else(|| corrupt("wait kind", &stored.kind))?;
    let state =
        WaitState::parse(&stored.state).ok_or_else(|| corrupt("wait state", &stored.state))?;
    let id = WaitId::parse_hex(&stored.id).ok_or_else(|| corrupt("wait id", &stored.id))?;
    if stored.compacted {
        let settlement =
            WaitSettlement::decode(state, stored.resolved_at.map(DurableInstant), stored.digest)
                .ok_or_else(|| corrupt("wait tombstone", &stored.id))?;
        return Ok(StoredWait::Compacted(WaitTombstone {
            id,
            kind,
            settlement,
        }));
    }
    let owner = stored
        .owner
        .ok_or_else(|| corrupt("wait owner", &stored.id))?;
    let purpose = WaitPurpose::decode(
        kind,
        WaitPurposeColumns {
            target: stored
                .target
                .map(|process| {
                    lash_protocol_types::ProcessId::parse(&process)
                        .map_err(|_| corrupt("wait target process", &process))
                })
                .transpose()?,
            call: stored
                .call
                .map(|call| {
                    lash_protocol_types::ToolCallId::parse(&call)
                        .map_err(|_| corrupt("wait call", &call))
                })
                .transpose()?,
            tool: stored.tool.map(lash_protocol_types::ToolId::new),
            key_name: stored.key_name,
            deadline: stored.deadline.map(DurableInstant),
        },
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
    Ok(StoredWait::Full(WaitRow {
        id,
        owner: actor_key(&owner)?,
        scope: ScopeKey::parse(&stored.scope).map_err(|_| corrupt("wait scope", &stored.scope))?,
        purpose,
        lifecycle,
        created_epoch: Epoch(stored.created_epoch),
    }))
}
