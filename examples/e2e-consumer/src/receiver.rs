//! The case's intent receiver: a host engine process whose events the
//! bodies' declared `EmitProcessEvent` intents append. Its id lives in a
//! case-owned file, so a node that resumes another node's work emits to the
//! same process.
use std::path::Path;

use anyhow::{Context as _, Result};

#[path = "../../shared/h2_receiver.rs"]
#[expect(
    dead_code,
    reason = "the source and sleeper engines' starters serve the workbench host only"
)]
mod engine;
pub use engine::{ReceiverEnginePlugin, ReceiverEvents};

/// The event type the bodies emit.
pub const EVENT: &str = "e2e_mutation";

/// The receiver the case started, as its file names it.
pub fn bound(path: &Path) -> Result<lash::ProcessId> {
    let bytes = std::fs::read(path).context("no receiver was started for the case")?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Start the receiver for `session` on `core` and record its id at `path`.
pub async fn start(
    core: &lash::LashCore,
    session: &lash::SessionId,
    path: &Path,
) -> Result<lash::ProcessId> {
    let receipt = engine::register_receiver(core, session, EVENT, core.effect_host()).await?;
    std::fs::write(path, serde_json::to_vec(&receipt.process_id)?)?;
    Ok(receipt.process_id)
}

/// The receiver's events.
pub async fn events(core: &lash::LashCore, process: &lash::ProcessId) -> Result<ReceiverEvents> {
    engine::receiver_events(core, process).await
}
