//! Integration contracts compiled together for each runtime feature lane.

#[path = "integration/embed_plugins.rs"]
mod embed_plugins;
#[path = "integration/facade_host_wrappers.rs"]
mod facade_host_wrappers;
#[path = "integration/facade_inventory.rs"]
mod facade_inventory;
#[cfg(feature = "mcp")]
#[path = "integration/mcp_catalog.rs"]
mod mcp_catalog;
#[path = "integration/one_home.rs"]
mod one_home;
#[path = "integration/owned_call_purpose.rs"]
mod owned_call_purpose;
#[path = "integration/prompt_section_leak.rs"]
mod prompt_section_leak;
#[path = "integration/prompt_sections.rs"]
mod prompt_sections;
#[path = "integration/stores_evidence.rs"]
mod stores_evidence;
#[path = "integration/support.rs"]
mod support;
#[path = "integration/tool_intent_ingress_observability.rs"]
mod tool_intent_ingress_observability;

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` to run
/// `model`, unbounded, unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    model: &str,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
