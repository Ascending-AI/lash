//! Waits, keyed promises and timers on PostgreSQL: the `waits` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L5 (FIG-5173). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::domain::{ResolveAnswer, WaitId, WaitResolution, WaitRow, WaitWrite};
use lash_durable::{ActorKey, DurableError, DurableInstant, Woken};
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &WaitWrite,
) -> Result<(), DurableError> {
    todo!("L5 (FIG-5173): pin, settle or revoke waits on PostgreSQL")
}

pub(super) async fn resolve(
    _tx: &mut PgConnection,
    _resolution: &WaitResolution,
    _now: DurableInstant,
) -> Result<(ResolveAnswer, Option<Woken>), DurableError> {
    todo!(
        "L5 (FIG-5173): resolve a wait from pending, first winner, and wake its owner on PostgreSQL"
    )
}

pub(super) async fn pending(
    _tx: &mut PgConnection,
    _owner: &ActorKey,
) -> Result<Vec<WaitRow>, DurableError> {
    todo!("L5 (FIG-5173): read an actor's pending waits on PostgreSQL")
}

pub(super) async fn wait(
    _tx: &mut PgConnection,
    _id: &WaitId,
) -> Result<Option<WaitRow>, DurableError> {
    todo!("L5 (FIG-5173): read one wait on PostgreSQL")
}
