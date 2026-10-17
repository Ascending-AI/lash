//! Waits, keyed promises and timers on PostgreSQL: the `waits` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L5 (FIG-5173). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`. A resolution locks
//! the wait row `FOR UPDATE` before it wakes the owner's actor row: the lock
//! order is the wait row, then the actor row.

use std::sync::LazyLock;

use lash_durable::domain::{
    ResolveAnswer, ScopeKey, StoredWait, TIMER_DIGEST, WaitId, WaitKind, WaitLifecycle,
    WaitPurpose, WaitPurposeColumns, WaitResolution, WaitRow, WaitSettlement, WaitState,
    WaitTombstone, WaitWrite,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, MailRefusal, Woken};
use lash_store_sql::Dialect;
use lash_store_sql::durable::waits::WaitStatements;
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use super::steps::{Collect, Entry, Shape, Step, Ty, Value};
use super::{Committing, actor_key, corrupt, get, sqlx_failure, wake_within};

lash_store_sql::statements! {
    /// The `waits` statements only PostgreSQL issues.
    struct PostgresWaitStatements @ "durable_wait_postgres" {
        /// Wait `?1`, locked for the rest of the transaction.
        lock = "SELECT wait_id, owner_actor, owner_scope, kind, target_process, state,
                    deadline_ms, resolution_digest, resolution_ref, resolved_at_ms,
                    created_epoch, call_id, tool_id, key_name, compacted
             FROM waits WHERE wait_id = ?1
             FOR UPDATE";
    }
}

struct Sql {
    waits: WaitStatements,
    postgres: PostgresWaitStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| Sql {
    waits: WaitStatements::render(Dialect::postgres()),
    postgres: PostgresWaitStatements::render(Dialect::postgres()),
});

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &WaitWrite,
) -> Result<(), DurableError> {
    match write {
        WaitWrite::Pin { id, scope, purpose } => {
            let columns = purpose.columns();
            crate::observed_sql::query(SQL.waits.pin.sql())
                .bind(id.to_hex())
                .bind(commit.actor.as_str())
                .bind(scope.stored())
                .bind(purpose.kind().as_str())
                .bind(purpose.kind().host_resolvable())
                .bind(columns.target.map(|process| process.as_str().to_owned()))
                .bind(columns.deadline.map(|deadline| deadline.0))
                .bind(commit.epoch.0)
                .bind(columns.call.map(|call| call.as_str().to_owned()))
                .bind(columns.tool.map(|tool| tool.as_str().to_owned()))
                .bind(columns.key_name)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        WaitWrite::Due { id } => {
            crate::observed_sql::query(SQL.waits.due.sql())
                .bind(id.to_hex())
                .bind(commit.actor.as_str())
                .bind(TIMER_DIGEST)
                .bind(commit.now.0)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        WaitWrite::RevokeScope(_)
        | WaitWrite::RetireScope(_)
        | WaitWrite::ResolveProcessTerminal { .. }
        | WaitWrite::ResolveChildSession { .. } => {
            // A scope's or a target's waits settle wherever they stand: no
            // step of these programs refuses.
            let program = steps(write).unwrap_or_default();
            super::steps::run(tx, &program, &commit.steps())
                .await
                .map(drop)
        }
    }
}

/// The settlements of a scope's or a target's waits, each waking the
/// owners it settled but the committing actor.
pub(super) static REVOKE_SCOPE: Entry = Entry::new(
    || &SQL.waits.revoke_scope,
    &[Ty::Text, Ty::Now],
    Shape::Returning,
)
.collecting(Collect::Wake("owner_actor"));
pub(super) static COMPACT_SCOPE: Entry =
    Entry::new(|| &SQL.waits.compact_scope, &[Ty::Text], Shape::Exec);
pub(super) static RESOLVE_PROCESS_TERMINAL: Entry = Entry::new(
    || &SQL.waits.resolve_process_terminal,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Returning,
)
.collecting(Collect::Wake("owner_actor"));
pub(super) static RESOLVE_CHILD_SESSION: Entry = Entry::new(
    || &SQL.waits.resolve_child_session,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Returning,
)
.collecting(Collect::Wake("owner_actor"));

/// `write`'s step program: a scope's revocation or retirement, or a
/// target's resolution. A pin or a due timer has none.
pub(super) fn steps(write: &WaitWrite) -> Option<Vec<Step>> {
    let resolved = |entry: &'static Entry,
                    process: &lash_protocol_types::ProcessId,
                    digest: &str,
                    resolution_ref: &str| {
        vec![Step::any(
            entry,
            vec![
                Value::text(process.as_str()),
                Value::text(digest),
                Value::text(resolution_ref),
                Value::Now,
            ],
        )]
    };
    Some(match write {
        WaitWrite::Pin { .. } | WaitWrite::Due { .. } => return None,
        WaitWrite::RevokeScope(scope) => vec![Step::any(
            &REVOKE_SCOPE,
            vec![Value::text(scope.stored()), Value::Now],
        )],
        WaitWrite::RetireScope(scope) => vec![
            Step::any(&REVOKE_SCOPE, vec![Value::text(scope.stored()), Value::Now]),
            Step::any(&COMPACT_SCOPE, vec![Value::text(scope.stored())]),
        ],
        WaitWrite::ResolveProcessTerminal {
            process,
            digest,
            resolution_ref,
        } => resolved(&RESOLVE_PROCESS_TERMINAL, process, digest, resolution_ref),
        WaitWrite::ResolveChildSession {
            process,
            digest,
            resolution_ref,
        } => resolved(&RESOLVE_CHILD_SESSION, process, digest, resolution_ref),
    })
}

/// Wake each owner but the committing actor. An owner that has ended or is
/// gone has nothing to wake, and never rolls the commit back.
pub(super) async fn wake_owners(
    tx: &mut PgConnection,
    committing: &ActorKey,
    mut owners: Vec<String>,
    now: DurableInstant,
) -> Result<(), DurableError> {
    owners.sort();
    owners.dedup();
    for owner in owners {
        let owner = actor_key(&owner)?;
        if &owner == committing {
            continue;
        }
        match wake_within(tx, &owner, false, now).await {
            Ok(_)
            | Err(DurableError::MailRefused(
                MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
            )) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) async fn resolve(
    tx: &mut PgConnection,
    resolution: &WaitResolution,
    now: DurableInstant,
) -> Result<(ResolveAnswer, Option<Woken>), DurableError> {
    let row = crate::observed_sql::query(SQL.postgres.lock.sql())
        .bind(resolution.id.to_hex())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let Some(row) = row.as_ref().map(decode_stored).transpose()? else {
        return Ok((ResolveAnswer::Unknown, None));
    };
    if let Some(answer) = row.settled_answer(resolution) {
        return Ok((answer, None));
    }
    // A wait that may resolve is pending, so never a tombstone.
    let StoredWait::Full(row) = row else {
        return Err(corrupt("pending wait", &resolution.id.to_hex()));
    };
    let owner: Option<String> = crate::observed_sql::query_scalar(SQL.waits.resolve.sql())
        .bind(resolution.id.to_hex())
        .bind(&resolution.digest)
        .bind(&resolution.resolution_ref)
        .bind(now.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if owner.is_none() {
        return Ok((ResolveAnswer::Revoked, None));
    }
    match wake_within(tx, &row.owner, false, now).await {
        Ok((woken, _)) => Ok((ResolveAnswer::Resolved, Some(woken))),
        Err(DurableError::MailRefused(
            MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
        )) => Ok((ResolveAnswer::Resolved, None)),
        Err(error) => Err(error),
    }
}

/// Delete every wait `owner` owns.
pub(super) async fn delete_owned(
    tx: &mut PgConnection,
    owner: &ActorKey,
) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.waits.delete_owned.sql())
        .bind(owner.as_str())
        .execute(crate::observed_sql::executor(tx))
        .await
        .map(drop)
}

/// Delete every wait whose scope starts with `prefix`.
pub(super) async fn delete_under(tx: &mut PgConnection, prefix: &str) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.waits.delete_under.sql())
        .bind(prefix)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map(drop)
}

pub(super) async fn pending(
    tx: &mut PgConnection,
    owner: &ActorKey,
) -> Result<Vec<WaitRow>, DurableError> {
    let rows = crate::observed_sql::query(SQL.waits.pending.sql())
        .bind(owner.as_str())
        .fetch_all(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    rows.iter().map(decode).collect()
}

pub(super) async fn wait(
    tx: &mut PgConnection,
    id: &WaitId,
) -> Result<Option<WaitRow>, DurableError> {
    let row = crate::observed_sql::query(SQL.waits.one.sql())
        .bind(id.to_hex())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    row.as_ref().map(decode).transpose()
}

/// A full read of a wait: a tombstone is refused, since nothing folds an
/// ended turn's waits.
fn decode(row: &PgRow) -> Result<WaitRow, DurableError> {
    match decode_stored(row)? {
        StoredWait::Full(row) => Ok(row),
        StoredWait::Compacted(tombstone) => Err(corrupt(
            "wait read in full after its turn ended",
            &tombstone.id.to_hex(),
        )),
    }
}

/// A wait as a resolver reads it: in full, or its tombstone.
fn decode_stored(row: &PgRow) -> Result<StoredWait, DurableError> {
    let id: String = get(row, 0)?;
    let scope: String = get(row, 2)?;
    let kind: String = get(row, 3)?;
    let kind = WaitKind::parse(&kind).ok_or_else(|| corrupt("wait kind", &kind))?;
    let target: Option<String> = get(row, 4)?;
    let state: String = get(row, 5)?;
    let state = WaitState::parse(&state).ok_or_else(|| corrupt("wait state", &state))?;
    let wait_id = WaitId::parse_hex(&id).ok_or_else(|| corrupt("wait id", &id))?;
    if get::<bool>(row, 14)? {
        let settlement = WaitSettlement::decode(
            state,
            get::<Option<i64>>(row, 9)?.map(DurableInstant),
            get(row, 7)?,
        )
        .ok_or_else(|| corrupt("wait tombstone", &id))?;
        return Ok(StoredWait::Compacted(WaitTombstone {
            id: wait_id,
            kind,
            settlement,
        }));
    }
    let call: Option<String> = get(row, 11)?;
    let purpose = WaitPurpose::decode(
        kind,
        WaitPurposeColumns {
            target: target
                .map(|process| {
                    lash_protocol_types::ProcessId::parse(&process)
                        .map_err(|_| corrupt("wait target process", &process))
                })
                .transpose()?,
            call: call
                .map(|call| {
                    lash_protocol_types::ToolCallId::parse(&call)
                        .map_err(|_| corrupt("wait call", &call))
                })
                .transpose()?,
            tool: get::<Option<String>>(row, 12)?.map(lash_protocol_types::ToolId::new),
            key_name: get(row, 13)?,
            deadline: get::<Option<i64>>(row, 6)?.map(DurableInstant),
        },
    )
    .ok_or_else(|| corrupt("wait purpose", &id))?;
    let lifecycle = WaitLifecycle::decode(
        kind,
        state,
        get::<Option<i64>>(row, 9)?.map(DurableInstant),
        get(row, 7)?,
        get(row, 8)?,
    )
    .ok_or_else(|| corrupt("wait lifecycle", &id))?;
    let owner: Option<String> = get(row, 1)?;
    let owner = owner.ok_or_else(|| corrupt("wait owner", &id))?;
    Ok(StoredWait::Full(WaitRow {
        id: wait_id,
        owner: actor_key(&owner)?,
        scope: ScopeKey::parse(&scope).map_err(|_| corrupt("wait scope", &scope))?,
        purpose,
        lifecycle,
        created_epoch: Epoch(get(row, 10)?),
    }))
}
