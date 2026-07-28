#[test]
fn workbench_ui_renders_accounts_panel() {
    assert!(ui::INDEX_HTML.contains("id=\"accountsView\""));
    assert!(ui::INDEX_HTML.contains("data-view=\"accounts\""));
    assert!(ui::INDEX_HTML.contains("id=\"accountAddForm\""));
    assert!(ui::INDEX_HTML.contains("async function loadAccounts"));
    assert!(ui::INDEX_HTML.contains("async function deleteAccount"));
}

#[test]
fn workbench_ui_distinguishes_running_turn_ingress_actions() {
    assert!(ui::INDEX_HTML.contains("id=\"injectNow\""));
    assert!(ui::INDEX_HTML.contains("id=\"queueNext\""));
    assert!(ui::INDEX_HTML.contains("injected now"));
    assert!(ui::INDEX_HTML.contains("queued next"));
    assert!(ui::INDEX_HTML.contains("/api/turn/input"));
    assert!(ui::INDEX_HTML.contains("event.type === \"turn_input\""));
    assert!(ui::INDEX_HTML.contains("event.type === \"message\""));
    assert!(ui::INDEX_HTML.contains("renderMessage(event.message)"));
}

#[test]
fn workbench_ui_renders_typed_session_open_errors() {
    assert!(ui::INDEX_HTML.contains("typeof body?.error === \"string\""));
    assert!(
        ui::INDEX_HTML
            .contains("renderError(error?.message || \"the workbench session could not be loaded")
    );
}
