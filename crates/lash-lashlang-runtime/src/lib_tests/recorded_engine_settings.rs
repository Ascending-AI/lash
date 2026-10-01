use super::*;

#[tokio::test]
async fn a_hand_built_engine_runs_under_recorded_bounds_after_a_remote_round_trip() {
    use lash_core::ProcessEngine as _;

    let harness = double_process_harness().await;
    let engine = LashlangProcessEngine::new(
        harness.artifact_store(),
        LashlangSurface::default(),
        harness.backend().worker_recovery(),
    )
    .with_execution_bounds(lashlang::ExecutionBounds::new(
        lashlang::ExecutionBound::instructions(1000),
        lashlang::ExecutionBound::logical_bytes(1024 * 1024),
    ));
    let recorded = engine
        .creation_config(&lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            harness_session_policy(),
        ))
        .expect("the creation records its behaviour")
        .expect("a hand-built engine must record its behaviour");
    assert_eq!(
        recorded["instruction_budget"],
        serde_json::json!({"bounded": 1000})
    );
    assert_eq!(
        recorded["memory_limit"],
        serde_json::json!({"bounded": 1048576})
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process main() -> int { finish 42 }",
        program: process_module("main", Vec::new(), lashlang::TypeExpr::Int, b::num(42.0)),
        environment: &LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            LashlangAbilities::default(),
        ),
    })
    .expect("compile the process");
    harness
        .artifact_store()
        .publish_module_artifact(&host_claim(), &output.artifact)
        .await
        .expect("publish the process");
    let input = LashlangProcessInput {
        module_ref: output.module_ref,
        process_ref: output.artifact.process_ref("main").unwrap().clone(),
        host_requirements_ref: output.host_requirements_ref,
        process_name: "main".to_owned(),
        args: serde_json::Map::new(),
    };
    let mut registration = lash_core::ProcessRegistration::new(
        input.to_process_input().unwrap(),
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ))
    .with_execution_env_ref(Some(harness.env_ref().clone()));
    let mut local = lash_core::ProcessRecord::from_registration(
        registration.clone(),
        lash_core::mint_process_id(),
    );
    local.engine_config = Some(recorded.clone());
    let remote = lash_remote_protocol::RemoteProcessRecord::try_from(local).unwrap();
    let remote = serde_json::from_slice::<lash_remote_protocol::RemoteProcessRecord>(
        &serde_json::to_vec(&remote).unwrap(),
    )
    .unwrap();
    let received = lash_core::ProcessRecord::try_from(remote).unwrap();
    assert_eq!(received.engine_config.as_ref(), Some(&recorded));
    registration.engine_config = received.engine_config;
    let record = harness
        .registry()
        .register_process(registration)
        .await
        .unwrap();
    let redeployed = LashlangProcessEngine::new(
        harness.artifact_store(),
        LashlangSurface::default(),
        harness.backend().worker_recovery(),
    )
    .with_execution_bounds(lashlang::ExecutionBounds::new(
        lashlang::ExecutionBound::instructions(1),
        lashlang::ExecutionBound::logical_bytes(1),
    ));
    harness.install_lashlang_worker(redeployed, Vec::new());
    harness.deliver_start(&record.id).await;
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        harness.await_terminal(&record.id),
    )
    .await
    .expect("the recorded bounds let the process finish");
    assert!(
        matches!(terminal, lash_core::ProcessAwaitOutput::Settled { ref output }
        if output.is_success()),
        "{terminal:?}"
    );
    assert!(serde_json::to_string(&terminal).unwrap().contains("42"));
}

#[tokio::test]
async fn a_hand_built_engine_refuses_a_run_without_recorded_behaviour() {
    let harness = double_process_harness().await;
    let engine = LashlangProcessEngine::new(
        harness.artifact_store(),
        LashlangSurface::default(),
        harness.backend().worker_recovery(),
    );
    let registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::Engine {
            kind: LASHLANG_ENGINE_KIND.to_owned(),
            payload: serde_json::json!({}),
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    );
    let context = lash_core::testing::process_engine_run_context_for_validation(
        harness.backend(),
        registration,
        Arc::new(lash_core::ToolCatalog::default()),
        false,
    );
    let error = engine
        .run_settings(&context)
        .err()
        .expect("a run cannot use constructor behaviour")
        .into_plugin_error();
    assert!(error.is_terminal());
    assert!(!error.is_retryable());
    let controller = lash_core::RuntimeEffectControllerError::from(error.clone());
    assert_eq!(
        controller.code,
        lash_core::RuntimeErrorCode::MissingRecordedProcessConfig
    );
    assert!(controller.is_terminal());
    assert!(!controller.code.is_retryable());
    let turn = error
        .clone()
        .into_turn_failure(lash_core::RuntimeErrorCode::Plugin);
    assert_eq!(
        turn.code,
        lash_core::RuntimeErrorCode::MissingRecordedProcessConfig
    );
    let wire = serde_json::to_vec(&error).unwrap();
    let error: lash_core::PluginError = serde_json::from_slice(&wire).unwrap();
    assert!(
        matches!(error, lash_core::PluginError::MissingRecordedProcessConfig { engine_kind }
        if engine_kind == LASHLANG_ENGINE_KIND)
    );
}
