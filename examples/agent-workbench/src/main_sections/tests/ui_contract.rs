use super::*;

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
