//! L11 (FIG-5187): the 1.0 decode-and-resume fixtures (ADR 0106 §1, §6).
//!
//! A fixture is the SQLite store a build left behind at a committed phase,
//! committed as encoded bytes (`support/images.rs`). Its actor's state is
//! written in the shipped build's format set for its kind, so it holds that
//! set's formats at their 1.0 shape. A resume test opens a copy on a fresh
//! node of this build, which claims the actor, decodes its state and
//! reaches the expected next commit:
//!
//! - `lashlang` (here): a lashlang process `worker() -> str { sleep(5);
//!   sleep(7); finish "done" }`, parked on its first sleep and released
//!   `ready` by a draining node. Its state is the engine's segment state
//!   around the VM's continuation.
//! - `session` (`vertical_crash_proof.rs`): V0's turn, cut by a crash right
//!   after its cell's `Once` operation's outcome committed: the turn
//!   checkpoint, the cell's VM continuation parked on the operation, the
//!   operation's run records and its outcome's material.
//!
//! The coverage check requires an image for every set the shipped build
//! writes (the session's, and the engine's of each engine lash ships), so
//! every format id names one. Moving a format's version moves its set: its
//! fixtures fail until their generators re-record them, which after 1.0
//! means the move needs a migration.

// Test code: the generator reads its workspace from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/images.rs"]
mod images;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::runtime::actor::process::ProcessActivation;
use lash_core_execution::runtime::actor::round::ToolBody;
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_core_execution::{
    Backend, EngineStepRun, EngineSteps as _, LifetimeDecision, ProcessId, ProcessIdMint,
    ProcessInput, ProcessProvenance, ProcessRecord, ProcessRegistration, StepRequest, StoreSet,
};
use lash_durable::runner::{Activation, Stopped};
use lash_durable::{ActorKey, ActorKind, ActorState, CommitLabel, DurableStore, LeaseConfig};
use lash_durable_test::{Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_lashlang_runtime::{
    LASHLANG_ENGINE_KIND, LashlangEngineSteps, LashlangProcessEngine, LashlangProcessInput,
    LashlangRecordedSettings, LashlangSurface,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use lashlang::testing::ast_builders as b;

use images::Image;

/// The horizon a fixture's run must finish within, in virtual time.
const HORIZON_MS: u64 = 600_000;

/// The lashlang process: a sequential mint makes it the first registered.
fn process() -> ProcessId {
    ProcessIdMint::sequential_id_for_testing(1)
}

fn actor() -> ActorKey {
    ActorKey::process(process().as_str()).expect("a process actor key")
}

/// The shipped build over `stores`: lash's durable backend with the one
/// process engine lash ships.
fn build(stores: Arc<dyn StoreSet>) -> (Backend, Arc<LashlangProcessEngine>) {
    let engine = Arc::new(LashlangProcessEngine::new(
        lashlang::LashlangArtifacts::new(stores.module_artifacts()),
        LashlangSurface::default(),
    ));
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

/// The settings a lashlang process records at creation.
fn settings() -> serde_json::Value {
    serde_json::to_value(LashlangRecordedSettings::new(
        LashlangSurface::default(),
        lashlang::ExecutionBounds::unbounded(),
    ))
    .expect("the settings encode")
}

/// The host's half of the process's steps: the engine's own bodies, under
/// their pinned `Repeatable` policy. The worker program runs no tool.
struct LashlangSteps {
    steps: LashlangEngineSteps,
    clock: Arc<SimClock>,
    backend: Backend,
    /// Every engine body entered: its kind, and whether it ran from a
    /// snapshot or from the program's entry.
    entered: Arc<Mutex<Vec<String>>>,
}

impl ProcessSteps for LashlangSteps {
    fn admit(
        &self,
        _process: &ProcessRecord,
        step: &StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        match step {
            StepRequest::Engine { .. } => Ok(StepAdmission {
                wait: None,
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
        process: &ProcessRecord,
        step: &StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> ToolBody {
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
        let steps = self.steps.clone();
        let entered = Arc::clone(&self.entered);
        // Whether the body starts from a committed VM snapshot rather than
        // from the program's entry.
        let from = if run.input.get("vm").is_some_and(|vm| !vm.is_null()) {
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
    }
}

/// A deployment of the shipped build over `stores`, on `clock`.
fn deployment(
    stores: Arc<dyn StoreSet>,
    database: Arc<dyn DurableStore>,
    clock: &Arc<SimClock>,
) -> (Arc<SimNodes>, Backend, Arc<Mutex<Vec<String>>>) {
    let (backend, engine) = build(stores);
    let entered = Arc::default();
    let activation: Arc<dyn Activation> = Arc::new(ProcessActivation::new(
        backend.clone(),
        Arc::new(LashlangSteps {
            steps: LashlangEngineSteps::new(engine),
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

/// Publish the worker module and register its process.
async fn seed(backend: &Backend) {
    let environment = LashlangSurface::default()
        .for_process_registry(true)
        .host_environment(&lash_core_execution::ToolCatalog::default())
        .expect("the host environment");
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process worker() -> str { sleep(5); sleep(7); finish \"done\" }",
        program: b::module(
            vec![b::process_returning(
                "worker",
                Vec::new(),
                lashlang::TypeExpr::Str,
                b::block(vec![
                    b::sleep_for(b::num(5.0)),
                    b::sleep_for(b::num(7.0)),
                    b::finish(b::string("done")),
                ]),
            )],
            Vec::new(),
        ),
        environment: &environment,
    })
    .expect("the worker module compiles");
    lashlang::LashlangArtifacts::of_backend(backend)
        .publish_module_artifact(
            &lash_core_execution::ReferrerClaim::unguarded(
                lash_core_execution::ArtifactReferrer::HostPin(
                    lash_core_execution::HostArtifactPin::mint(),
                ),
            )
            .expect("a host pin is unguarded"),
            &output.artifact,
        )
        .await
        .expect("the worker module publishes");
    let input = LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("worker")
            .expect("the worker export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "worker".to_owned(),
        args: serde_json::Map::new(),
    };
    let mut registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: LASHLANG_ENGINE_KIND.to_owned(),
            payload: serde_json::to_value(&input).expect("the input encodes"),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture_ref(),
    ));
    registration.engine_config = Some(settings());
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

/// Re-record the lashlang image: the process runs on node A to its first
/// sleep, A drains, and the store A leaves is the fixture.
#[tokio::test(flavor = "current_thread")]
#[ignore = "regenerates crates/lash-durable-test/tests/fixtures/formats/lashlang"]
async fn regenerate_lashlang_format_fixture() {
    let clock = SimClock::new();
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = images::database_path(dir.path());
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
            &path,
            lash_sqlite_store::SqliteStoreSetOptions {
                process_id_mint: ProcessIdMint::sequential_for_testing(),
                ..Default::default()
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
    // The VM parks on its first sleep: one vm_run, and the process waits.
    run_until(&nodes, || async {
        entered.lock_recover().as_slice() == ["vm_run:entry"]
            && state(&*database, &actor()).await == Some(ActorState::Owned)
    })
    .await;
    nodes.drain("a");
    run_until(&nodes, || async {
        matches!(nodes.stopped("a").await, Some(Ok(Stopped::Drained)))
    })
    .await;
    assert_eq!(state(&*database, &actor()).await, Some(ActorState::Ready));
    images::regenerate(&path, images::LASHLANG.name);
}

/// The lashlang image resumes on a fresh node of this build: it claims the
/// process in the engine's set, decodes the VM's continuation, wakes from
/// the sleep the VM parked on and runs the program on from there to its
/// end, never from its entry.
#[tokio::test(flavor = "current_thread")]
async fn a_lashlang_process_decodes_and_resumes_from_its_1_0_image() {
    let clock = SimClock::new();
    let (stores, _dir) = images::open(&images::LASHLANG, Arc::clone(&clock)).await;
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
        backend.formats().process(LASHLANG_ENGINE_KIND),
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
    // The resumed VM ran from its continuation: one run to the second
    // sleep, one to the end, and no run from the program's entry.
    assert_eq!(
        entered.lock_recover().as_slice(),
        ["vm_run:snapshot", "vm_run:snapshot"],
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
    let (backend, _) = build(probe);
    let mut written = vec![backend.formats().session().clone()];
    written.push(
        backend
            .formats()
            .process(LASHLANG_ENGINE_KIND)
            .expect("the build has the lashlang engine")
            .clone(),
    );
    let actors: [(&Image, ActorKey); 2] = [
        (
            &images::SESSION,
            ActorKey::session("v0-session").expect("a session actor key"),
        ),
        (&images::LASHLANG, actor()),
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
        DurableFormat::VmContinuation => Some("vm-continuation"),
        DurableFormat::LashlangSegmentHandover => {
            return Some(format!("engine/{LASHLANG_ENGINE_KIND}"));
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

    let probe = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(SimClock::new() as _)
            .await
            .expect("an in-memory store set opens"),
    );
    let (backend, _) = build(probe);
    let sets = [
        backend.formats().session().clone(),
        backend
            .formats()
            .process(LASHLANG_ENGINE_KIND)
            .expect("the build has the lashlang engine")
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
    assert_eq!(checked, 6, "the drain formats this build declares");
}
