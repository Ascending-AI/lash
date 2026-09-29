//! In-crate test suite.
//!
//! Split by what each file pins: the platform's wire contract, the bot's event
//! semantics, the log's per-line atomicity, recovery across a restart, and the
//! full-host E2E driver's picture of where the bot keeps its stores. The
//! harness in [`support`] serves the platform over a real ephemeral socket so
//! the wire assertions are made against bytes rather than against Rust
//! structs, and drives the bot through
//! [`crate::bot::channel::ChannelBot::ingest`] with envelopes the platform
//! actually produced.

mod bot_events;
mod browser_views;
mod full_host_driver;
mod log_atomicity;
mod platform_wire;
mod restart_recovery;
mod support;

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
pub(crate) async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
