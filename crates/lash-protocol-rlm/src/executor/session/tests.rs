//! The laws of a session's saved bindings: one fragment per binding, each
//! written only when its binding changed, under the dialect that recorded
//! them.

use std::collections::BTreeSet;

use lash_kernel_doc::NumberPolicy;
use lash_rlm_types::RlmGlobalsPatchPluginBody;

use super::RlmExecutionState;

fn bind(name: &str, value: serde_json::Value) -> RlmGlobalsPatchPluginBody {
    RlmGlobalsPatchPluginBody {
        set_default: [(name.to_string(), value)].into_iter().collect(),
    }
}

/// A value large enough that its fragment is a leaf of its own.
fn large(tag: &str) -> serde_json::Value {
    serde_json::json!([format!("{tag}-{}", "x".repeat(2_000))])
}

async fn capture(state: &mut RlmExecutionState) -> usize {
    state
        .snapshot_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("capture the session's bindings");
    let written = state.written_leaves_in_last_snapshot();
    state.acknowledge_execution_state_capture();
    written
}

/// The session's bindings are saved per root: a capture writes the
/// fragment of each binding that changed since the last one and names the
/// rest unchanged.
#[tokio::test]
async fn a_capture_writes_only_the_bindings_that_changed() {
    let mut state = RlmExecutionState::new("typescript", NumberPolicy::Float);
    let none = BTreeSet::new();
    for (name, tag) in [("first", "a"), ("second", "b"), ("third", "c")] {
        state
            .patch_globals(&bind(name, large(tag)), &none)
            .await
            .expect("bind a session variable");
    }
    assert_eq!(capture(&mut state).await, 3, "every binding is new");
    assert_eq!(capture(&mut state).await, 0, "nothing changed");

    state
        .prune_protected_globals(&BTreeSet::from(["second".to_string()]))
        .await
        .expect("unbind one session variable");
    state
        .patch_globals(&bind("second", large("changed")), &none)
        .await
        .expect("rebind one session variable");
    assert_eq!(
        capture(&mut state).await,
        1,
        "only the rebound binding's fragment is written"
    );

    let saved = state
        .hydrated_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("the saved session state");
    let mut reloaded = RlmExecutionState::new("typescript", NumberPolicy::Float);
    reloaded
        .restore_execution_state(&saved, lash_core::FleetFormat::current())
        .await
        .expect("load the saved bindings");
    assert_eq!(
        reloaded.binding_names().collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );
    assert_eq!(
        reloaded.bound_variable_values(&none),
        state.bound_variable_values(&none)
    );
}

/// A session reads its dialect from its record: bindings recorded by a
/// session of one dialect are refused by a state of another.
#[tokio::test]
async fn bindings_recorded_in_another_dialect_are_refused() {
    let mut state = RlmExecutionState::new("typescript", NumberPolicy::Float);
    state
        .patch_globals(&bind("answer", serde_json::json!(42)), &BTreeSet::new())
        .await
        .expect("bind a session variable");
    capture(&mut state).await;
    let saved = state
        .hydrated_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("the saved session state");

    let error = RlmExecutionState::new("python", NumberPolicy::BySpelling)
        .restore_execution_state(&saved, lash_core::FleetFormat::current())
        .await
        .expect_err("another dialect's bindings are refused");
    assert!(
        matches!(
            &error,
            super::RlmSnapshotError::DialectMismatch { expected, found }
                if expected == "python" && found == "typescript"
        ),
        "{error}"
    );
}
