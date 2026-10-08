//! D-DEFAULTS2: facade policy choices reach the worker and VM that execute them.
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use crate::plugins::{AdmittedPluginConfig, EngineSteps, PluginHost};
use crate::rlm::lang::{
    self, ExecutionBound, ExecutionBounds, ExecutionMode, VmExecutionStart, VmInstance, VmRequest,
    VmRunConfig, VmStep,
};
use crate::rlm::*;

async fn backend() -> std::result::Result<crate::Backend, Box<dyn std::error::Error>> {
    let stores = crate::sqlite::SqliteStoreSet::memory().await?;
    Ok(crate::durable::DurableBackendBuilder::new(Arc::new(stores)).build()?)
}

fn factory(backend: &crate::Backend, service: WorkerService) -> RlmProtocolPluginFactory {
    RlmProtocolPluginFactory::new(
        RlmProtocolPluginConfig::builder()
            .channel(RlmChannel::Cell)
            .instruction_limit(InstructionBound::instructions(1_000_000))
            .memory_limit(MemoryBound::mebibytes(64))
            .build(),
        Arc::new(TypescriptDialect),
        backend,
    )
    .with_worker_service(service)
}

async fn compile(
    factory: RlmProtocolPluginFactory,
) -> std::result::Result<ModuleCompileOutput, LashlangModuleCompileError> {
    let factory = Arc::new(factory);
    factory
        .compile_lashlang_module(
            &PluginHost::new(vec![factory.clone()]),
            false,
            LashlangModuleCompileRequest::new(
                "worker-policy",
                "const answer = 42;",
                crate::process::ProcessExecutionEnvSpec::new(
                    AdmittedPluginConfig::default(),
                    crate::runtime::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(16),
                        crate::NoProgressBudget::bounded(12),
                    ),
                ),
            ),
        )
        .await
}

/// D-DEFAULTS2: pool capacity and worker IO waits are effective facade policy.
#[tokio::test]
async fn facade_pool_capacity_and_parent_wait_reach_real_workers() {
    let backend = backend().await.expect("backend");
    let mut config = WorkerService::default().config().clone();
    config.min_workers = 2;
    config.max_workers = 2;
    config.tuning.parent_wait = Duration::from_millis(100);
    config.tuning.inbound_buffer_bytes = NonZeroUsize::new(64).expect("buffer");
    let service = WorkerService::new(config);
    let pool = service.pool().expect("prewarm");
    assert_eq!(pool.stats().workers, 2, "two real workers are prewarmed");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let error = compile(factory(&backend, service))
        .await
        .expect_err("parent wait expired");
    assert!(
        matches!(error, LashlangModuleCompileError::Worker(_)),
        "the refusal must come from the worker: {error}"
    );
}

/// D-DEFAULTS2: native parser reservations come from the service's facade config.
#[tokio::test]
async fn facade_parser_stack_policy_reaches_worker_frontend() {
    let backend = backend().await.expect("backend");
    let mut config = WorkerService::default().config().clone();
    // Arithmetically valid, but no native thread can reserve this address space.
    config.tuning.parser_stack_base_bytes = isize::MAX as usize;
    config.tuning.parser_stack_bytes_per_source_byte = 0;
    let error = compile(factory(&backend, WorkerService::new(config)))
        .await
        .expect_err("the configured reservation is unavailable");
    assert!(
        error.to_string().contains("reserve"),
        "native parser resource refusal: {error}"
    );
}

fn program(
    source: &str,
) -> std::result::Result<Arc<lang::CompiledProgram>, Box<dyn std::error::Error>> {
    let ast = crate::typescript::parse(source)?;
    let artifact = lang::ModuleArtifact::from_program(ast)?;
    Ok(Arc::new(lang::compile(&artifact, lang::Entry::Main, None)?))
}

/// D-DEFAULTS2: cancellation pacing changes when a pure VM hands control back.
#[test]
fn facade_cancellation_pacing_changes_first_vm_checkpoint() {
    let mut config = VmRunConfig::new(ExecutionMode::Foreground, ExecutionBounds::unbounded());
    config.pacing.cooperative_yield_instructions = NonZeroUsize::MIN;
    config.pacing.cancel_checkpoint_instructions = NonZeroU64::MIN;
    let step = VmInstance::pristine()
        .start(
            program("1 + 2;").expect("compile"),
            VmExecutionStart::Session,
            config,
        )
        .expect("start");
    assert!(
        matches!(step, VmStep::Suspended(suspended) if matches!(suspended.request, VmRequest::CancelCheckpoint(1))),
        "the configured first instruction yields a checkpoint"
    );
}

/// D-DEFAULTS2: GC cadence frees transient guest allocations before the spend bound.
#[test]
fn facade_gc_cadence_prevents_transient_allocations_exhausting_memory() {
    let program =
        program("let i = 0; while (i < 200) { const scratch = [i, i + 1, i + 2]; i += 1; } i;")
            .expect("compile");
    let mut config = VmRunConfig::new(
        ExecutionMode::Foreground,
        ExecutionBounds::new(
            ExecutionBound::Unbounded,
            ExecutionBound::Bounded(NonZeroU64::new(4096).expect("memory")),
        ),
    );
    config.pacing.heap_gc_allocation_interval = NonZeroU64::MIN;
    let result = VmInstance::pristine()
        .start(program.clone(), VmExecutionStart::Session, config.clone())
        .expect("start");
    assert!(
        matches!(result, VmStep::Complete(_)),
        "frequent GC fits the memory bound: {result:?}"
    );
    config.pacing.heap_gc_allocation_interval = NonZeroU64::new(10_000).expect("interval");
    let result = VmInstance::pristine()
        .start(program, VmExecutionStart::Session, config)
        .expect("start");
    assert!(
        matches!(result, VmStep::GuestError(_)),
        "deferred GC exhausts the same bound: {result:?}"
    );
}

/// D-DEFAULTS2: cache policies survive reset while guest-derived entries do not.
#[test]
fn facade_zero_cache_capacity_bypasses_residency_across_reset() {
    let tuning = WorkerTuning {
        linked_program_cache_capacity: 0,
        compiled_process_cache_capacity: 0,
        ..WorkerTuning::standard()
    };
    let mut instance = VmInstance::with_cache_capacities(
        tuning.linked_program_cache_capacity,
        tuning.compiled_process_cache_capacity,
    );
    let environment = lang::LashlangHostEnvironment::default();
    let source = "const scan = async () => 42;";
    let builders = lang::testing::ast_builders::module(
        vec![lang::testing::ast_builders::process(
            "scan",
            Vec::new(),
            lang::testing::ast_builders::block(vec![lang::testing::ast_builders::finish(
                lang::testing::ast_builders::num(42.0),
            )]),
        )],
        Vec::new(),
    );
    let artifact = lang::LinkedModule::link(builders, &environment)
        .expect("link process")
        .artifact;
    let process = artifact.process_ref("scan").expect("process");
    for _ in 0..2 {
        let cache = instance.linked_programs_mut();
        let a = cache
            .get_or_compile_ast(
                source,
                crate::typescript::parse(source).expect("parse"),
                &environment,
            )
            .expect("compile");
        let b = cache
            .get_or_compile_ast(
                source,
                crate::typescript::parse(source).expect("parse"),
                &environment,
            )
            .expect("compile");
        assert!(!Arc::ptr_eq(&a, &b), "zero capacity recompiles a cell");
        let cache = instance.compiled_processes_mut();
        let a = cache
            .get_or_compile(&artifact, process, artifact.host_requirements_ref())
            .expect("compile");
        let b = cache
            .get_or_compile(&artifact, process, artifact.host_requirements_ref())
            .expect("compile");
        assert!(!Arc::ptr_eq(&a, &b), "zero capacity recompiles a process");
        instance.reset();
    }
}

/// D-DEFAULTS2: the engine step interface uses the selected segment policy.
#[tokio::test]
async fn facade_vm_segment_policy_reaches_engine_step_admission() {
    let backend = backend().await.expect("backend");
    let policy = VmSegmentPolicy {
        execution: Duration::from_secs(7),
        attempts: NonZeroU32::new(2).expect("attempts"),
        retry_initial_ms: 11,
        retry_max_ms: 22,
    };
    let engine = LashlangProcessEngine::new(
        crate::persistence::LashlangArtifacts::of_backend(&backend),
        LashlangSurface::default(),
    )
    .with_segment_policy(policy);
    let steps = LashlangEngineSteps::new(Arc::new(engine));
    let kind = steps.kinds().pop().expect("VM run kind");
    assert_eq!(steps.execution(&kind), Duration::from_secs(7));
    assert_eq!(
        steps.retry(&kind),
        crate::tools::ExecutionPolicy::repeatable(policy.attempts, 11, 22)
    );

    let factory = Arc::new(factory(&backend, WorkerService::default()).with_segment_policy(policy));
    let host = PluginHost::new(vec![factory]);
    let runtime = host
        .install_process_engine_contributions(
            crate::durability::RuntimeHostConfig::new(
                backend,
                crate::CommitBudget::bounded(8 * 1024 * 1024, 1024),
                crate::QueuedWorkBatchingConfig::new(1),
                crate::tools::ToolSourcePolicy::Tolerate,
                crate::ExecutionBudgets::recommended(),
                crate::DeltaCoalescing::recommended(),
                crate::durability::DataRetentionConfig::standard(),
            ),
            true,
        )
        .expect("engine contribution");
    let installed = runtime
        .process_engines
        .engine_steps(crate::process::LASHLANG_ENGINE_KIND, &kind)
        .expect("registered step");
    assert_eq!(installed.execution(&kind), Duration::from_secs(7));
    assert_eq!(installed.retry(&kind), steps.retry(&kind));
}
