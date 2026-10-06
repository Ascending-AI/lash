//! Turn phase state, the session commit and turn-cancel mail on PostgreSQL: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::domain::{
    SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnRow, TurnWrite,
};
use lash_durable::{DurableError, DurableInstant, Woken};
use lash_sansio::SessionId;
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &TurnWrite,
) -> Result<(), DurableError> {
    todo!("V0 (FIG-5170): write the turn phase row on PostgreSQL")
}

pub(super) async fn apply_session_commit(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &SessionCommitWrite,
) -> Result<(), DurableError> {
    todo!("V0 (FIG-5170): commit the turn to its session head on PostgreSQL")
}

pub(super) async fn request_cancel(
    _tx: &mut PgConnection,
    _request: &TurnCancelRequest,
    _now: DurableInstant,
) -> Result<(TurnCancelAnswer, Option<Woken>), DurableError> {
    todo!("L3 (FIG-5172): record a turn cancel request and wake the session on PostgreSQL")
}

pub(super) async fn turn(
    _tx: &mut PgConnection,
    _session: &SessionId,
) -> Result<Option<TurnRow>, DurableError> {
    todo!("V0 (FIG-5170): read the session's unfinished turn on PostgreSQL")
}
