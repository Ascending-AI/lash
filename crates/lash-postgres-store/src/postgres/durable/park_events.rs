//! The operator's park feed on PostgreSQL: the `park_events` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use lash_durable::DurableError;
use lash_durable::domain::{ParkEventRow, ParkEventSeq, ParkEventWrite};
use sqlx::PgConnection;

use super::Committing;

pub(super) async fn apply(
    _tx: &mut PgConnection,
    _commit: &Committing<'_>,
    _write: &ParkEventWrite,
) -> Result<(), DurableError> {
    todo!("L6 (FIG-5175): append to the park feed on PostgreSQL")
}

pub(super) async fn read(
    _tx: &mut PgConnection,
    _after: Option<ParkEventSeq>,
    _limit: usize,
) -> Result<Vec<ParkEventRow>, DurableError> {
    todo!("L6 (FIG-5175): read the park feed on PostgreSQL")
}
