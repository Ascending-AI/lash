//! FIG-3586 (T12): a parked segment written under the previous state shape is
//! refused before anything runs. The start record's executable generation is
//! the worker's fence (FIG-3571), before the engine is entered.

use super::*;
use crate::lib_tests::process_module;
use lashlang::testing::ast_builders as b;

/// Runs the `pause` sleep process through a real `LashProcessWorkflow`
/// segment on the Restate server double, returning its terminal record and
/// trace graph.
pub(crate) async fn run_sleep_process()
-> (lash_core::ProcessAwaitOutput, Arc<TraceLashlangGraphStore>) {
    let harness = crate::lib_tests::double_process_harness().await;
    let store = harness.artifact_store();
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_sleep(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process pause() -> null { finish await sleep_for(0) }",
        program: process_module(
            "pause",
            Vec::new(),
            lashlang::TypeExpr::Null,
            b::sleep_for(b::num(0.0)),
        ),
        environment: &environment,
    })
    .expect("sleep process compiles");
    store
        .publish_module_artifact(
            &lash_core::ArtifactOwner::host("sleep-fixture"),
            &output.artifact,
        )
        .await
        .expect("sleep process artifact publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("pause")
            .expect("pause export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "pause".to_string(),
        args: serde_json::Map::new(),
    };
    let registration = lash_core::ProcessRegistration::new(
        input.to_process_input().expect("valid process input"),
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ))
    .with_execution_env_ref(Some(harness.env_ref().clone()));
    let graph_store = Arc::new(TraceLashlangGraphStore::default());
    let sink: Arc<dyn lash_trace::TraceSink> = graph_store.clone();
    harness.install_lashlang_worker(
        LashlangProcessEngine::new(store, LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default()),
        Vec::new(),
    );
    let process_id = harness.admit(registration).await;
    let terminal = harness.await_terminal(&process_id).await;
    (terminal, graph_store)
}

/// T12 (FIG-3586): a parked segment written under the previous segment-state
/// shape is refused with the typed version mismatch, never decoded.
#[test]
fn a_segment_state_of_the_previous_version_is_refused() {
    let previous = serde_json::json!({
        "version": crate::LASHLANG_SEGMENT_STATE_VERSION - 1,
        "replay_ordinals": { "sleep_sequence": 3 }
    });
    let error = crate::process::decode_lashlang_segment_state_for_tests(
        &serde_json::to_vec(&previous).expect("encode"),
    )
    .expect_err("a previous-version segment is refused");
    assert!(
        error.to_string().contains(&format!(
            "version {} is incompatible with version {}",
            crate::LASHLANG_SEGMENT_STATE_VERSION - 1,
            crate::LASHLANG_SEGMENT_STATE_VERSION
        )),
        "{error}"
    );
}
