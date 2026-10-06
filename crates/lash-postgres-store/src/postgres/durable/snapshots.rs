//! VM snapshots on PostgreSQL: the `snapshots` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::DurableError;
use lash_durable::domain::{ExecKey, SnapshotRow, SnapshotWrite};
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &SnapshotWrite,
) -> Result<(), DurableError> {
    todo!("V0 (FIG-5170): compare-and-set an execution's snapshot on PostgreSQL")
}

pub(super) async fn read(
    _tx: &mut PgConnection,
    _exec: &ExecKey,
) -> Result<Option<SnapshotRow>, DurableError> {
    todo!("V0 (FIG-5170): read an execution's latest snapshot on PostgreSQL")
}
