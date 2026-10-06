use super::*;

#[test]
fn turn_cancel_query_maps_stop_and_abort_onto_lash_modes() {
    let parse = |query: &str| {
        let uri: axum::http::Uri = format!("/api/turn/cancel?{query}")
            .parse()
            .expect("cancel route uri");
        axum::extract::Query::<TurnCancelQuery>::try_from_uri(&uri)
            .expect("cancel query parses")
            .0
    };
    let stop = parse("session_id=s-1&mode=stop");
    assert_eq!(stop.session.session_id.as_deref(), Some("s-1"));
    assert_eq!(stop.mode, WorkbenchTurnCancelMode::Stop);
    assert_eq!(stop.mode.lash_mode(), lash::TurnCancelMode::AfterStep);
    let abort = parse("session_id=s-1&mode=abort");
    assert_eq!(abort.mode, WorkbenchTurnCancelMode::Abort);
    assert_eq!(abort.mode.lash_mode(), lash::TurnCancelMode::Immediate);
    let legacy = parse("session_id=s-1");
    assert_eq!(
        legacy.mode,
        WorkbenchTurnCancelMode::Abort,
        "an unqualified Stop control keeps today's immediate abort"
    );
    assert!(ui::INDEX_HTML.contains("id=\"abort\""));
    assert!(ui::INDEX_HTML.contains("stop after step"));
    assert!(ui::INDEX_HTML.contains("\"/api/turn/cancel?mode=\" + mode"));
    assert!(ui::INDEX_HTML.contains("stopTurn(\"stop\")"));
    assert!(ui::INDEX_HTML.contains("stopTurn(\"abort\")"));
    assert!(ui::INDEX_HTML.contains("STOP_ESCALATION_MS"));
    assert!(ui::INDEX_HTML.contains("escalated"));
}
