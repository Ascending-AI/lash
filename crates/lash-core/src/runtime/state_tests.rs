//! Resident session-state tests that need lash-core's runtime helpers.
//!
//! `RuntimeSessionState` itself lives in `lash-core-store`; these cases shift
//! it through a live plugin session, so they run here.

use super::RuntimeSessionState;

#[tokio::test]
async fn corrupt_commit_result_cannot_forge_discarded_execution_state_residency() {
    use crate::store::SessionCommitStore as _;

    let store = crate::testing::unbound_recording_store().await;
    let mut generation_a = RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    crate::store::SessionCatalogStore::admit_session(
        &store,
        &crate::testing::store_fixtures::root_session_request(&generation_a.session_id),
    )
    .await
    .expect("admit the session");
    generation_a.set_execution_graph(crate::testing::store_fixtures::execution_graph(&format!(
        "generation-a {}",
        "a".repeat(1024)
    )));
    let result_a = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &generation_a,
        ))
        .await
        .expect("persist valid generation-A state");

    let mut generation_b = generation_a.clone();
    generation_b
        .apply_persisted_commit_result(result_a.clone())
        .unwrap();
    generation_b.set_execution_graph(crate::testing::store_fixtures::execution_graph(&format!(
        "generation-b {}",
        "b".repeat(1024)
    )));
    let result_b = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &generation_b,
        ))
        .await
        .expect("persist valid generation-B state");
    let generation_b_root = result_b
        .manifest
        .execution_ref
        .clone()
        .expect("generation-B root");

    let cases = [
        (
            "root absent from store",
            Box::new(|result: &mut crate::store::RuntimeCommitReceipt| {
                result.manifest.execution_ref = Some(crate::store::BlobRef(
                    crate::durable_port::domain::checkpoint_graph::NodeId::of_envelope(b"absent")
                        .to_hex(),
                ));
            }) as Box<dyn Fn(&mut crate::store::RuntimeCommitReceipt)>,
        ),
        (
            "another generation's committed root",
            Box::new(move |result: &mut crate::store::RuntimeCommitReceipt| {
                result.manifest.execution_ref = Some(generation_b_root.clone());
            }),
        ),
        (
            "manifest omits the staged root",
            Box::new(|result: &mut crate::store::RuntimeCommitReceipt| {
                result.manifest.execution_ref = None;
            }),
        ),
    ];

    let mut failures = Vec::new();
    for (case, tamper) in cases {
        let mut tampered = result_a.clone();
        tamper(&mut tampered);
        let mut resident = generation_a.clone();
        resident.apply_persisted_commit_result(tampered).unwrap();

        let hydration = resident.execution_state_hydration();
        if !matches!(hydration, Err(crate::StoreError::StoredDataCorrupt { .. })) {
            failures.push(format!(
                "{case}: corrupt commit-result evidence must not authorize skipped hydration; got {hydration:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
