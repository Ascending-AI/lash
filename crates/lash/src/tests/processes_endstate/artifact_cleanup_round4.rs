// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

#[tokio::test]
async fn stale_process_cleanup_cannot_release_reregistered_incarnation_owner() -> Result<()> {
    let dir = tempfile::tempdir().expect("stale cleanup tempdir");
    let backend = Arc::new(
        lash_sqlite_store::SqliteBackend::open(dir.path())
            .await
            .expect("open the stale cleanup backend"),
    );
    let registry = backend.process_registry();
    let artifact_store = backend.process_env_store();
    let engine = Arc::new(FailOnceReleaseEngine {
        state: std::sync::Mutex::new(PruneEngineState::default()),
        release_failures: std::sync::atomic::AtomicUsize::new(1),
    });
    let process_id = ProcessId::from("reused-process-owner");
    let env_spec = process_env_spec();
    let env_ref = env_spec.stable_ref().expect("stable environment ref");
    let env_bytes = env_spec.to_store_bytes().expect("environment bytes");

    let first = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                process_id.clone(),
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "reused-process-owner"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    let first_owner = lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&first));
    artifact_store
        .publish_process_execution_env(&first_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(first_owner.clone());
    registry
        .complete_process(
            &first.id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.to_string()),
        )
        .await?;

    let core = prune_recovery_core(
        backend.clone().into(),
        artifact_store.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>,
        Arc::clone(&engine),
    )?;
    assert!(
        core.processes()
            .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
            .await
            .is_err(),
        "engine release fault must leave the first incarnation cleanup pending"
    );

    let second = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                process_id.clone(),
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "reused-process-owner"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    assert_ne!(first.incarnation, second.incarnation);
    let second_owner =
        lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&second));
    assert_ne!(
        first_owner, second_owner,
        "owner identity is incarnation-typed"
    );
    assert!(matches!(
        registry
            .get_process_ref(&lash_core::ProcessRef::from_record(&first))
            .await,
        Err(lash_core::PluginError::ProcessIncarnationSuperseded {
            requested_incarnation,
            current_incarnation,
            ..
        }) if requested_incarnation == first.incarnation
            && current_incarnation == second.incarnation
    ));
    artifact_store
        .publish_process_execution_env(&second_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(second_owner);

    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(
        report.artifact_cleanup_acknowledgements,
        vec![lash_core::ProcessArtifactCleanupAck::StaleIncarnation {
            expected: lash_core::ProcessRef::from_record(&first),
            found: lash_core::ProcessRef::from_record(&second),
        }],
        "the facade must surface that the durable cleanup belonged to a predecessor incarnation"
    );
    assert!(
        artifact_store
            .get_process_execution_env(&env_ref)
            .await?
            .is_some(),
        "acknowledging the stale cleanup must preserve the live incarnation's owner"
    );
    assert!(
        registry
            .pending_process_artifact_cleanup()
            .await?
            .is_empty()
    );
    Ok(())
}
