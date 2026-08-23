//! Axis 7: a live closure in a mid-cell VM continuation survives a restart.
//!
//! The ordinary RLM executor intentionally runs a foreground cell to a
//! terminal outcome. These scenarios use the test-only process-mode driver to
//! stop after the awaited tool effect, which is the state a durable process
//! worker snapshots. The continuation is encoded and decoded before the same
//! compiled cell resumes.

use super::harness::{Dialect, HarnessMode, Session};

fn parked_source(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Lashlang => {
            "fn add(left: float, right: float) -> float { left + right }\nfinish add(1, await cell.park({ \"value\": 41 })?)"
        }
        Dialect::Typescript => {
            "const add = (left: number, right: number) => left + right;\nfinish(add(1, await cell.park({ value: 41 })));"
        }
    }
}

fn parked_cell_with_live_closure_survives_snapshot_restore(dialect: Dialect) {
    let mut session = Session::open(dialect, HarnessMode::Resident);
    let base_cell = match dialect {
        Dialect::Lashlang => "base = [1, 2]",
        Dialect::Typescript => "const base = [1, 2];",
    };
    session.run_ok(base_cell);
    let before = session.user_bindings();

    let evidence = session.run_parked(parked_source(dialect));
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
    let outcome = session.run_ok(match dialect {
        Dialect::Lashlang => "finish base",
        Dialect::Typescript => "finish(base);",
    });
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2])));
}

fn broken_retention_law_is_non_vacuous(dialect: Dialect) {
    let mut session = Session::open(dialect, HarnessMode::Resident);
    let error = session.run_parked_broken(parked_source(dialect));
    eprintln!("red-proof {dialect}: {error}");
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

mod lashlang {
    use super::*;

    const DIALECT: Dialect = Dialect::Lashlang;

    #[test]
    fn parked_cell_with_live_closure_survives_snapshot_restore() {
        super::parked_cell_with_live_closure_survives_snapshot_restore(DIALECT);
    }

    #[test]
    fn broken_retention_law_is_non_vacuous() {
        super::broken_retention_law_is_non_vacuous(DIALECT);
    }
}

mod typescript {
    use super::*;

    const DIALECT: Dialect = Dialect::Typescript;

    #[test]
    fn parked_cell_with_live_closure_survives_snapshot_restore() {
        super::parked_cell_with_live_closure_survives_snapshot_restore(DIALECT);
    }

    #[test]
    fn broken_retention_law_is_non_vacuous() {
        super::broken_retention_law_is_non_vacuous(DIALECT);
    }
}
