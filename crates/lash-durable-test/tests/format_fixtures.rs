//! L11 (FIG-5187): the 1.0 decode-and-resume fixtures (ADR 0106 §1, §6).
//!
//! A fixture is the SQLite store a build left behind at a committed phase,
//! committed as encoded bytes (`support/images.rs`). Its actor's state is
//! written in the shipped build's format set for its kind, so it holds that
//! set's formats at their 1.0 shape. A resume test opens a copy on a fresh
//! node of this build, which claims the actor, decodes its state and
//! reaches the expected next commit:
//!
//! - `lashvm` (here): a kernel process whose entry `worker()` sleeps 5,
//!   sleeps 7 and returns `"done"`, parked on its first sleep and released
//!   `ready` by a draining node. Its state is the kernel engine's state
//!   around the machine's parked run.
//! - `session` (`vertical_crash_proof.rs`): V0's turn, cut by a crash right
//!   after its cell's `Once` operation's outcome committed: the turn
//!   checkpoint, the cell's kernel continuation parked on the operation,
//!   the operation's run records and its outcome's material.
//!
//! The coverage check requires an image for every set the shipped build
//! writes (the session's, and the engine's of each engine lash ships), so
//! every format id names one. Moving a format's version moves its set: its
//! fixtures fail until their generators re-record them, which after 1.0
//! means the move needs a migration. A kernel version moves by one
//! (FIG-5716): the build that ships it decodes these images' sets as the
//! previous build's and carries the process forward at its claim.

// Test code: the generator reads its workspace from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/images.rs"]
mod images;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Backend, EngineStepRun, EngineSteps as _, LifetimeDecision, ProcessId, ProcessIdMint,
    ProcessInput, ProcessProvenance, ProcessRecord, ProcessRegistration, StepRequest, StoreSet,
};
use lash_durable::runner::{Activation, Stopped};
use lash_durable::{ActorKey, ActorKind, ActorState, CommitLabel, DurableStore, LeaseConfig};
use lash_durable_test::{Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use lash_vm_runtime::{
    KernelDocuments, KernelEngineSteps, KernelProcessEngine, KernelProcessInput,
    LASH_VM_ENGINE_KIND,
};

use images::Image;

/// The horizon a fixture's run must finish within, in virtual time.
const HORIZON_MS: u64 = 600_000;

/// The kernel process: a sequential mint makes it the first registered.
fn process() -> ProcessId {
    ProcessIdMint::sequential_id_for_testing(1)
}

fn actor() -> ActorKey {
    ActorKey::process(process().as_str()).expect("a process actor key")
}

/// `worker()`: two sleeps, then its answer.
const WORKER: &str = r#"kernel 1
numbers float
entry worker() -> Any

fn worker() {
  do sleep 5
  do sleep 7
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
fn build(stores: Arc<dyn StoreSet>, clock: &Arc<SimClock>) -> (Backend, Arc<KernelProcessEngine>) {
    let workers = sim::workers(clock);
    let engine = Arc::new(
        KernelProcessEngine::new(
            KernelDocuments::new(stores.module_artifacts()),
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
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        match step {
            StepRequest::Engine { .. } => Ok(StepAdmission {
                park: None,
                policy: ExecutionPolicy::repeatable(
                    std::num::NonZeroU32::new(3).expect("nonzero"),
                    0,
                    0,
                ),
                limit: ExecutionLimit::starting_at(
                    now_ms,
                    Duration::from_secs(60),
                    Duration::from_secs(60),
                ),
            }),
            StepRequest::Tool { step, tool, .. } => Err(StepRefusal::UnknownTool {
                step: step.0.clone(),
                tool: tool.as_str().to_owned(),
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
        lash_core_execution::runtime::actor::round::member_body({
            let StepRequest::Engine { kind, input, .. } = step.clone() else {
                unreachable!("admit refuses a tool step");
            };
            let run = EngineStepRun {
                process: process.id.clone(),
                engine_config: process.engine_config.clone(),
                tool_catalog: Arc::new(lash_core_execution::ToolCatalog::default()),
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
                Box::pin(async move { steps.run(run, token).await })
            })
        })
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
async fn seed(backend: &Backend) {
    let document =
        lash::workflow::document::parse_document(WORKER).expect("the worker document parses");
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
    let mut registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: LASH_VM_ENGINE_KIND.to_owned(),
            payload: serde_json::to_value(&input).expect("the input encodes"),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture(
            backend.process_env_store().as_ref(),
        )
        .await,
    ));
    registration.engine_config = Some(
        serde_json::to_value(lash_vm_runtime::KernelRecordedSettings::from(bounds()))
            .expect("the settings encode"),
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
/// first sleep, A drains, and the store A leaves is the fixture.
#[tokio::test(flavor = "current_thread")]
#[ignore = "regenerates crates/lash-durable-test/tests/fixtures/formats/lashvm"]
async fn regenerate_kernel_process_format_fixture() {
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
    seed(&backend).await;
    nodes.start("a");
    // The machine parks on its first sleep: one kernel_run, and the process
    // waits.
    run_until(&nodes, || async {
        entered.lock_recover().as_slice() == ["kernel_run:entry"]
            && state(&*database, &actor()).await == Some(ActorState::Owned)
    })
    .await;
    nodes.drain("a");
    run_until(&nodes, || async {
        matches!(nodes.stopped("a").await, Some(Ok(Stopped::Drained)))
    })
    .await;
    assert_eq!(state(&*database, &actor()).await, Some(ActorState::Ready));
    images::regenerate(&path, images::LASH_VM.name);
}

/// The kernel process image resumes on a fresh node of this build: it
/// claims the process in the engine's set, decodes the machine's parked
/// run, wakes from the sleep it parked on and runs the document on from
/// there to its end, never from its entry.
#[tokio::test(flavor = "current_thread")]
async fn a_kernel_process_decodes_and_resumes_from_its_1_0_image() {
    let clock = SimClock::new();
    let (stores, _dir) = images::open(&images::LASH_VM, Arc::clone(&clock)).await;
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
    let end = record
        .terminal()
        .map(|terminal| serde_json::to_value(terminal.clone().into_await_output()).unwrap())
        .unwrap_or_default();
    assert!(
        end.to_string().contains("\"done\""),
        "the process did not end with its result: {end}"
    );
    // The resumed machine ran from its parked run: one run to the second
    // sleep, one to the end, and no run from the document's entry.
    assert_eq!(
        entered.lock_recover().as_slice(),
        ["kernel_run:snapshot", "kernel_run:snapshot"],
        "the resumed process's engine bodies"
    );
}

/// Fixture coverage: every set the shipped build writes has a committed
/// image whose actor is stored in it, and every image is in one of them.
#[tokio::test(flavor = "current_thread")]
async fn every_format_set_the_build_writes_has_a_decode_and_resume_fixture() {
    let clock = SimClock::new();
    let probe = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock) as _)
            .await
            .expect("an in-memory store set opens"),
    );
    let (backend, _) = build(probe, &clock);
    let mut written = vec![backend.formats().session().clone()];
    written.push(
        backend
            .formats()
            .process(LASH_VM_ENGINE_KIND)
            .expect("the build has the kernel engine")
            .clone(),
    );
    let actors: [(&Image, ActorKey); 2] = [
        (
            &images::SESSION,
            ActorKey::session("v0-session").expect("a session actor key"),
        ),
        (&images::LASH_VM, actor()),
    ];
    assert_eq!(
        actors.len(),
        images::ALL.len(),
        "an image has no coverage entry"
    );
    let mut covered = Vec::new();
    for (image, key) in &actors {
        let (stores, _dir) = images::open(image, Arc::clone(&clock)).await;
        let snapshot = stores
            .durable_store()
            .actor(key)
            .await
            .expect("the image reads")
            .unwrap_or_else(|| panic!("image {} holds no actor {key}", image.name));
        assert!(
            written.contains(&snapshot.formats),
            "image {} is in {}, which this build does not write",
            image.name,
            snapshot.formats
        );
        assert_eq!(
            key.kind(),
            if snapshot.formats == written[0] {
                ActorKind::Session
            } else {
                ActorKind::Process
            }
        );
        covered.push(snapshot.formats);
    }
    for set in &written {
        assert!(
            covered.contains(set),
            "no decode-and-resume fixture is in {set}"
        );
    }
}

/// The id a drain format has in the actor format sets, or `None` for one
/// that no actor's state holds.
fn actor_format_id(format: lash::formats::DurableFormat) -> Option<String> {
    use lash::formats::DurableFormat;
    match format {
        DurableFormat::TurnCheckpoint => {
            Some(lash_core_execution::formats::TURN_CHECKPOINT_FORMAT_ID)
        }
        DurableFormat::RunRecord => Some(lash_core_execution::formats::RUN_RECORD_FORMAT_ID),
        DurableFormat::WaitRow => Some(lash_core_execution::formats::WAIT_ROW_FORMAT_ID),
        DurableFormat::OutcomeMaterial => {
            Some(lash_core_execution::formats::OUTCOME_MATERIAL_FORMAT_ID)
        }
        _ => None,
    }
    .map(str::to_owned)
}

/// Every persisted durable format whose stored bytes drain rather than
/// migrate is in an actor's format set at the version this build writes:
/// moving its version moves the set, and the claim filter keeps an actor
/// written in it from a node that cannot decode it (ADR 0106 §1).
#[tokio::test(flavor = "current_thread")]
async fn every_drain_format_is_in_an_actor_format_set() {
    use lash::formats::{FormatProbe, FormatVersion, UpgradePolicy};

    let clock = SimClock::new();
    let probe = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock) as _)
            .await
            .expect("an in-memory store set opens"),
    );
    let (backend, _) = build(probe, &clock);
    let sets = [
        backend.formats().session().clone(),
        backend
            .formats()
            .process(LASH_VM_ENGINE_KIND)
            .expect("the build has the kernel engine")
            .clone(),
    ];
    let members: Vec<&str> = sets
        .iter()
        .flat_map(|set| {
            let (_, members) = set.as_str().split_once(':').expect("a kind prefix");
            members.split(',')
        })
        .collect();
    let mut checked = 0;
    for entry in lash::formats::durable_formats() {
        if entry.format.upgrade_policy() != UpgradePolicy::Drain
            || entry.probe == FormatProbe::NotPersisted
        {
            continue;
        }
        let name = entry.format.name();
        let id = actor_format_id(entry.format)
            .unwrap_or_else(|| panic!("the drain format {name} is in no actor's format set"));
        let FormatVersion::Counter(version) = entry.version else {
            panic!("the drain format {name} has no counter");
        };
        let member = format!("{id}@{version}");
        assert!(
            members.contains(&member.as_str()),
            "the drain format {name} ({member}) is in no set: {sets:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 4, "the drain formats this build declares");
}
