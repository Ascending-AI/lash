//! FIG-3531 cancelled-turn withheld-input laws on PostgreSQL.
//!
//! A cancelled turn settles input it withheld from its terminal checkpoint
//! through the cancellation's undelivered disposition, so PostgreSQL owes the
//! same law as every other backend.

use std::sync::Arc;

use lash_core::store::RuntimePersistence;

use super::{reset, storage};

lash_conformance::cancelled_turn_withheld_input_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cancelled-turn withheld-input conformance: database is not configured"
        );
        return;
    };
    reset(storage.pool()).await;
    (
        database_lock,
        "postgres",
        Arc::new(storage.session_store(lash_conformance::CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID))
            as Arc<dyn RuntimePersistence>,
    )
});
