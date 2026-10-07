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
    KeyVersion, ResolveAnswer, ScopeKey, TIMER_DIGEST, WaitId, WaitKind, WaitResolution, WaitRow,
    WaitState, WaitWrite,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, MailRefusal, Woken};
use lash_store_sql::Dialect;
use lash_store_sql::durable::waits::WaitStatements;
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use super::{Committing, actor_key, corrupt, get, sqlx_failure, wake_within};

lash_store_sql::statements! {
    /// The `waits` statements only PostgreSQL issues.
    struct PostgresWaitStatements @ "durable_wait_postgres" {
        /// Wait `?1`, locked for the rest of the transaction.
        lock = "SELECT wait_id, owner_actor, owner_scope, kind, target_process, state,
                    deadline_ms, resolution_digest, resolution_ref, resolved_at_ms,
                    key_version, created_epoch
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
        WaitWrite::Pin {
            id,
            scope,
            kind,
            target_process,
            deadline,
            key_version,
        } => {
            sqlx::query(SQL.waits.pin.sql())
                .bind(id.to_hex())
                .bind(commit.actor.as_str())
                .bind(scope.stored())
                .bind(kind.as_str())
                .bind(kind.host_resolvable())
                .bind(
                    target_process
                        .as_ref()
                        .map(|process| process.as_str().to_owned()),
                )
                .bind(deadline.map(|deadline| deadline.0))
                .bind(key_version.map(|version| i32::from(version.0)))
                .bind(commit.epoch.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        WaitWrite::Due { id } => {
            sqlx::query(SQL.waits.due.sql())
                .bind(id.to_hex())
                .bind(commit.actor.as_str())
                .bind(TIMER_DIGEST)
                .bind(commit.now.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        WaitWrite::RevokeScope(scope) => {
            let owners: Vec<String> = sqlx::query_scalar(SQL.waits.revoke_scope.sql())
                .bind(scope.stored())
                .bind(commit.now.0)
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            wake_owners(tx, commit.actor, owners, commit.now).await
        }
        WaitWrite::ResolveProcessTerminal {
            process,
            digest,
            resolution_ref,
        } => {
            let owners: Vec<String> = sqlx::query_scalar(SQL.waits.resolve_process_terminal.sql())
                .bind(process.as_str())
                .bind(digest)
                .bind(resolution_ref)
                .bind(commit.now.0)
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            wake_owners(tx, commit.actor, owners, commit.now).await
        }
        WaitWrite::ResolveChildSession {
            process,
            digest,
            resolution_ref,
        } => {
            let owners: Vec<String> = sqlx::query_scalar(SQL.waits.resolve_child_session.sql())
                .bind(process.as_str())
                .bind(digest)
                .bind(resolution_ref)
                .bind(commit.now.0)
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            wake_owners(tx, commit.actor, owners, commit.now).await
        }
    }
}

/// Wake each owner but the committing actor. An owner that has ended or is
/// gone has nothing to wake, and never rolls the commit back.
async fn wake_owners(
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
    let row = sqlx::query(SQL.postgres.lock.sql())
        .bind(resolution.id.to_hex())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let Some(row) = row.as_ref().map(decode).transpose()? else {
        return Ok((ResolveAnswer::UnknownOrRevoked, None));
    };
    if let Some(answer) = settled_answer(&row, resolution) {
        return Ok((answer, None));
    }
    let owner: Option<String> = sqlx::query_scalar(SQL.waits.resolve.sql())
        .bind(resolution.id.to_hex())
        .bind(&resolution.digest)
        .bind(&resolution.resolution_ref)
        .bind(now.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    if owner.is_none() {
        return Ok((ResolveAnswer::UnknownOrRevoked, None));
    }
    match wake_within(tx, &row.owner, false, now).await {
        Ok((woken, _)) => Ok((ResolveAnswer::Resolved, Some(woken))),
        Err(DurableError::MailRefused(
            MailRefusal::ActorTerminal(_) | MailRefusal::UnknownActor(_),
        )) => Ok((ResolveAnswer::Resolved, None)),
        Err(error) => Err(error),
    }
}

/// The answer a resolution gets without writing: a reserved kind for a host,
/// or a wait that is no longer pending. `None` when it may resolve.
fn settled_answer(row: &WaitRow, resolution: &WaitResolution) -> Option<ResolveAnswer> {
    if resolution.by_host && !row.kind.host_resolvable() {
        return Some(ResolveAnswer::ReservedKind);
    }
    match row.state {
        WaitState::Pending => None,
        WaitState::Resolved if row.resolution_digest.as_deref() == Some(&resolution.digest) => {
            Some(ResolveAnswer::AlreadyResolved)
        }
        WaitState::Resolved => Some(ResolveAnswer::Conflict),
        WaitState::TimedOut | WaitState::Revoked => Some(ResolveAnswer::UnknownOrRevoked),
    }
}

pub(super) async fn pending(
    tx: &mut PgConnection,
    owner: &ActorKey,
) -> Result<Vec<WaitRow>, DurableError> {
    let rows = sqlx::query(SQL.waits.pending.sql())
        .bind(owner.as_str())
        .fetch_all(tx)
        .await
        .map_err(sqlx_failure)?;
    rows.iter().map(decode).collect()
}

pub(super) async fn wait(
    tx: &mut PgConnection,
    id: &WaitId,
) -> Result<Option<WaitRow>, DurableError> {
    let row = sqlx::query(SQL.waits.one.sql())
        .bind(id.to_hex())
        .fetch_optional(tx)
        .await
        .map_err(sqlx_failure)?;
    row.as_ref().map(decode).transpose()
}

fn decode(row: &PgRow) -> Result<WaitRow, DurableError> {
    let id: String = get(row, 0)?;
    let scope: String = get(row, 2)?;
    let kind: String = get(row, 3)?;
    let target: Option<String> = get(row, 4)?;
    let state: String = get(row, 5)?;
    let key_version: Option<i32> = get(row, 10)?;
    Ok(WaitRow {
        id: WaitId::parse_hex(&id).ok_or_else(|| corrupt("wait id", &id))?,
        owner: actor_key(&get::<String>(row, 1)?)?,
        scope: ScopeKey::parse(&scope).map_err(|_| corrupt("wait scope", &scope))?,
        kind: WaitKind::parse(&kind).ok_or_else(|| corrupt("wait kind", &kind))?,
        target_process: target
            .map(|process| {
                lash_sansio::ProcessId::parse(&process)
                    .map_err(|_| corrupt("wait target process", &process))
            })
            .transpose()?,
        state: WaitState::parse(&state).ok_or_else(|| corrupt("wait state", &state))?,
        deadline: get::<Option<i64>>(row, 6)?.map(DurableInstant),
        resolution_digest: get(row, 7)?,
        resolution_ref: get(row, 8)?,
        resolved_at: get::<Option<i64>>(row, 9)?.map(DurableInstant),
        key_version: key_version
            .map(|version| {
                u16::try_from(version)
                    .map(KeyVersion)
                    .map_err(|_| corrupt("wait key version", &version.to_string()))
            })
            .transpose()?,
        created_epoch: Epoch(get(row, 11)?),
    })
}
