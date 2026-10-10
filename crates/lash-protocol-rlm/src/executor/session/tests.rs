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

/// A saved function whose code is `return 1`, written for kernel version
/// `kernel`.
fn saved(name: &str, kernel: u32) -> serde_json::Value {
    let mut document = lash_kernel_doc::Document::new(NumberPolicy::Float, Vec::new());
    document.manifest.kernel = kernel;
    document.functions.insert(
        lash_kernel_doc::Name::new(name),
        lash_kernel_doc::Function {
            params: Vec::new(),
            body: Vec::new(),
        },
    );
    serde_json::to_value(lash_kernel_dialect::SavedFunction {
        name: lash_kernel_doc::Name::new(name),
        document,
        captures: Default::default(),
        written: None,
    })
    .expect("the saved function encodes")
}

/// Kernel spec §6: a saved function is carried to this build's kernel
/// version when its session is seeded or restored, and one the migration
/// refuses is not held. The session lists its name with the refusal, as it
/// lists a binding that was not saved, and keeps the functions it carries.
#[tokio::test]
async fn a_saved_function_the_migration_refuses_is_listed_and_not_held() {
    let none = BTreeSet::new();
    let unknown = lash_kernel_doc::KERNEL_VERSION + 98;
    let listed = |state: &RlmExecutionState| {
        let why = state
            .bindings
            .not_carried()
            .get(&lash_kernel_doc::Name::new("old"))
            .cloned();
        assert!(
            matches!(
                &why,
                Some(lash_kernel_dialect::NotSaved::NotMigrated { from, problem })
                    if *from == unknown && problem.contains("kernel version")
            ),
            "{why:?}"
        );
        let held: Vec<String> = state
            .bindings
            .functions()
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(held, ["kept"]);
    };

    // Seeded: the session is created with both.
    let mut seeded = RlmExecutionState::new("typescript", NumberPolicy::Float);
    let functions = [
        (
            "kept".to_string(),
            saved("kept", lash_kernel_doc::KERNEL_VERSION),
        ),
        ("old".to_string(), saved("old", unknown)),
    ]
    .into_iter()
    .collect();
    seeded
        .seed_functions(&functions, &none)
        .await
        .expect("a refused function does not refuse the seed");
    listed(&seeded);

    // Restored: a stored state holds both.
    let mut stored = RlmExecutionState::new("typescript", NumberPolicy::Float);
    let both = [
        (
            "kept".to_string(),
            saved("kept", lash_kernel_doc::KERNEL_VERSION),
        ),
        (
            "old".to_string(),
            saved("old", lash_kernel_doc::KERNEL_VERSION),
        ),
    ]
    .into_iter()
    .collect();
    stored
        .seed_functions(&both, &none)
        .await
        .expect("seed the stored session");
    capture(&mut stored).await;
    let mut saved_state = stored
        .hydrated_execution_state(lash_core::FleetFormat::current())
        .await
        .expect("the saved session state");
    let mut root: serde_json::Value =
        serde_json::from_slice(&saved_state.root).expect("the stored root");
    root["functions"]["old"]["function"]["document"]["manifest"]["kernel"] = unknown.into();
    saved_state.root = serde_json::to_vec(&root)
        .expect("the stored root encodes")
        .into();
    let mut restored = RlmExecutionState::new("typescript", NumberPolicy::Float);
    restored
        .restore_execution_state(&saved_state, lash_core::FleetFormat::current())
        .await
        .expect("a refused function does not refuse the restore");
    listed(&restored);
    assert!(
        restored.execution_state_dirty(),
        "what the restore carried and dropped is stored by the next capture"
    );
}
