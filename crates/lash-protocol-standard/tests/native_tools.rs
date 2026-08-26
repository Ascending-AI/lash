use std::sync::Arc;

use lash_core::facade_support::PluginHost;

fn tool_names(session: &lash_core::facade_support::PluginSession) -> Vec<String> {
    session
        .resolved_tool_catalog("root")
        .expect("tool catalog")
        .tool_names()
        .as_ref()
        .clone()
}

#[test]
fn standard_protocol_owns_batch_not_processes() {
    let session = PluginHost::new(vec![Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory,
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
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory),
    ])
    .build_session("root")
    .expect("session");

    let names = tool_names(&session);
    assert!(names.contains(&"batch".to_string()));
    assert!(names.contains(&"list_process_handles".to_string()));
    assert!(names.contains(&"cancel_process".to_string()));
}
