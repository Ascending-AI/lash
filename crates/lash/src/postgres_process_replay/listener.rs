//! One LISTEN connection per replica.
//!
//! The listener confirms its LISTEN, then reports itself listening; a
//! subscribe waits for that, so every doorbell after a subscriber registers
//! reaches it. When the connection drops it reconnects with backoff and
//! rings every local subscription, which then re-reads from its cursor:
//! notifications sent while it was away are lost, the rows are not.

use std::sync::Arc;

use super::Shared;
use super::codec::Doorbell;
use super::schema::db_error;

pub(super) async fn run(shared: Arc<Shared>) {
    shared
        .listener
        .run(
            || async { Ok::<_, lash_core::ProcessReplayStoreError>(()) },
            |payload| shared.ring(&payload.map_or_else(Doorbell::all, Doorbell::unpack)),
            db_error("listen"),
        )
        .await;
}
