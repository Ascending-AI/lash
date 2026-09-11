use std::sync::Arc;

use lash_core::facade_support::PluginHost;

fn tool_names(session: &lash_core::facade_support::PluginSession) -> Vec<String> {
    session
        .resolved_tool_catalog(&lash_core::SessionId::from("root"))
        .expect("tool catalog")
        .tool_names()
        .as_ref()
        .clone()
}

fn standard_session_with_access(
    session_id: &str,
    tool_access: lash_core::SessionToolAccess,
) -> Arc<lash_core::facade_support::PluginSession> {
    PluginHost::new(vec![Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    )])
    .build_session_with_parent(
        session_id,
        None,
        lash_core::plugin::SessionCreationConfig {
            authority: lash_core::plugin::SessionAuthorityContext {
                tool_access,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect("standard protocol session")
}

#[test]
fn standard_protocol_distinguishes_ambient_from_restricted_empty_access() {
    let ambient =
        standard_session_with_access("standard-ambient", lash_core::SessionToolAccess::ambient());
    assert!(tool_names(&ambient).contains(&"batch".to_string()));

    let restricted = standard_session_with_access(
        "standard-restricted-empty",
        lash_core::SessionToolAccess::restricted([]).expect("restricted empty is valid"),
    );
    assert!(tool_names(&restricted).is_empty());
}

#[test]
fn standard_protocol_owns_batch_not_processes() {
    let session = PluginHost::new(vec![Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    )])
    .build_session("root")
    .expect("session");

    let names = tool_names(&session);
    assert!(names.contains(&"batch".to_string()));
    assert!(!names.contains(&"list_process_handles".to_string()));
    assert!(!names.contains(&"cancel_process".to_string()));
}

#[test]
fn processes_are_composed_with_standard_protocol() {
    let session = PluginHost::new(vec![
        Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()),
        Arc::new(lash_tools::shell::StandardShellPluginFactory::new()),
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
    ])
    .build_session("root")
    .expect("session");

    let names = tool_names(&session);
    assert!(names.contains(&"batch".to_string()));
    assert!(names.contains(&"list_process_handles".to_string()));
    assert!(names.contains(&"cancel_process".to_string()));
}
