//! Process state another generation wrote is refused with a typed terminal
//! before any effect, never redriven under this build's node ids. The
//! refusals name the generation they found, so a drain can identify that
//! state.
//!
//! Each case runs `run_lashlang_process` on a segment parked by this build and
//! restamped as another generation's, behind an effect controller that counts
//! every crossing, and asserts the run ends typed with zero crossings and
//! without ever building its execution runtime.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Counts every crossing of the controller boundary and executes none.
#[derive(Default)]
struct CrossingCounter {
    crossings: AtomicUsize,
}

impl lash_core::AwaitEventResolver for CrossingCounter {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

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
}

struct StoredBytesArtifactStore {
    bytes: Vec<u8>,
}

#[async_trait::async_trait]
impl lash_core::ModuleArtifactStore for StoredBytesArtifactStore {
    fn durability_tier(&self) -> lash_core::DurabilityTier {
        lash_core::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _module_ref: &str,
        _bytes: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Err(lash_core::ArtifactStoreError::Backend(
            "read-only predecessor store".to_string(),
        ))
    }

    async fn acquire_module_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _module_ref: &str,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn end_module_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        Ok(Some(self.bytes.to_vec()))
    }
}

struct RefusedRun {
    outcome: lash_core::ProcessRunOutcome,
    crossings: usize,
    runtime_built: bool,
}

/// Run one process through `run_lashlang_process` behind a crossing counter.
async fn run_counted(
    store: lashlang::LashlangArtifacts,
    input: &LashlangProcessInput,
    handover: Option<lash_core::SegmentHandover>,
) -> RefusedRun {
    let registration = lash_core::ProcessRegistration::new(
        input.to_process_input().expect("valid process input"),
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
        input.process_identity(),
    ));
    // The refusal lands before the run reads the registry, so the process is
    // never registered and a fixture id stands in for the one a registrar mints.
    let process_id = lash_core::ProcessId::fixture("pre-cutover-process");
    let counter = Arc::new(CrossingCounter::default());
    let scoped = lash_core::ScopedEffectController::shared(
        Arc::clone(&counter) as Arc<dyn lash_core::RuntimeEffectController>,
        lash_core::AdmittedScope::process(process_id.clone()),
    )
    .expect("valid process scope");
    let built =
        lash_core::testing::TestExecutionContextBuilder::over_controller(scoped.clone()).build();
    let plugins = Arc::clone(&built.dispatch.plugins);
    let catalog = Arc::clone(&built.dispatch.tool_catalog);
    let registry: Arc<dyn lash_core::ProcessRegistry> = crate::lib_tests::sqlite_memory_store_set()
        .await
        .process_registry();
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "pre-cutover-run",
    )
    .bind_attempt(1);
    let runtime_built = Arc::new(AtomicBool::new(false));
    let context = lash_core::ProcessEngineRunContext::new(
        registration,
        process_id.clone(),
        lash_core::ProcessExecutionContext::default().with_execution_write_authority(authority),
        lash_core::testing::process_work_wiring_for_registry(registry),
        plugins,
        catalog,
        None,
        None,
        Arc::new(lash_core::NoSessionWork::new()),
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
        LashlangProcessEngine::new(
            store,
            LashlangSurface::default(),
            crate::lib_tests::sqlite_recording_backend()
                .await
                .worker_recovery(),
        ),
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

/// The shared resume-refusal terminal (FIG-3588): the process is Abandoned
/// with `ResumeRefused` evidence carrying exactly `reason`.
#[track_caller]
fn assert_resume_refused_before_any_effect(
    run: &RefusedRun,
    reason: lash_core::ProcessResumeRefusal,
) {
    let lash_core::ProcessRunOutcome::Terminal { output, .. } = &run.outcome else {
        panic!("expected a terminal, got {:?}", run.outcome);
    };
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, control } = output.as_ref() else {
        panic!("expected an Abandoned terminal, got {output:?}");
    };
    assert_eq!(
        evidence.writer,
        lash_core::AbandonWriter::ResumeRefused { reason },
        "the refusal is the shared resume-refusal terminal"
    );
    assert!(control.is_none(), "a resume refusal carries no control");
    assert_stopped_before_any_effect(run);
}

#[track_caller]
fn assert_stopped_before_any_effect(run: &RefusedRun) {
    assert_eq!(run.crossings, 0, "no effect may cross the controller");
    assert!(
        !run.runtime_built,
        "the run must stop before its execution runtime exists"
    );
}

/// A sleep process the current build published, for the handover cases: were
/// it resumed, its first act would be a sleep effect.
async fn published_sleep_process() -> (LashlangArtifacts, LashlangProcessInput) {
    let store = crate::lib_tests::memory_artifact_store().await;
    let environment = LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        LashlangAbilities::default().with_sleep(),
    );
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process pause() -> null { finish await sleep_for(0) }",
        program: process_module(
            "pause",
            Vec::new(),
            lashlang::TypeExpr::Null,
            b::sleep_for(b::num(0.0)),
        ),
        environment: &environment,
    })
    .expect("sleep process compiles");
    store
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
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

/// The handover of a segment another generation parked: this build's golden
/// with its envelope restamped one version on.
fn other_generation_handover(program_hash: String) -> (lash_core::SegmentHandover, u32) {
    let mut segment_state =
        crate::process::segment_trace_tests::parked_loop_segment_golden()["segment_state"].clone();
    let other_version = crate::LASHLANG_SEGMENT_STATE_VERSION + 1;
    segment_state["version"] = serde_json::json!(other_version);
    let handover = lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash,
        engine_state: serde_json::to_vec(&segment_state)
            .unwrap_or_else(|error| panic!("re-encode the parked handover: {error}")),
    };
    (handover, other_version)
}

/// A segment another build parked carries that build's program identity, so
/// its handover is refused at the identity fence: the shared resume refusal,
/// naming the identity it found.
async fn another_builds_parked_segment_is_refused_at_the_identity_fence_before_any_effect_law() {
    let recorded = "sha256:another-build".to_string();
    let (store, input) = published_sleep_process().await;
    let (handover, _) = other_generation_handover(recorded.clone());
    let run = run_counted(store, &input, Some(handover)).await;
    assert_resume_refused_before_any_effect(
        &run,
        lash_core::ProcessResumeRefusal::RetiredGeneration { found: recorded },
    );
}

/// Behind the identity fence the parked bytes still meet the segment-version
/// fence: even under a matching identity, another generation's segment is
/// refused with the shared resume refusal before its continuation is restored.
async fn another_generations_parked_segment_is_refused_at_the_version_fence_before_any_effect_law()
{
    let (store, input) = published_sleep_process().await;
    let current = crate::process::lashlang_program_hash(&input);
    let (handover, other_version) = other_generation_handover(current);
    let run = run_counted(store, &input, Some(handover)).await;
    assert_resume_refused_before_any_effect(
        &run,
        lash_core::ProcessResumeRefusal::RetiredGeneration {
            found: format!("lashlang-segment-state-v{other_version}"),
        },
    );
}

#[tokio::test(flavor = "current_thread")]
async fn another_builds_parked_segment_is_refused_at_the_identity_fence_before_any_effect() {
    another_builds_parked_segment_is_refused_at_the_identity_fence_before_any_effect_law().await;
}

#[tokio::test(flavor = "current_thread")]
async fn another_generations_parked_segment_is_refused_at_the_version_fence_before_any_effect() {
    another_generations_parked_segment_is_refused_at_the_version_fence_before_any_effect_law()
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn another_artifact_family_is_a_typed_terminal_before_any_effect() {
    let (store, input) = published_sleep_process().await;
    let bytes = store
        .store()
        .get_module_artifact(input.module_ref.as_str())
        .await
        .expect("read current artifact")
        .expect("published artifact");
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).expect("artifact JSON");
    wire["family"] = serde_json::json!("unsupported-artifact-family");
    let bytes = serde_json::to_vec(&wire).expect("encode foreign artifact family");
    let decode =
        lashlang::ModuleArtifact::from_store_bytes(&bytes).expect_err("foreign family refuses");
    assert!(matches!(
        lash_core::ArtifactStoreError::from(decode),
        lash_core::ArtifactStoreError::UnsupportedGeneration { .. }
    ));
    let run = run_counted(
        lashlang::LashlangArtifacts::new(Arc::new(StoredBytesArtifactStore { bytes })),
        &input,
        None,
    )
    .await;
    assert_resume_refused_before_any_effect(
        &run,
        lash_core::ProcessResumeRefusal::RetiredGeneration {
            found: input.module_ref.to_string(),
        },
    );
}
