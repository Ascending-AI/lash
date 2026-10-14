//! Kernel-process image recording and resume through the durable engine.
#![allow(dead_code)]
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Backend, EngineStepRun, EngineSteps as _, LifetimeDecision, ProcessId, ProcessIdMint,
    ProcessInput, ProcessProvenance, ProcessRecord, ProcessRegistration, StepRequest, StoreSet,
    WaitKind,
};
use lash_durable::runner::{Activation, Stopped};
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore, LeaseConfig, MailTx};
use lash_durable_test::{Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use lash_vm_runtime::{
    KernelDocuments, KernelEngineSteps, KernelProcessEngine, KernelProcessInput,
    LASH_VM_ENGINE_KIND,
};

use crate::{images, sim};

/// The horizon a fixture's run must finish within, in virtual time.
const HORIZON_MS: u64 = 600_000;

/// The kernel process: a sequential mint makes it the first registered.
fn process() -> ProcessId {
    ProcessIdMint::sequential_id_for_testing(1)
}

pub fn actor() -> ActorKey {
    ActorKey::process(process().as_str()).expect("a process actor key")
}

/// `worker()`: two sleeps, then its answer.
pub const WORKER: &str = r#"kernel 1
numbers float
entry worker() -> Any

fn worker() {
  do sleep 5000
  do sleep 7000
  return "done"
}

main {
  finish null
}
"#;

/// The bounds a process of the fixture's deployment runs under, which it
/// records at creation.
fn bounds() -> lash_kernel_vm::Bounds {
    let pool = sim::untimed_workers().config().run_bounds;
    lash_kernel_vm::Bounds {
        charge: 1_000_000,
        memory: 64 * 1024 * 1024,
        call_depth: pool.call_depth,
        live_tasks: pool.live_tasks,
        requests_per_park: pool.requests_per_park,
        join_members: pool.join_members,
    }
}

/// The shipped build over `stores`: lash's durable backend with the one
/// process engine lash ships, its workers held to `clock`.
pub fn build(
    stores: Arc<dyn StoreSet>,
    clock: &Arc<SimClock>,
) -> (Backend, Arc<KernelProcessEngine>) {
    let workers = sim::workers(clock);
    let engine = Arc::new(
        KernelProcessEngine::new(
            KernelDocuments::new(stores.module_artifacts()),
            lash_vm_runtime::TYPESCRIPT_DIALECT,
            workers,
            bounds(),
        )
        .expect("the shipped library assembles"),
    );
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .process_engine(Arc::clone(&engine) as _)
        .build()
        .expect("the shipped backend builds");
    (backend, engine)
}

fn config(backend: &Backend) -> SimNodesConfig {
    SimNodesConfig {
        lease: LeaseConfig::default(),
        decodes: backend.formats().decodes(),
        serves: Vec::new(),
        max_active: 4,
    }
}

/// The host's half of the process's steps: the engine's own bodies, under
/// their pinned `Repeatable` policy. The worker program runs no tool.
struct KernelSteps {
    steps: Arc<KernelEngineSteps>,
    clock: Arc<SimClock>,
    backend: Backend,
    /// Every engine body entered: its kind, and whether it ran from a
    /// parked run or from the document's entry.
    entered: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ProcessSteps for KernelSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    async fn admit(
        &self,
        _runtime: &Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        match step {
            StepRequest::Engine { .. } => Ok(StepAdmission::new(
                lash_core_execution::testing::step_tool(step, LASH_VM_ENGINE_KIND),
                ExecutionPolicy::repeatable(std::num::NonZeroU32::new(3).expect("nonzero"), 0, 0),
                ExecutionLimit::starting_at(
                    now_ms,
                    Duration::from_secs(60),
                    Duration::from_secs(60),
                ),
                None,
            )),
            StepRequest::Tool { step, effect, .. } => Err(StepRefusal::UnknownTool {
                step: step.0.clone(),
                tool: effect.to_string(),
            }),
        }
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_execution::runtime::actor::round::Material<
            lash_core_store::tool_run::CompletionSource,
        >,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::SettledOutput {
        lash_core_execution::runtime::actor::round::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        {
            let StepRequest::Engine { kind, input, .. } = step.clone() else {
                unreachable!("admit refuses a tool step");
            };
            let run = EngineStepRun {
                process: process.id.clone(),
                engine_settings: process.engine_settings().cloned(),
                effects: Arc::default(),
                now: lash_durable::DurableInstant(
                    i64::try_from(SimClock::timestamp_ms_at(self.clock.logical_ms()))
                        .expect("the clock fits"),
                ),
                clock: Arc::clone(&self.clock) as _,
                projection_providers: Some(Arc::clone(self.backend.projection_providers())),
                kind,
                input,
            };
            let steps = Arc::clone(&self.steps);
            let entered = Arc::clone(&self.entered);
            // Whether the body starts from a committed parked run rather
            // than from the document's entry.
            let from = if run
                .input
                .get("parked")
                .is_some_and(|parked| !parked.is_null())
            {
                "snapshot"
            } else {
                "entry"
            };
            Box::new(move |token| {
                entered
                    .lock_recover()
                    .push(format!("{}:{from}", run.kind.0));
                Box::pin(async move { steps.run(run, token).await.into() })
            })
        }
    }
}

/// A deployment of the shipped build over `stores`, on `clock`.
fn deployment(
    stores: Arc<dyn StoreSet>,
    database: Arc<dyn DurableStore>,
    clock: &Arc<SimClock>,
) -> (Arc<SimNodes>, Backend, Arc<Mutex<Vec<String>>>) {
    let (backend, engine) = build(stores, clock);
    let entered = Arc::default();
    let activation: Arc<dyn Activation> = Arc::new(ProcessActivation::new(
        backend.clone(),
        Arc::new(KernelSteps {
            steps: Arc::new(KernelEngineSteps::new(Arc::clone(&engine))),
            clock: Arc::clone(clock),
            backend: backend.clone(),
            entered: Arc::clone(&entered),
        }),
        Arc::new(Tripwire::default()) as _,
    ));
    let nodes = Arc::new(SimNodes::new(
        database,
        Arc::clone(clock),
        Script::new(),
        config(&backend),
        activation,
    ));
    (nodes, backend, entered)
}

/// Publish the worker document and register its process.
async fn seed(backend: &Backend, source: &str) {
    let mut source = source.to_owned();
    if source.contains("num.add(") {
        let library = lash_vm_runtime::standard_functions().expect("the shipped library");
        let (id, _) = library
            .iter()
            .find(|(_, entry)| entry.definition.name.to_string() == "num.add")
            .expect("num.add is shipped");
        source.push_str(&format!("\nuse num.add = @{id}\n"));
    }
    let document =
        lash::workflow::document::parse_document(&source).expect("the worker document parses");
    let identity = KernelDocuments::new(backend.module_artifacts())
        .publish(
            &lash_core_execution::ReferrerClaim::unguarded(
                lash_core_execution::ArtifactReferrer::HostPin(
                    lash_core_execution::HostArtifactPin::mint(),
                ),
            )
            .expect("a host pin is unguarded"),
            &document,
        )
        .await
        .expect("the worker document publishes");
    let input = KernelProcessInput {
        document: identity,
        entry: lash::workflow::document::Name::new("worker"),
        args: serde_json::Map::new(),
    };
    let settings = lash_core::EngineSettings::new(
        serde_json::to_value(lash_vm_runtime::KernelRecordedSettings::new(
            lash_vm_runtime::TYPESCRIPT_DIALECT,
            bounds(),
        ))
        .expect("the settings encode"),
    );
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: LASH_VM_ENGINE_KIND.to_owned(),
            payload: serde_json::to_value(&input).expect("the input encodes"),
            launch: lash_core::EngineLaunch::new(
                lash_core_execution::testing::process_execution_env_fixture(
                    backend.process_env_store().as_ref(),
                )
                .await,
                Some(settings),
            ),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    );
    let id = backend
        .process_registry()
        .register_process(registration)
        .await
        .expect("the worker registers")
        .id;
    assert_eq!(id, process(), "the sequential mint names the first process");
}

/// Step the deployment until `reached` holds, within the horizon.
async fn run_until<F, Fut>(nodes: &SimNodes, reached: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    nodes.quiesce().await;
    while !reached().await {
        assert!(
            nodes.clock().logical_ms() < HORIZON_MS,
            "not reached after {HORIZON_MS} ms of virtual time:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
}

async fn state(database: &dyn DurableStore, actor: &ActorKey) -> Option<ActorState> {
    database
        .actor(actor)
        .await
        .ok()
        .flatten()
        .map(|snapshot| snapshot.state)
}

/// Re-record the kernel process image: the process runs on node A to its
/// first committed sleep, A drains, and a mailbox wake readies the fixture.
pub async fn record(source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let clock = SimClock::new();
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = images::database_path(dir.path());
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
            &path,
            lash_sqlite_store::SqliteStoreSetOptions {
                process_id_mint: ProcessIdMint::sequential_for_testing(),
                ..lash_sqlite_store::SqliteStoreSetOptions::standard(
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
            },
            Arc::clone(&clock) as _,
        )
        .await
        .expect("the store opens"),
    );
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let (nodes, backend, entered) = deployment(stores, Arc::clone(&database), &clock);
    seed(&backend, source).await;
    nodes.start("a");
    // The sleep outlasts the simulated worker's polling. Record the
    // committed blocker, rather than racing the entry body's outcome or
    // the owner's release to waiting.
    run_until(&nodes, || async {
        let record = backend
            .process_registry()
            .get_process(&process())
            .await
            .expect("the process reads")
            .expect("the process is registered");
        entered.lock_recover().as_slice() == ["kernel_run:entry"]
            && record
                .waits()
                .iter()
                .any(|wait| matches!(wait.kind, WaitKind::Sleep { .. }))
            && state(&*database, &actor()).await == Some(ActorState::Waiting)
    })
    .await;
    nodes.drain("a");
    run_until(&nodes, || async {
        matches!(nodes.stopped("a").await, Some(Ok(Stopped::Drained)))
    })
    .await;
    assert_eq!(state(&*database, &actor()).await, Some(ActorState::Waiting));
    // With the old node stopped, waking makes the saved process claimable
    // without running it past the first sleep before the image is copied.
    let mut wake = MailTx::new();
    wake.wake(actor());
    database
        .commit_mail(wake, CommitLabel::new("fixture.wake"))
        .await
        .expect("the saved process wakes");
    assert_eq!(state(&*database, &actor()).await, Some(ActorState::Ready));
    (dir, path)
}

/// The kernel process image resumes on a fresh node of this build: it
/// claims the process in the engine's set, decodes the machine's parked
/// run, wakes from the sleep it parked on and runs the document on from
/// there to its end, never from its entry.
pub async fn resume(image: &images::Image) -> serde_json::Value {
    let clock = SimClock::new();
    let (stores, _dir) = images::open(image, Arc::clone(&clock)).await;
    let stores = Arc::new(stores);
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let (nodes, backend, entered) = deployment(stores, Arc::clone(&database), &clock);
    let snapshot = database
        .actor(&actor())
        .await
        .expect("the image reads")
        .expect("the image holds the process");
    assert_eq!(snapshot.state, ActorState::Ready);
    assert_eq!(
        Some(&snapshot.formats),
        backend.formats().process(LASH_VM_ENGINE_KIND),
        "the image is in another build's set"
    );
    nodes.start("b");
    run_until(&nodes, || async {
        state(&*database, &actor()).await == Some(ActorState::Terminal)
    })
    .await;

    let trace = nodes.script().trace();
    let first = trace
        .iter()
        .find(|write| write.actor.as_ref() == Some(&actor()) && write.committed())
        .map(|write| write.point.label);
    assert_eq!(
        first,
        Some(CommitLabel::PROCESS_ADVANCE),
        "the resumed process's first commit:\n{}",
        nodes.script().rendered_trace()
    );
    let record = backend
        .process_registry()
        .get_process(&process())
        .await
        .expect("the registry reads")
        .expect("the process is registered");
    let terminal = record.terminal().expect("the image reached a terminal");
    assert_eq!(
        terminal.status(),
        lash_core_execution::TerminalProcessStatus::Completed,
        "{} did not complete: {terminal:?}",
        image.name
    );
    let end = record
        .terminal()
        .map(|terminal| serde_json::to_value(terminal.clone().into_await_output()).unwrap())
        .unwrap_or_default();
    assert!(
        entered
            .lock_recover()
            .iter()
            .all(|body| body == "kernel_run:snapshot"),
        "the image restarted from entry"
    );
    assert!(
        !entered.lock_recover().is_empty(),
        "the image never resumed"
    );
    if image.name == "lashvm" {
        assert_eq!(
            entered.lock_recover().as_slice(),
            ["kernel_run:snapshot", "kernel_run:snapshot"],
            "the original image resumes its two remaining bodies"
        );
    }
    end
}
