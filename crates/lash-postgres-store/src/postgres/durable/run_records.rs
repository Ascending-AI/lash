//! Run records on PostgreSQL: the `run_records` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::DurableError;
use lash_durable::domain::{OwnerKey, RunRecordRow, RunRecordWrite};
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &RunRecordWrite,
) -> Result<(), DurableError> {
    todo!("V0 (FIG-5170): append or prune run records on PostgreSQL")
}

pub(super) async fn read(
    _tx: &mut PgConnection,
    _owner: &OwnerKey,
) -> Result<Vec<RunRecordRow>, DurableError> {
    todo!("V0 (FIG-5170): read an owner's run records on PostgreSQL")
}
