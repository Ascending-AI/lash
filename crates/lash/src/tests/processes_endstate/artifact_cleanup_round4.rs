// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::tests::restate_double;

const SEED: u64 = 0x5eed_f303;

#[tokio::test]
async fn a_pruned_process_cleanup_cannot_release_its_successor_owner() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let registry = backend.process_registry();
    let artifact_store = backend.process_env_store();
    let engine = Arc::new(FailOnceReleaseEngine {
        state: std::sync::Mutex::new(PruneEngineState::default()),
        release_failures: std::sync::atomic::AtomicUsize::new(1),
    });
    let env_spec = process_env_spec();
    let env_ref = env_spec.stable_ref().expect("stable environment ref");
    let env_bytes = env_spec.to_store_bytes().expect("environment bytes");

    let first = registry
        .register_process(
            lash_core::ProcessRegistration::new(
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
    let first_owner = lash_core::ArtifactOwner::process(first.id.clone());
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
            lash_core::ProcessCompletionAuthority::workflow_key(first.id.to_string()),
        )
        .await?;

    let core = prune_recovery_core(
        backend.clone(),
        Arc::clone(&artifact_store),
        Arc::clone(&engine),
    )?;
    assert!(
        core.processes()
            .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
            .await
            .is_err(),
        "engine release fault must leave the first process's cleanup pending"
    );

    let second = registry
        .register_process(
            lash_core::ProcessRegistration::new(
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
    assert_ne!(first.id, second.id, "the successor is minted a new id");
    let second_owner = lash_core::ArtifactOwner::process(second.id.clone());
    assert_ne!(
        first_owner, second_owner,
        "an owner is named by its process's minted id"
    );
    assert!(matches!(
        registry.get_process(&first.id).await,
        Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
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
        vec![lash_core::ProcessArtifactCleanupAck::Acknowledged {
            process_id: first.id.clone(),
        }],
        "the facade acknowledges the pruned process's own cleanup"
    );
    assert!(
        artifact_store
            .get_process_execution_env(&env_ref)
            .await?
            .is_some(),
        "acknowledging the pruned process's cleanup must preserve the successor's owner"
    );
    assert!(
        registry
            .pending_process_artifact_cleanup()
            .await?
            .is_empty()
    );
    Ok(())
}
