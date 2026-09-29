//! FIG-4028's keyed-start fence law under the referrer model.

use pretty_assertions::assert_eq;
use std::sync::Arc;

use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupAuthorities, ArtifactCleanupPorts, ArtifactCleanupRelay, RetainedStart,
    SubscriptionRevisionStanding,
};

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

/// FIG-4130 (the residual of C7), ADR 0113 §3.3: `Start(key)`'s end, applied
/// by the relay the instant a terminal refusal arms it, keeps a concurrent
/// start's environment and engine artifacts, held by its `ProcessRecord`.
///
/// A reads the key and finds no record. B then stages its environment and
/// its engine's module under `Start(key)`, registers, and checks for a fence
/// before there is one, so it leaves its content to `Start(key)`'s cleanup;
/// the host pin that published the module is released. A ends `Start(key)`
/// on its stale read, carrying nothing, and the relay applies that end before
/// A does anything more. B's environment and module must still load, held by
/// B's `ProcessRecord` alone, and A's environment must be gone.
///
/// Red on the parent commit: the relay severed `Start(key)` with no carries
/// and reclaimed both, and A's re-read found B's row only to meet artifacts
/// already gone, which it logged.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each setup and transition"
)]
pub async fn a_start_key_end_applied_before_the_rescue_keeps_the_concurrent_start_held(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let key = crate::StartKey::for_host("start-end-applied-before-rescue");
    let module_ref = "fig-4130-concurrent-start-module";
    let module_bytes = b"the concurrent start's module".to_vec();
    let pin = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    ports
        .modules()
        .publish_module_artifact(
            &crate::ReferrerClaim::unguarded(pin.clone()).expect("pin claim"),
            module_ref,
            &module_bytes,
        )
        .await
        .expect("publish the module under the starter's pin");
    let engine_start = || {
        crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: MODULE_NAMING_ENGINE.to_owned(),
                payload: serde_json::json!({ "module": module_ref }),
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_start_key(Some(key.clone()))
    };
    let engines = || {
        crate::ProcessEngineRegistry::new().with_registration(
            crate::ProcessEngineRegistration::accepting(Arc::new(ModuleNamingEngine)),
        )
    };

    // A's starter: a process that has ended, so a start it makes is refused.
    let ended = registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
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

    // The relay, applying `Start(key)`'s end the instant A arms it.
    let relay = Arc::new(ArtifactCleanupRelay::new(ArtifactCleanupPorts {
        ledger: Arc::clone(ports.cleanup()),
        authorities: Arc::new(KeyRecords(Arc::clone(&registry))),
        process_env: Arc::clone(ports.env()),
        modules: Arc::clone(ports.modules()),
        engines: engines(),
    }));
    let relay_on_end = Arc::new(RelayOnEnd {
        inner: Arc::clone(ports.cleanup()),
        start: crate::ArtifactReferrer::Start(key.clone()),
        relay,
        verdict: std::sync::OnceLock::new(),
    });
    let ports_a = crate::ArtifactReferrerPorts::new(
        Arc::clone(ports.modules()),
        Arc::clone(ports.env()),
        Arc::clone(&relay_on_end) as Arc<dyn crate::ArtifactCleanupLedger>,
        Arc::new(crate::SystemClock),
    );
    let (engines_a, engines_b) = (
        engines().with_artifact_ports(ports_a),
        engines().with_artifact_ports(ports.clone()),
    );
    let env_store = Arc::clone(ports.env());
    let faults_a = crate::testing::ProcessRegistryFaults::new(Arc::clone(&registry));
    let journal = |operation: &str| {
        crate::ExecutionScope::runtime_operation(operation)
            .journal_identity()
            .expect("starter journal")
    };
    let (starter_a, starter_b) = (journal("refused-start-a"), journal("concurrent-start-b"));
    let stores_a = crate::ProcessStartStores {
        registry: &faults_a,
        env_store: Some(&env_store),
        engines: Some(&engines_a),
        engines_required: true,
        executor: "conformance process start",
        starter: &starter_a,
    };
    let stores_b = crate::ProcessStartStores {
        registry: registry.as_ref(),
        env_store: Some(&env_store),
        engines: Some(&engines_b),
        engines_required: true,
        executor: "conformance process start",
        starter: &starter_b,
    };
    let spec = |budget| {
        crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(budget),
        )
    };
    let spec_a = spec(crate::TurnBudget::Bounded(
        std::num::NonZeroUsize::new(5).expect("a nonzero budget"),
    ));
    let spec_b = spec(crate::TurnBudget::Bounded(
        std::num::NonZeroUsize::new(7).expect("a nonzero budget"),
    ));
    let (env_a, env_b) = (
        spec_a.stable_ref().expect("A's environment ref"),
        spec_b.stable_ref().expect("B's environment ref"),
    );
    let bytes_b = spec_b.to_store_bytes().expect("B's environment bytes");
    assert_ne!(env_a, env_b);

    let a_read_the_key = faults_a.pause_next_start_key_read();
    let refuse_a = crate::register_process_start(
        &stores_a,
        crate::started_until_starter(engine_start(), ended_scope),
        &[],
        Some(&spec_a),
    );
    let start_b = async {
        // A has staged, been refused, and read the key: no record yet.
        a_read_the_key.wait_until_validated().await;
        let started = crate::register_process_start(&stores_b, engine_start(), &[], Some(&spec_b))
            .await
            .expect("B registers under the key");
        // The starter's own hold on the module ends with its start.
        ports
            .modules()
            .end_module_referrer(&crate::ResolvedArtifactCleanup {
                referrer: pin.clone(),
                carries: Vec::new(),
            })
            .await
            .expect("release the starter's pin");
        a_read_the_key.resume();
        started
    };
    let (refused_a, started_b) = tokio::join!(refuse_a, start_b);
    let refused_a = refused_a.expect_err("A's starter has ended");
    assert_eq!(
        refused_a.code,
        crate::RuntimeErrorCode::ProcessParentEnded,
        "A is refused: {refused_a:?}"
    );
    assert_eq!(
        started_b.disposition,
        crate::ProcessRegistrationDisposition::Created
    );
    assert_eq!(started_b.record.env_ref.as_ref(), Some(&env_b));
    assert_eq!(
        relay_on_end.verdict.get(),
        Some(&crate::drive::relay::RelayVerdict::Delivered),
        "the relay applied A's end of `Start(key)` before A went on"
    );

    let module_held = || async {
        ports
            .modules()
            .get_module_artifact(module_ref)
            .await
            .expect("read B's module")
    };
    let env_held = |env_ref| {
        let env_store = Arc::clone(&env_store);
        async move {
            env_store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read an environment")
        }
    };
    assert_eq!(
        env_held(env_b.clone()).await,
        Some(bytes_b),
        "B's environment survives `Start(key)`'s end"
    );
    assert_eq!(
        module_held().await,
        Some(module_bytes),
        "B's engine artifact survives `Start(key)`'s end"
    );
    assert_eq!(
        env_held(env_a).await,
        None,
        "A's environment went with `Start(key)`"
    );

    // B's process record is what holds them: ending it reclaims both.
    let record_end = crate::ResolvedArtifactCleanup {
        referrer: crate::ArtifactReferrer::ProcessRecord(started_b.record.id.clone()),
        carries: Vec::new(),
    };
    env_store
        .end_process_env_referrer(&record_end)
        .await
        .expect("end B's record in the environment store");
    ports
        .modules()
        .end_module_referrer(&record_end)
        .await
        .expect("end B's record in the module store");
    assert_eq!(env_held(env_b).await, None);
    assert_eq!(module_held().await, None);
}

const MODULE_NAMING_ENGINE: &str = "start-staging-module";

/// An engine whose start payload names one module: an artifact a start
/// acquires and never publishes, so only its referrers keep it.
struct ModuleNamingEngine;

#[async_trait::async_trait]
impl crate::ProcessEngine for ModuleNamingEngine {
    fn kind(&self) -> &'static str {
        MODULE_NAMING_ENGINE
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        Ok(
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        let module = payload
            .get("module")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| crate::PluginError::Session("the payload names no module".into()))?;
        Ok(vec![crate::ArtifactName {
            store: crate::ArtifactStoreId::LashlangModule,
            artifact_ref: module.to_owned(),
        }])
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the engine stores no artifact `{artifact_ref}`"
        )))
    }
}

/// The one authority `Start(key)`'s end asks: the record its key holds.
struct KeyRecords(Arc<dyn crate::ProcessRegistry>);

#[async_trait::async_trait]
impl ArtifactCleanupAuthorities for KeyRecords {
    async fn journal_replay(
        &self,
        journal: &crate::EffectJournalIdentity,
    ) -> Result<crate::JournalReplay, String> {
        Err(format!("the law asks no verdict on `{}`", journal.key()))
    }

    async fn retained_start(&self, key: &crate::StartKey) -> Result<Option<RetainedStart>, String> {
        Ok(self
            .0
            .get_process_by_start_key(key)
            .await
            .map_err(|error| error.to_string())?
            .map(|record| RetainedStart {
                process_id: record.id,
                env_ref: record.env_ref,
                input: record.input,
            }))
    }

    async fn subscription_revision(
        &self,
        revision: &crate::SubscriptionRevisionId,
    ) -> Result<SubscriptionRevisionStanding, String> {
        Err(format!(
            "the law asks no subscription revision `{revision:?}`"
        ))
    }

    async fn definition_revision_current(
        &self,
        revision: &crate::DefinitionRevisionId,
    ) -> Result<bool, String> {
        Err(format!(
            "the law asks no definition revision `{revision:?}`"
        ))
    }
}

/// The store set's cleanup ledger, where arming `start`'s `Ended` record has
/// the relay deliver it at once, before the arming caller goes on.
struct RelayOnEnd {
    inner: Arc<dyn crate::ArtifactCleanupLedger>,
    start: crate::ArtifactReferrer,
    relay: Arc<ArtifactCleanupRelay>,
    verdict: std::sync::OnceLock<crate::drive::relay::RelayVerdict>,
}

#[async_trait::async_trait]
impl crate::ObligationLedger for RelayOnEnd {
    fn kind(&self) -> crate::ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &crate::ObligationKey,
        now_ms: u64,
    ) -> Result<Option<crate::ObligationId>, crate::StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ClaimedObligation>, crate::StoreError> {
        self.inner.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &crate::ObligationId,
        token: &crate::ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<crate::ClaimedObligation>, crate::StoreError> {
        self.inner.claim(id, token, now_ms, claim_ttl_ms).await
    }

    async fn settle(
        &self,
        id: &crate::ObligationId,
        token: &crate::ClaimToken,
        settlement: crate::ObligationSettlement,
        now_ms: u64,
    ) -> Result<crate::SettleOutcome, crate::StoreError> {
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(
        &self,
        id: &crate::ObligationId,
        now_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&crate::ObligationId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::StalledObligation>, crate::StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> Result<u64, crate::StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(
        &self,
        id: &crate::ObligationId,
    ) -> Result<Option<crate::ObligationStanding>, crate::StoreError> {
        self.inner.standing(id).await
    }
}

#[async_trait::async_trait]
impl crate::ArtifactCleanupLedger for RelayOnEnd {
    async fn arm_cleanup(
        &self,
        cleanup: &crate::ArtifactCleanup,
        now_ms: u64,
    ) -> Result<crate::ObligationId, crate::StoreError> {
        let id = self.inner.arm_cleanup(cleanup, now_ms).await?;
        if cleanup.plan.is_ended() && cleanup.referrer == self.start {
            let verdict =
                crate::drive::relay::deliver_now(self.relay.as_ref(), &id, &crate::SystemClock)
                    .await?;
            let _ = self.verdict.set(verdict);
        }
        Ok(id)
    }

    async fn nudge(
        &self,
        referrer: &crate::ArtifactReferrer,
        now_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        self.inner.nudge(referrer, now_ms).await
    }

    async fn load_cleanup(
        &self,
        id: &crate::ObligationId,
    ) -> Result<Option<crate::ArtifactCleanup>, crate::StoreError> {
        self.inner.load_cleanup(id).await
    }
}
