use lash_execution_contracts::{
    ProcessEnvStore, ProcessLifecycle, ProcessRegistrar, ProcessRetention,
};

use lash_execution_contracts::backend::StoreSet as _;
use lash_execution_contracts::process::Lifetime;
use lash_execution_contracts::process::ProcessAwaitOutput;
use lash_execution_contracts::process::ProcessCompletionAuthority;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessProvenance;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::process::ProjectionWatermark;

use crate::support::sqlite_memory_process_store_set;

#[tokio::test]
async fn prune_retains_exact_artifact_cleanup_until_acknowledged() {
    let backend = sqlite_memory_process_store_set().await;
    let registry = backend.process_registry();
    let cleanup_ledger = backend.artifact_cleanup();
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"module_ref": "module-1"}),
            launch: lash_execution_contracts::process::EngineLaunch::new(
                lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                None,
            ),
        },
        ProcessProvenance::host(),
        Lifetime::Detached,
    );
    let registered = registry
        .register_process(registration)
        .await
        .expect("register process with exact artifact inputs");
    registry
        .complete_process(
            &registered.id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key("artifact-cleanup-process"),
        )
        .await
        .expect("complete process before prune");

    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune process row and persist cleanup evidence atomically");
    let pending = cleanup_ledger
        .claim_due(
            i64::MAX as u64 / 2,
            1000,
            std::num::NonZeroUsize::new(1).expect("nonzero"),
        )
        .await
        .expect("claim pending artifact cleanup");
    assert_eq!(pending.len(), 1);
    let cleanup = cleanup_ledger
        .load_cleanup(&pending[0].id)
        .await
        .expect("read cleanup")
        .expect("cleanup exists");
    assert_eq!(
        cleanup,
        crate::ArtifactCleanup::ended(
            crate::ArtifactReferrer::ProcessRecord(registered.id.clone()),
            Vec::new(),
            None,
        ),
        "row deletion must leave the process referrer cleanup"
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector)
            .await
            .expect("compact while cleanup is pending"),
        1,
        "the self-contained cleanup obligation survives tombstone compaction"
    );

    let acknowledgement = cleanup_ledger
        .settle(
            &pending[0].id,
            &pending[0].token,
            crate::store::ObligationSettlement::Delivered,
            i64::MAX as u64 / 2,
        )
        .await
        .expect("acknowledge artifact cleanup");
    assert_eq!(acknowledgement, crate::store::SettleOutcome::Applied);
    assert!(
        cleanup_ledger
            .load_cleanup(&pending[0].id)
            .await
            .expect("read cleanup after acknowledgement")
            .is_none()
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector)
            .await
            .expect("compact after cleanup acknowledgement"),
        0
    );
}

/// The store's refusals carry the typed reasons, so every caller downstream of
/// `ProcessEnvStore` classifies by code.
#[tokio::test]
async fn env_store_reports_typed_referrer_fences_and_carry_refusals() {
    let backend = crate::support::sqlite_memory_store_set().await;
    let store = backend.process_env_store();
    let spec = lash_execution_contracts::testing::process_env_with_limit(1024);
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let staged = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    let staged_claim = crate::ReferrerClaim::unguarded(staged.clone()).expect("staged claim");
    let retired_destination = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());

    store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: retired_destination.clone(),
            carries: Vec::new(),
        })
        .await
        .expect("retire destination owner");
    store
        .publish_process_env(&staged_claim, &env_ref, &bytes)
        .await
        .expect("stage env");

    store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: staged.clone(),
            carries: vec![crate::ArtifactCarry {
                artifact: crate::ArtifactName {
                    store: crate::ArtifactStoreId::ProcessEnv,
                    artifact_ref: env_ref.as_str().to_owned(),
                },
                to: retired_destination.clone(),
            }],
        })
        .await
        .expect("a carry into an ended destination does not revive its edge");
    assert_eq!(
        store.get_process_env(&env_ref).await.expect("read env"),
        None,
    );
    let destination_claim =
        crate::ReferrerClaim::unguarded(retired_destination.clone()).expect("destination claim");
    let destination_error = store
        .publish_process_env(&destination_claim, &env_ref, &bytes)
        .await
        .expect_err("the destination remains fenced");
    assert!(
        matches!(destination_error, lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { ref referrer } if *referrer == retired_destination),
        "destination fence classifies by referrer: {destination_error}"
    );

    let absent = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    let missing_edge = store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: absent,
            carries: vec![crate::ArtifactCarry {
                artifact: crate::ArtifactName {
                    store: crate::ArtifactStoreId::ProcessEnv,
                    artifact_ref: "process-env:missing".to_owned(),
                },
                to: crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint()),
            }],
        })
        .await
        .expect_err("a transfer with neither edge refuses");
    assert!(
        matches!(
            missing_edge,
            lash_execution_contracts::module_artifacts::ArtifactStoreError::CarryArtifactMissing { .. }
        ),
        "missing carry bytes classify by code: {missing_edge}"
    );

    store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: staged.clone(),
            carries: Vec::new(),
        })
        .await
        .expect("retire staged owner");
    let retired_error = store
        .publish_process_env(&staged_claim, &env_ref, &bytes)
        .await
        .expect_err("a retired staging owner refuses publication");
    assert!(
        matches!(retired_error, lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded { ref referrer } if *referrer == staged),
        "staged referrer fence classifies by code: {retired_error}"
    );
}
