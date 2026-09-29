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
    let key = crate::StartKey::for_host("prune-referrer-fences");
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

/// FIG-4111 (the study's C7), ADR 0113 §3.3: a terminal refusal that ends
/// `Start(key)` never strands a concurrent start already staged there.
///
/// A host key is global, so two originators' starts meet under one
/// `Start(key)`. Start B stages its environment there and holds before its
/// row commits. Start A, under the same key, is refused because its starter
/// has ended; no process holds the key, so A ends `Start(key)`, carrying
/// nothing. B's row then commits. When the relay applies `Start(key)`'s end,
/// B's environment must still be held — under B's `ProcessRecord` — and load.
///
/// Red on the parent commit: B skipped acquiring under `ProcessRecord`
/// because it had staged, so severing `Start(key)` reclaimed the environment
/// of a running process.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each setup and transition"
)]
pub async fn a_refused_start_never_strands_a_concurrent_start_under_its_key(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let key = crate::StartKey::for_host("refused-start-concurrent-stager");
    let external = || {
        crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
    };
    // An engine start: it carries an environment to stage.
    let engine_start = || {
        crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: "testing-fixture".to_owned(),
                payload: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_start_key(Some(key.clone()))
    };
    // A's starter: a process that has ended, so a start it makes is refused.
    let ended = registry
        .register_process(external())
        .await
        .expect("register the ended starter");
    registry
        .complete_process(
            &ended.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("end the starter");
    let ended_scope = crate::ScopeId::process(ended.id.clone());

    let faults = crate::testing::ProcessRegistryFaults::new(Arc::clone(&registry));
    let engines = crate::testing::process_engine_fixture().with_artifact_ports(ports.clone());
    let env_store = Arc::clone(ports.env());
    let journal = |operation: &str| {
        crate::ExecutionScope::runtime_operation(operation)
            .journal_identity()
            .expect("starter journal")
    };
    let (starter_a, starter_b) = (journal("refused-start-a"), journal("staged-start-b"));
    let stores = |starter| crate::ProcessStartStores {
        registry: &faults,
        env_store: Some(&env_store),
        engines: Some(&engines),
        engines_required: false,
        executor: "conformance process start",
        starter,
    };
    let (stores_a, stores_b) = (stores(&starter_a), stores(&starter_b));
    let spec = |budget| {
        crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(budget),
        )
    };
    let spec_b = spec(crate::TurnBudget::Unbounded);
    let spec_a = spec(crate::TurnBudget::Bounded(
        std::num::NonZeroUsize::new(3).expect("a nonzero budget"),
    ));
    let env_b = spec_b.stable_ref().expect("B's environment ref");
    let bytes_b = spec_b.to_store_bytes().expect("B's environment bytes");
    assert_ne!(spec_a.stable_ref().expect("A's environment ref"), env_b);

    let paused = faults.pause_next_registration();
    let start_b = crate::register_process_start(&stores_b, engine_start(), &[], Some(&spec_b));
    let refuse_a = async {
        // B has staged under `Start(key)` and holds before its row.
        paused.wait_until_validated().await;
        let refused = crate::register_process_start(
            &stores_a,
            crate::started_until_starter(engine_start(), ended_scope.clone()),
            &[],
            Some(&spec_a),
        )
        .await
        .expect_err("A's starter has ended");
        paused.resume();
        refused
    };
    let (started_b, refused_a) = tokio::join!(start_b, refuse_a);
    assert_eq!(
        refused_a.code,
        crate::RuntimeErrorCode::ProcessParentEnded,
        "A is refused: {refused_a:?}"
    );
    let started_b = started_b.expect("B registers under the key");
    assert_eq!(
        started_b.disposition,
        crate::ProcessRegistrationDisposition::Created
    );
    assert_eq!(started_b.record.env_ref.as_ref(), Some(&env_b));

    // The relay applies A's abandonment: `Start(key)` carries nothing.
    env_store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: crate::ArtifactReferrer::Start(key),
            carries: Vec::new(),
        })
        .await
        .expect("apply Start(key)'s end");
    assert_eq!(
        env_store
            .get_process_execution_env(&env_b)
            .await
            .expect("load B's environment"),
        Some(bytes_b),
        "B's environment is held by its process record, not stranded"
    );
}
