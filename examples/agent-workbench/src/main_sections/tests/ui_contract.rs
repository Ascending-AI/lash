#[test]
fn healthz_reports_workbench_fingerprint() {
    // scripts/agent-workbench-dev.sh readiness-checks this exact shape to
    // distinguish the workbench from a random process on the port.
    let Json(body) = sync_await(healthz());
    assert_eq!(body["service"], "agent-workbench");
    assert_eq!(body["status"], "ok");
}

#[test]
fn workbench_renders_scoped_tabs_and_trigger_lifecycle_controls() {
    assert!(ui::INDEX_HTML.contains("id=\"newSessionTab\""));
    assert!(ui::INDEX_HTML.contains("id=\"triggerRegistrations\""));
    assert!(ui::INDEX_HTML.contains("re-enable"));
    assert!(ui::INDEX_HTML.contains("deleteTrigger"));
    assert!(ui::INDEX_HTML.contains("scopedSessionId"));
}

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
    // The typed error a failed /api/state carries still reaches the operator; it
    // is the detail line of the unavailable banner rather than a transcript row,
    // because a failure to load the session is not a fact about the session.
    assert!(ui::INDEX_HTML.contains("typeof body?.error === \"string\""));
    assert!(ui::INDEX_HTML.contains("function snapshotFailureReason(error)"));
    assert!(
        ui::INDEX_HTML.contains(
            "markShellChannel(shellAvailability, \"state\", false, snapshotFailureReason(error))"
        )
    );
    assert!(ui::INDEX_HTML.contains("shellStatusDetail.textContent = model.banner.detail"));
}

#[test]
fn workbench_ui_never_ships_an_unhydrated_session_claim() {
    // FIG-791: a reload during a backend outage rendered the pristine shell —
    // `session —`, an `idle` pill, "no turns yet" — which is an affirmative
    // claim about a session the shell had heard nothing about. The static
    // markup must claim only the state of its own connection.
    let markup = ui::INDEX_HTML
        .split("</head>")
        .nth(1)
        .expect("the workbench page has a body")
        .split("<script")
        .next()
        .expect("the body carries markup before its script");
    assert!(markup.contains("id=\"busyText\" aria-live=\"polite\">connecting<"));
    assert!(markup.contains("id=\"sessionId\">connecting…<"));
    assert!(!markup.contains("aria-live=\"polite\">idle<"));
    assert!(!markup.contains("no turns yet"));
    assert!(markup.contains("connecting to the workbench…"));
    assert!(markup.contains("id=\"shellStatus\""));
    // The empty-session claim exists only as the live phase's placeholder.
    assert!(ui::INDEX_HTML.contains("function shellPhase(availability)"));
    assert!(ui::INDEX_HTML.contains("function timelinePlaceholder(phase)"));
    assert_eq!(
        ui::INDEX_HTML
            .matches("no turns yet. ask the agent")
            .count(),
        1,
        "the empty-session claim must have exactly one source: the live placeholder"
    );
}
