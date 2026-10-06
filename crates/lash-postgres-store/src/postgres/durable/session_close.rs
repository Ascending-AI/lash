//! The session's closing state on PostgreSQL: the `session_close` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6b (FIG-5176). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::DurableError;
use lash_durable::domain::SessionCloseWrite;
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &SessionCloseWrite,
) -> Result<(), DurableError> {
    todo!("L6b (FIG-5176): record a session close step on PostgreSQL")
}
