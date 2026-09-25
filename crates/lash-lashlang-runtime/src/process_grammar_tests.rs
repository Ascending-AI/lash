//! FIG-3586 (T12): a parked segment written under the previous state shape is
//! refused before anything runs. The start record's executable generation is
//! the worker's fence (FIG-3571), before the engine is entered.

use super::*;
use crate::lib_tests::{durable_process_events, process_module};
use lashlang::testing::ast_builders as b;

/// Runs the `pause` sleep process through a real process context, returning
/// its outcome and trace graph.
pub(crate) async fn run_sleep_process()
-> (lash_core::ProcessRunOutcome, Arc<TraceLashlangGraphStore>) {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_sleep(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process pause() -> null { finish await sleep_until(0) }",
        program: process_module(
            "pause",
            Vec::new(),
            lashlang::TypeExpr::Null,
            b::sleep_until(b::num(0.0)),
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
    let process_id = lash_core::ProcessId::from("sleep-process");
    let registration = lash_core::ProcessRegistration::new(
        process_id.clone(),
        input.to_process_input().expect("valid process input"),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ));
    let incarnation = lash_core::ProcessIncarnation::from_registration_sequence(1);
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    let effect_host = lash_core::Backend::from(backend.clone()).effect_host();
    let scoped = lash_core::EffectHost::scoped_static(
        effect_host.as_ref(),
        lash_core::AdmittedScope::process(lash_core::ProcessRef::new(
            process_id.clone(),
            incarnation,
        )),
    )
    .expect("valid process scope")
    .expect("the backend host lends a static controller");
    let parent = lash_core::RuntimeInvocation::effect(
        lash_core::EffectAddress::new(
            lash_core::ExecutionScope::process(process_id.clone()),
            "process-body",
        )
        .expect("valid process effect address"),
        lash_core::RuntimeAttribution::none(),
        "process-body",
    );
    let built = lash_core::testing::TestExecutionContextBuilder::over_controller(scoped.clone())
        .runtime_parent_invocation(parent)
        .build();
    let plugins = Arc::clone(&built.dispatch.plugins);
    let catalog = Arc::clone(&built.dispatch.tool_catalog);
    let expected_catalog = Arc::clone(&catalog);
    let registry = lash_core::Backend::from(backend.clone()).process_registry();
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "sleep-run")
        .bind_attempt(1);
    let process_events = durable_process_events(&registry, &registration, &authority).await;
    let execution_registration = registration.clone();
    let context = lash_core::ProcessEngineRunContext::new(
        registration,
        incarnation,
        lash_core::ProcessExecutionContext::default().with_execution_write_authority(authority),
        lash_core::testing::process_work_wiring_for_registry(registry),
        lash_core::SessionId::from("sleep-session"),
        plugins,
        catalog,
        None,
        None,
        Arc::new(lash_core::NoQueuedWork::new()),
        lash_core::DeliveryPolicy::EarliestSafeBoundary,
        Arc::new(lash_core::facade_support::SystemClock),
        true,
        lash_core::CancellationToken::new(),
        None,
        scoped,
        None,
        Box::new(move |catalog| {
            assert!(Arc::ptr_eq(&catalog, &expected_catalog));
            Ok(
                lash_core_execution::runtime::ProcessEngineRuntimeContext::new(
                    built
                        .into_runtime()
                        .with_process_execution(&execution_registration, process_events),
                    lash_core_execution::runtime::ProcessEngineRunGuard::new(|_| {
                        Box::pin(async { Ok(()) })
                    }),
                ),
            )
        }),
    );
    let graph_store = Arc::new(TraceLashlangGraphStore::default());
    let sink: Arc<dyn lash_trace::TraceSink> = graph_store.clone();
    let result = Box::pin(crate::process::run_lashlang_process(
        LashlangProcessEngine::new(store, LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default()),
        context,
        serde_json::to_value(input).expect("process input serializes"),
    ))
    .await
    .expect("process run succeeds");
    (result, graph_store)
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
