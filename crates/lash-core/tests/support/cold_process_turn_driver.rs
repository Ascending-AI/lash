//! Shared full-turn driver for SQLite/PostgreSQL cold-process crash helpers.

use std::sync::Arc;

pub async fn run_real_turn_action(
    store: Arc<dyn lash_core::RuntimePersistence>,
    controller: Arc<dyn lash_core::RuntimeEffectController>,
    action: &str,
    nonce: &str,
    marker: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    lash_core::testing::conformance::cold_process_real_turn_driver(
        store, controller, nonce, action, marker,
    )
    .await;
    Ok(())
}
