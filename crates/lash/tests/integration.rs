//! Integration contracts compiled together for each runtime feature lane.

// The store witnesses refer to this module through `crate::facade_support`.
#[cfg(feature = "testing")]
mod facade_support {
    pub use lash_core::facade_support::*;
}

#[path = "integration/embed_plugins.rs"]
mod embed_plugins;
#[path = "integration/facade_inventory.rs"]
mod facade_inventory;
#[path = "integration/integrator_facade.rs"]
mod integrator_facade;
#[path = "integration/one_home.rs"]
mod one_home;
#[path = "integration/stores_evidence.rs"]
mod stores_evidence;
#[path = "integration/support.rs"]
mod support;
#[path = "integration/tool_intent_ingress_observability.rs"]
mod tool_intent_ingress_observability;

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
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
