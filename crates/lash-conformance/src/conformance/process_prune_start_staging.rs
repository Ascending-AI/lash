//! FIG-4028's keyed-start fence law under the referrer model.

use pretty_assertions::assert_eq;
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each setup and transition"
)]
pub async fn prune_and_late_transfer_fences(
    registry: Arc<dyn crate::ProcessRegistry>,
    env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
) {
    let key = crate::StartKey::for_host(crate::StartKeyOwner::HOST, "prune-referrer-fences");
    let starter = crate::ExecutionScope::runtime_operation("prune-referrer-fences")
        .journal_identity()
        .expect("starter journal");
    let start = crate::ArtifactReferrer::Start(key.clone());
    let start_claim = crate::ReferrerClaim::guarded(
        start.clone(),
        crate::ArtifactCleanupPlan::AwaitStart { starter },
    )
    .expect("start claim");
    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env");
    env_store
        .publish_process_execution_env(&start_claim, &env_ref, &bytes)
        .await
        .expect("stage under start");
    let registered = registry
        .register_process(
            crate::ProcessRegistration::new(
                crate::ProcessInput::Engine {
                    kind: "test-engine".to_owned(),
                    payload: serde_json::Value::Null,
                },
                crate::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_start_key(Some(key))
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await
        .expect("register keyed process");
    let process = crate::ArtifactReferrer::ProcessRecord(registered.id.clone());
    let start_cleanup = crate::ResolvedArtifactCleanup {
        referrer: start.clone(),
        carries: vec![crate::ArtifactCarry {
            artifact: crate::ArtifactName {
                store: crate::ArtifactStoreId::ProcessEnv,
                artifact_ref: env_ref.as_str().to_owned(),
            },
            to: process.clone(),
        }],
    };
    env_store
        .end_process_env_referrer(&start_cleanup)
        .await
        .expect("settle start");
    assert_eq!(
        env_store
            .get_process_execution_env(&env_ref)
            .await
            .expect("load process env"),
        Some(bytes.clone())
    );
    let terminal = registry
        .complete_process(
            &registered.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::workflow_key(registered.id.to_string()),
        )
        .await
        .expect("complete process");
    let report = registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process");
    assert_eq!(report.pruned_processes, 1);
    env_store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: process.clone(),
            carries: Vec::new(),
        })
        .await
        .expect("apply process cleanup");
    assert_eq!(
        env_store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read reclaimed env"),
        None
    );
    for referrer in [start, process] {
        let claim = match referrer.clone() {
            crate::ArtifactReferrer::Start(_) => start_claim.clone(),
            _ => crate::ReferrerClaim::unguarded(referrer.clone()).expect("process claim"),
        };
        let refusal = env_store
            .publish_process_execution_env(&claim, &env_ref, &bytes)
            .await
            .expect_err("ended referrer is fenced");
        assert!(matches!(
            refusal,
            crate::ArtifactStoreError::ReferrerEnded { referrer: ended } if ended == referrer
        ));
    }
    env_store
        .end_process_env_referrer(&start_cleanup)
        .await
        .expect("late carry skips fenced process");
    assert_eq!(
        env_store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
}
