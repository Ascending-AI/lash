//! FIG-3571 is a clean cutover: process state written before it is refused
//! with a typed terminal before any effect, never re-driven under the carrier
//! IR's node ids.
//!
//! Each case runs `run_lashlang_process` on the real predecessor bytes behind
//! an effect controller that counts every crossing, and asserts the run ends
//! typed with zero crossings and without ever building its execution runtime.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The module artifact the pre-FIG-3571 writer published (source commit
/// 3e7301d21cdddb0126761cba300645b15a2b9cda), as its store bytes.
const MODULE_ARTIFACT_PRE_FIG3571: &[u8] =
    include_bytes!("../fixtures/lashlang_module_artifact_pre_fig3571.json");
/// A segment the pre-FIG-3571 writer parked (source commit d5d4956d3).
const SEGMENT_V17_PARKED_PRE_FIG3571: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_v17_parked_pre_fig3571.json");

/// Counts every crossing of the controller boundary and executes none.
#[derive(Default)]
struct CrossingCounter {
    crossings: AtomicUsize,
}

impl lash_core::AwaitEventResolver for CrossingCounter {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for CrossingCounter {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        _local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.crossings.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            format!(
                "a refused predecessor dispatched {:?}",
                envelope.command.kind()
            ),
        ))
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.crossings.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            "a refused predecessor opened an effect group",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.crossings.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            "a refused predecessor awaited an effect group",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.crossings.fetch_add(1, Ordering::SeqCst);
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            "a refused predecessor closed an effect group",
        ))
    }
}

/// An artifact store that serves stored bytes through the same decoder, and
/// the same error mapping, the SQLite and PostgreSQL stores' reads use.
struct StoredBytesArtifactStore {
    bytes: &'static [u8],
}

#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for StoredBytesArtifactStore {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        _owner: &lash_core::ArtifactOwner,
        _artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Err(lashlang::ArtifactStoreError::Backend(
            "read-only predecessor store".to_string(),
        ))
    }

    async fn retain_module_artifact(
        &self,
        _owner: &lash_core::ArtifactOwner,
        _module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Ok(())
    }

    async fn transfer_module_artifact(
        &self,
        _from: &lash_core::ArtifactOwner,
        _to: &lash_core::ArtifactOwner,
        _module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Ok(())
    }

    async fn release_module_artifact(
        &self,
        _owner: &lash_core::ArtifactOwner,
        _module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Ok(())
    }

    async fn retire_module_artifact_owner(
        &self,
        _owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
        lashlang::ModuleArtifact::from_store_bytes(self.bytes)
            .map(|artifact| Some(Arc::new(artifact)))
            .map_err(lashlang::ArtifactStoreError::from)
    }
}

struct RefusedRun {
    outcome: lash_core::ProcessRunOutcome,
    crossings: usize,
    runtime_built: bool,
}

/// Run one process through `run_lashlang_process` behind a crossing counter.
#[expect(
    clippy::expect_used,
    reason = "test harness: each step is established by the fixture above"
)]
async fn run_counted(
    store: Arc<dyn lashlang::LashlangArtifactStore>,
    input: &LashlangProcessInput,
    handover: Option<lash_core::SegmentHandover>,
) -> RefusedRun {
    let process_id = lash_core::ProcessId::from("pre-cutover-process");
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
    let counter = Arc::new(CrossingCounter::default());
    let scoped = lash_core::ScopedEffectController::shared(
        Arc::clone(&counter) as Arc<dyn lash_core::RuntimeEffectController>,
        lash_core::AdmittedScope::process(lash_core::ProcessRef::new(
            process_id.clone(),
            incarnation,
        )),
    )
    .expect("valid process scope");
    let built = lash_core::testing::TestExecutionContextBuilder::new()
        .borrowed_effect_controller(scoped.clone())
        .build();
    let plugins = Arc::clone(&built.dispatch.plugins);
    let catalog = Arc::clone(&built.dispatch.tool_catalog);
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "pre-cutover-run")
            .bind_attempt(1);
    let runtime_built = Arc::new(AtomicBool::new(false));
    let context = lash_core::ProcessEngineRunContext::new(
        registration,
        incarnation,
        lash_core::ProcessExecutionContext::default().with_execution_write_authority(authority),
        lash_core::testing::process_work_wiring_for_registry(registry),
        lash_core::SessionId::from("pre-cutover-session"),
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
        handover,
        Box::new({
            let runtime_built = Arc::clone(&runtime_built);
            move |_catalog| {
                runtime_built.store(true, Ordering::SeqCst);
                Err(lash_core::PluginError::Session(
                    "a refused predecessor built its execution runtime".to_string(),
                ))
            }
        }),
    );
    let outcome = Box::pin(crate::process::run_lashlang_process(
        LashlangProcessEngine::new(store, LashlangSurface::default()),
        context,
        serde_json::to_value(input).expect("process input serializes"),
    ))
    .await
    .expect("a refused predecessor is a terminal, not an infrastructure fault");
    RefusedRun {
        outcome,
        crossings: counter.crossings.load(Ordering::SeqCst),
        runtime_built: runtime_built.load(Ordering::SeqCst),
    }
}

#[track_caller]
fn assert_refused_before_any_effect(run: &RefusedRun, code: &str) {
    assert!(run.outcome.is_terminal(), "the refusal is terminal");
    let lash_core::ProcessRunOutcome::Terminal { output } = &run.outcome else {
        panic!("expected a terminal, got {:?}", run.outcome);
    };
    let lash_core::ProcessAwaitOutput::Settled { output } = output.as_ref() else {
        panic!("expected a settled terminal, got {output:?}");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("expected a typed failure, got {:?}", output.outcome);
    };
    assert_eq!(failure.code, code, "{}", failure.message);
    assert_eq!(run.crossings, 0, "no effect may cross the controller");
    assert!(
        !run.runtime_built,
        "the run must stop before its execution runtime exists"
    );
}

/// A module artifact the pre-FIG-3571 writer published cannot decode under
/// the carrier IR. That is deterministic, so the run ends with the typed
/// `process_artifact_generation_retired` terminal instead of retrying a
/// fault that can never clear.
#[tokio::test(flavor = "current_thread")]
async fn pre_fig3571_module_artifact_is_a_typed_terminal_before_any_effect() {
    let stored: serde_json::Value = serde_json::from_slice(MODULE_ARTIFACT_PRE_FIG3571)
        .expect("the predecessor artifact is JSON");
    let decode = lashlang::ModuleArtifact::from_store_bytes(MODULE_ARTIFACT_PRE_FIG3571)
        .expect_err("the predecessor artifact must not decode under the carrier IR");
    assert!(matches!(
        lashlang::ArtifactStoreError::from(decode),
        lashlang::ArtifactStoreError::Decode(_)
    ));
    let (process_name, process_ref) = stored["exports"]["processes"]
        .as_object()
        .and_then(|processes| processes.iter().next())
        .expect("the predecessor exports one process");
    let input = LashlangProcessInput {
        module_ref: serde_json::from_value(stored["module_ref"].clone())
            .expect("predecessor module ref"),
        process_ref: serde_json::from_value(process_ref.clone()).expect("predecessor process ref"),
        host_requirements_ref: serde_json::from_value(stored["host_requirements_ref"].clone())
            .expect("predecessor host requirements ref"),
        process_name: process_name.clone(),
        args: serde_json::Map::new(),
    };

    let run = run_counted(
        Arc::new(StoredBytesArtifactStore {
            bytes: MODULE_ARTIFACT_PRE_FIG3571,
        }),
        &input,
        None,
    )
    .await;
    assert_refused_before_any_effect(&run, "process_artifact_generation_retired");
}

/// A sleep process the current build published, for the handover cases: were
/// it resumed, its first act would be a sleep effect.
#[expect(
    clippy::expect_used,
    reason = "test harness: each step is established by the fixture above"
)]
async fn published_sleep_process() -> (Arc<InMemoryLashlangArtifactStore>, LashlangProcessInput) {
    let store = Arc::new(InMemoryLashlangArtifactStore::new());
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
            &lash_core::ArtifactOwner::host("pre-cutover-fixture"),
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
    (store, input)
}

fn parked_handover(program_hash: String) -> lash_core::SegmentHandover {
    let fixture: serde_json::Value = serde_json::from_slice(SEGMENT_V17_PARKED_PRE_FIG3571)
        .unwrap_or_else(|error| panic!("the parked-segment fixture is JSON: {error}"));
    lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash,
        engine_state: serde_json::to_vec(&fixture["segment_state"])
            .unwrap_or_else(|error| panic!("re-encode the parked handover: {error}")),
    }
}

/// A segment the pre-FIG-3571 writer parked carries that build's program
/// identity, so its handover is refused at the identity fence.
#[tokio::test(flavor = "current_thread")]
async fn pre_fig3571_parked_segment_is_refused_at_the_identity_fence_before_any_effect() {
    let fixture: serde_json::Value = serde_json::from_slice(SEGMENT_V17_PARKED_PRE_FIG3571)
        .unwrap_or_else(|error| panic!("the parked-segment fixture is JSON: {error}"));
    let recorded = fixture["program_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("the fixture records its program hash"))
        .to_string();
    let (store, input) = published_sleep_process().await;
    let run = run_counted(store, &input, Some(parked_handover(recorded))).await;
    assert_refused_before_any_effect(&run, "restate_segment_program_hash_mismatch");
}

/// Behind the identity fence the parked bytes still meet the segment-version
/// fence: even under a matching identity, a v17 segment is refused before its
/// continuation is restored.
#[tokio::test(flavor = "current_thread")]
async fn pre_fig3571_parked_segment_is_refused_at_the_version_fence_before_any_effect() {
    let (store, input) = published_sleep_process().await;
    let current = crate::process::lashlang_program_hash(&input);
    let run = run_counted(store, &input, Some(parked_handover(current))).await;
    assert_refused_before_any_effect(&run, "process_segment_handover_invalid");
}
