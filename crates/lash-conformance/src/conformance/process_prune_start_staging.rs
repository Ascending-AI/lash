//! Process Prune carries a keyed start's staging owner past the pruned row.

use pretty_assertions::assert_eq;
use std::sync::Arc;

/// FIG-4028, ADR 0107: a start stages its artifacts under the staging owner
/// its start key names, and a start that registered its process and stopped
/// before settling leaves them there. Pruning that process records the key in
/// the artifact cleanup the prune leaves behind — exactly the record
/// [`ProcessArtifactCleanup::from_record`](crate::ProcessArtifactCleanup::from_record)
/// derives from the pruned row — so the cleanup drain retires the staging
/// owner: what it held is reclaimed, and a late publication under it is
/// refused.
///
/// Red on PostgreSQL before the fix, whose prune assembled the cleanup record
/// in SQL without the start key, so the drain never learned the owner.
///
/// Integrator class (ADR 0051): **conformance-suite embedders** run this law
/// against custom process registries and process-environment stores.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_prune_retires_the_start_staging_owner(
    registry: Arc<dyn crate::ProcessRegistry>,
    env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
) {
    let key = crate::StartKey::for_host(crate::StartKeyOwner::HOST, "prune-retires-start-staging");
    let staging_owner =
        crate::ArtifactOwner::process_start(&crate::ProcessCommand::start_effect_id(Some(&key)));
    let env_spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = env_spec.stable_ref().expect("stable env ref");
    let env_bytes = env_spec.to_store_bytes().expect("encode env spec");

    // The start staged its environment and registered its process, then
    // stopped before settling the environment onto the process.
    env_store
        .publish_process_execution_env(&staging_owner, &env_ref, &env_bytes)
        .await
        .expect("stage the start's environment");
    let registered = registry
        .register_process(
            crate::ProcessRegistration::new(
                crate::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                crate::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_start_key(Some(key.clone()))
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await
        .expect("register the keyed process");

    let terminal = registry
        .complete_process(
            &registered.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::workflow_key(registered.id.to_string()),
        )
        .await
        .expect("complete the keyed process");
    let report = registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the keyed process");
    assert_eq!(report.pruned_processes, 1, "the keyed process is prunable");

    let pending = registry
        .pending_process_artifact_cleanup()
        .await
        .expect("read the prune's cleanup record");
    assert_eq!(
        pending,
        vec![crate::ProcessArtifactCleanup::from_record(&terminal)],
        "the prune records the cleanup its pruned row derives"
    );
    let cleanup = &pending[0];
    assert_eq!(
        cleanup.start_key.as_ref(),
        Some(&key),
        "the cleanup record carries the start key"
    );

    // The drain half the facade runs for the environment store.
    let retired = cleanup
        .start_staging_owner()
        .expect("a keyed process's cleanup names its start's staging owner");
    assert_eq!(retired, staging_owner);
    env_store
        .retire_process_execution_env_owner(&retired)
        .await
        .expect("retire the start's staging owner");
    assert_eq!(
        registry
            .complete_process_artifact_cleanup(&cleanup.process_id)
            .await
            .expect("acknowledge the cleanup"),
        crate::ProcessArtifactCleanupAck::Acknowledged {
            process_id: registered.id.clone(),
        }
    );

    assert!(
        env_store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read the staged environment after the drain")
            .is_none(),
        "retiring the staging owner reclaims what only it held"
    );
    let refusal = env_store
        .publish_process_execution_env(&staging_owner, &env_ref, &env_bytes)
        .await
        .expect_err("the pruned start's staging owner is fenced");
    assert!(
        lash_core::runtime::artifact_owner_is_permanently_retired(&refusal),
        "the fence refusal carries the typed retirement reason, got {refusal}"
    );
}
