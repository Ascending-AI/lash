//! Process actors, cancel, terminal and cascade on PostgreSQL: the `processes` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::domain::{CancelAnswer, CancelRequest, ProcessActorRow, ProcessWrite, ScopeKey};
use lash_durable::{DurableError, DurableInstant, Woken};
use lash_sansio::ProcessId;
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &ProcessWrite,
) -> Result<(), DurableError> {
    todo!("L6 (FIG-5175): register, advance, end or cascade a process on PostgreSQL")
}

pub(super) async fn request_cancel(
    _tx: &mut PgConnection,
    _request: &CancelRequest,
    _now: DurableInstant,
) -> Result<(CancelAnswer, Option<Woken>), DurableError> {
    todo!(
        "L6 (FIG-5175): record a process's first cancel request and control-wake it on PostgreSQL"
    )
}

pub(super) async fn process(
    _tx: &mut PgConnection,
    _process: &ProcessId,
) -> Result<Option<ProcessActorRow>, DurableError> {
    todo!("L6 (FIG-5175): read a process actor's row on PostgreSQL")
}

pub(super) async fn live_until_descendants(
    _tx: &mut PgConnection,
    _scope: &ScopeKey,
    _limit: usize,
) -> Result<Vec<ProcessId>, DurableError> {
    todo!("L6 (FIG-5175): list a scope's live Until descendants on PostgreSQL")
}
