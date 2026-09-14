//! Axis 7: a live closure in a mid-cell VM continuation survives a restart.
//!
//! The ordinary RLM executor intentionally runs a foreground cell to a
//! terminal outcome. These scenarios use the test-only process-mode driver to
//! stop after the awaited tool effect, which is the state a durable process
//! worker snapshots. The continuation is encoded and decoded before the same
//! compiled cell resumes.

use super::harness::{HarnessMode, Session};

const PARKED_SOURCE: &str = "const add = (left: number, right: number) => left + right;\nfinish(add(1, await cell.park({ value: 41 })));";

#[test]
fn parked_cell_with_live_closure_survives_snapshot_restore() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok("const base = [1, 2];");
    let before = session.user_bindings();

    let evidence = session.run_parked(PARKED_SOURCE);
    assert_eq!(evidence.finish, serde_json::json!(42));
    assert!(
        evidence.closure_root,
        "the parked continuation retained a closure root"
    );
    assert!(evidence.continuation_bytes > 0);

    assert_eq!(
        session.user_bindings(),
        before,
        "a closure created inside the parked cell must not become a session binding"
    );
    assert!(
        !session
            .persisted_state()
            .root
            .windows(3)
            .any(|window| window == b"add"),
        "the completed cell must not persist its closure name"
    );
    let outcome = session.run_ok("finish(base);");
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2])));
}

#[test]
fn broken_retention_law_is_non_vacuous() {
    let mut session = Session::open(HarnessMode::Resident);
    let error = session.run_parked_broken(PARKED_SOURCE);
    eprintln!("red-proof: {error}");
    assert!(
        error.contains("closure")
            || error.contains("function")
            || error.contains("null")
            || error.contains("call")
            || error.contains("heap")
            || error.contains("serializable"),
        "broken continuation should fail because the retained closure was removed: {error}"
    );
}
