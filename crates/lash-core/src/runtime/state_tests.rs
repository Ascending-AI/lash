//! Resident session-state tests that need lash-core's runtime helpers.
//!
//! `RuntimeSessionState` itself lives in `lash-core-store`; these cases drive
//! it through a live plugin session, so they run here.

use super::RuntimeSessionState;
use crate::SessionId;
use crate::facade_support::ToolStateFacadeOps;
use lash_sansio::sync::MutexExt;

use std::sync::{Arc, Mutex};

struct DynamicSnapshotTools {
    names: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for DynamicSnapshotTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.names
            .lock_recover()
            .iter()
            .map(|name| {
                crate::ToolDefinition::raw(
                    format!("tool:{name}"),
                    name,
                    "dynamic snapshot tool",
                    crate::ToolDefinition::default_input_schema(),
                    serde_json::json!({}),
                )
                .manifest()
            })
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.names
            .lock_recover()
            .iter()
            .any(|candidate| candidate == name)
            .then(|| {
                Arc::new(
                    crate::ToolDefinition::raw(
                        format!("tool:{name}"),
                        name,
                        "dynamic snapshot tool",
                        crate::ToolDefinition::default_input_schema(),
                        serde_json::json!({}),
                    )
                    .contract(),
                )
            })
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        crate::ToolOutcome::ok(serde_json::json!("ok"))
    }
}

#[tokio::test]
async fn corrupt_commit_result_cannot_forge_discarded_execution_state_residency() {
    use crate::store::SessionCommitStore as _;

    const LEAF_A: &str = "execution_state/leaf-a";
    const LEAF_B: &str = "execution_state/leaf-b";

    let store = crate::InMemorySessionStore::new();
    let mut generation_a =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    let mut snapshot_a = crate::plugin::ExecutionStateSnapshot::from_root(Some(
        br#"{"generation":"a","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
            .to_vec(),
    ));
    snapshot_a.changed_component(LEAF_A, b"generation-a leaf-a".to_vec());
    snapshot_a.changed_component(LEAF_B, b"generation-a leaf-b".to_vec());
    generation_a
        .set_execution_state_components(snapshot_a)
        .expect("stage valid generation-A two-leaf execution state");

    let result_a = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &generation_a,
            &[],
        ))
        .await
        .expect("persist valid generation-A state");

    let mut generation_b = generation_a.clone();
    generation_b.apply_persisted_commit_result(result_a.clone());
    let mut snapshot_b = crate::plugin::ExecutionStateSnapshot::from_root(Some(
        br#"{"generation":"b","leaves":["execution_state/leaf-a","execution_state/leaf-b"]}"#
            .to_vec(),
    ));
    snapshot_b.unchanged_component(LEAF_A);
    snapshot_b.unchanged_component(LEAF_B);
    generation_b
        .set_execution_state_components(snapshot_b)
        .expect("stage valid generation-B root over unchanged leaves");
    let result_b = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &generation_b,
            &[],
        ))
        .await
        .expect("persist valid generation-B state");

    let generation_b_root = result_b
        .manifest
        .components
        .get(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .expect("generation-B root descriptor")
        .clone();
    let generation_a_leaf_b = result_a
        .manifest
        .components
        .get(LEAF_B)
        .expect("generation-A leaf-b descriptor")
        .clone();

    let cases = [
        (
            "leaf ref absent from store",
            Box::new(|result: &mut crate::store::RuntimeCommitReceipt| {
                result
                    .manifest
                    .components
                    .get_mut(LEAF_A)
                    .expect("leaf-a descriptor")
                    .blob_ref = "execution-state-missing-leaf".to_string().into();
            }) as Box<dyn Fn(&mut crate::store::RuntimeCommitReceipt)>,
        ),
        (
            "leaf ref hashes different bytes",
            Box::new(move |result: &mut crate::store::RuntimeCommitReceipt| {
                result
                    .manifest
                    .components
                    .insert(LEAF_A.to_string(), generation_a_leaf_b.clone());
            }),
        ),
        (
            "generation-B root with generation-A leaves",
            Box::new(move |result: &mut crate::store::RuntimeCommitReceipt| {
                result.manifest.components.insert(
                    crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                    generation_b_root.clone(),
                );
            }),
        ),
        (
            "manifest omits leaf-b still listed by root",
            Box::new(|result: &mut crate::store::RuntimeCommitReceipt| {
                result.manifest.components.remove(LEAF_B);
            }),
        ),
    ];

    let mut failures = Vec::new();
    for (case, tamper) in cases {
        let mut tampered = result_a.clone();
        tamper(&mut tampered);
        let mut resident = generation_a.clone();
        resident.apply_persisted_commit_result(tampered);

        let hydration = resident.execution_state_hydration();
        if !matches!(hydration, Err(crate::StoreError::StoredDataCorrupt { .. })) {
            failures.push(format!(
                "{case}: corrupt commit-result evidence must not authorize skipped hydration; got {hydration:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}


#[test]
fn reconciled_generation_forces_next_plugin_state_export() {
    let names = Arc::new(Mutex::new(vec!["dynamic_one".to_string()]));
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(DynamicSnapshotTools {
        names: Arc::clone(&names),
    });
    let plugins =
        crate::runtime::tests::helpers::plugin_session_with_tools(&SessionId::from("root"), tools);
    let snapshot = plugins.tool_registry().export_state();
    let persisted_generation = snapshot.generation();
    let mut projected =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            .to_snapshot();
    projected.tool_state_ref = Some("persisted-tool-state".to_string().into());
    projected.tool_state_generation = Some(persisted_generation);
    let mut state = RuntimeSessionState::from_snapshot(projected);

    names.lock_recover().push("dynamic_two".to_string());
    let report = plugins
        .tool_registry()
        .restore_state(snapshot)
        .expect("live surface restore");
    assert_eq!(report.generation, persisted_generation + 1);

    state.refresh_plugin_states(&plugins);
    let refreshed = state
        .tool_state_snapshot()
        .expect("generation change re-exports the tool snapshot");
    assert_eq!(refreshed.generation(), report.generation);
    assert!(refreshed.contains(&crate::ToolId::from("tool:dynamic_two")));
}

