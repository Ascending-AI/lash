//! D04 on the production route: an RLM cell calls a native tool declared
//! isolated, the turn's Run binds it at admission to a registered
//! [`WorkerProcessEngine`] and starts one real OS worker under one lash
//! process, and no ordinary body runs (L08).
//!
//! Every law runs a real session turn on the in-process Restate double over
//! SQLite memory stores, with the double's process workflow served by the
//! core's own worker. The worker program writes its PID to a marker file
//! and sleeps, so the law sees every OS worker the engine spawned.

use super::*;
use lash_core::PhysicalProcessWorker as _;
use lash_core::tool_dispatch::IsolatedProcessDescriptor;
use lash_restate_test::{CrashCount, CrashPoint};

const KIND: &str = "fig4997-worker";
const TOOL: &str = "iso_run";
/// How long a law waits on a step that only a wedge delays.
const WEDGE: std::time::Duration = std::time::Duration::from_secs(120);

struct IsolatedTools {
    bound: bool,
    executions: AtomicUsize,
}

fn isolated_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:iso_run",
        TOOL,
        "an isolated worker",
        serde_json::json!({"type":"object","properties":{"label":{"type":"string"}},"required":["label"],"additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    )
    .unwrap()
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["iso"], "run"))
    .with_declaration(lash_core::tool_run::ToolDeclaration {
        isolated: true,
        ..Default::default()
    })
}

#[async_trait]
impl ToolProvider for IsolatedTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![isolated_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(isolated_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!({"ordinary": true})).into()
    }

    fn isolated_process(
        &self,
        call: lash_core::IsolatedProcessRequest<'_>,
    ) -> Option<lash_core::IsolatedProcessBinding> {
        self.bound.then(|| lash_core::IsolatedProcessBinding {
            engine: KIND.to_owned(),
            payload: call.args.clone(),
            boundary: lash_core::ProcessExecutionBoundary::WorkerProcess,
        })
    }
}

struct WorkerEnginePlugin;

impl lash_core::plugin::SessionPlugin for WorkerEnginePlugin {
    fn id(&self) -> &'static str {
        "fig4997-worker-engine"
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

/// Contributes the law's one engine instance from every call.
struct WorkerEngineFactory(Arc<lash_core::WorkerProcessEngine>);

impl lash_core::plugin::PluginFactory for WorkerEngineFactory {
    fn id(&self) -> &'static str {
        "fig4997-worker-engine"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError>
    {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            self.0.clone(),
        )])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(WorkerEnginePlugin))
    }
}

impl lash_core::plugin::PluginDefinition for WorkerEngineFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("fig4997-worker-engine")
    }
}

/// One law's world: the double, the core over it, the engine and the PID
/// marker its workers write.
struct Isolated {
    double: lash_restate_test::RestateTestBackend,
    core: LashCore,
    tools: Arc<IsolatedTools>,
    engine: Arc<lash_core::WorkerProcessEngine>,
    marker: Arc<tempfile::TempDir>,
}

impl Isolated {
    async fn new(bound: bool) -> Self {
        Self::on(bound, lash_restate_test::ServerConfig::default()).await
    }

    async fn on(bound: bool, config: lash_restate_test::ServerConfig) -> Self {
        let double = lash_restate_test::backend(0x4997_0001, config)
            .await
            .expect("build the Restate double");
        Self::over(double, Arc::new(tempfile::tempdir().unwrap()), bound).await
    }

    async fn over(
        double: lash_restate_test::RestateTestBackend,
        marker: Arc<tempfile::TempDir>,
        bound: bool,
    ) -> Self {
        let engine = Arc::new(
            lash_core::WorkerProcessEngine::new(
                KIND,
                lash_core::WorkerCommand {
                    program: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "echo $$ >> \"$1\"; exec sleep 600".into(),
                        "sh".into(),
                        marker.path().join("pids").into_os_string(),
                    ],
                },
                marker.path().join("ownership"),
            )
            .with_spawn_wait(std::time::Duration::from_secs(2)),
        );
        let tools = Arc::new(IsolatedTools {
            bound,
            executions: AtomicUsize::new(0),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
            .serve_test_llm_profile(
                crate::testing::TestProvider::builder()
                    .kind("isolated-tool-route")
                    .complete(move |_| {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        async move {
                            Ok(text_response(&typescript_block(if call == 0 {
                                "const started = await iso.run({label: \"fig4997\"});\nfinish(started);"
                            } else {
                                "finish(\"asked again\");"
                            })))
                        }
                    })
                    .build()
                    .into_handle(),
                mock_llm_profile_spec(),
            )
            .tools(tools.clone())
            .plugin(Arc::new(WorkerEngineFactory(engine.clone())))
            .build(crate::testing::runtime_lease_owner())
            .expect("build the core");
        super::harness::serve_processes_on(&double, &core);
        Self {
            double,
            core,
            tools,
            engine,
            marker,
        }
    }

    /// The PIDs of every OS worker the engine spawned.
    #[expect(
        clippy::disallowed_methods,
        reason = "the law reads the PID marker its workers write"
    )]
    fn pids(&self) -> Vec<u32> {
        std::fs::read_to_string(self.marker.path().join("pids"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim().parse().unwrap())
            .collect()
    }

    /// Wait until `count` workers wrote their PIDs.
    async fn spawned(&self, count: usize) -> Vec<u32> {
        tokio::time::timeout(WEDGE, async {
            loop {
                let pids = self.pids();
                if pids.len() >= count {
                    return pids;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the worker writes its PID")
    }

    /// Wait until a redelivered process workflow has committed its terminal.
    async fn settled(&self, process_id: &lash_core::ProcessId) {
        tokio::time::timeout(WEDGE, async {
            loop {
                if self
                    .processes()
                    .await
                    .iter()
                    .any(|record| record.id == *process_id && record.outcome().is_some())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the recovered process settles without spawning");
    }

    /// The engine processes the registry holds for the law's engine.
    async fn processes(&self) -> Vec<lash_core::ProcessRecord> {
        self.double
            .lash_backend()
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await
            .unwrap()
            .into_iter()
            .filter(|record| {
                matches!(record.input.as_ref(), lash_core::ProcessInput::Engine { kind, .. } if kind == KIND)
            })
            .collect()
    }
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// The descriptor the isolated call presented, from the turn's tool records.
fn descriptor(report: &crate::TurnReport) -> IsolatedProcessDescriptor {
    let record = report
        .tool_calls
        .iter()
        .find(|record| record.tool == TOOL)
        .unwrap_or_else(|| panic!("the turn records its isolated call: {report:?}"));
    serde_json::from_value(record.output.value_for_projection())
        .unwrap_or_else(|error| panic!("the call presents its descriptor ({error}): {record:?}"))
}

fn isolated_key(key: &lash_core::StartKey) -> bool {
    key.as_str().starts_with("process-start-key:v1:isolated:")
}

/// L08/D04: a supported isolated call starts exactly one OS worker under one
/// lash process before any body runs, and returns its descriptor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_an_isolated_rlm_tool_starts_one_worker_process_before_any_body_and_returns_its_descriptor()
-> Result<()> {
    let world = Isolated::new(true).await;
    let session = world
        .core
        .session(crate::SessionId::parse("isolated-route").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let output = tokio::time::timeout(
        WEDGE,
        session
            .send(TurnInput::text("start the worker"))
            .id(crate::TurnId::parse("isolated-route-run").expect("nonblank host identity"))
            .await?
            .output(),
    )
    .await
    .expect("the turn settles")?;
    assert_eq!(output.status(), crate::TurnStatus::Answered, "{output:?}");
    let descriptor = descriptor(&output.result);
    assert_eq!(
        descriptor.boundary,
        lash_core::ProcessExecutionBoundary::WorkerProcess
    );
    assert_eq!(descriptor.termination, None);
    assert!(
        isolated_key(&descriptor.start_key),
        "{:?}",
        descriptor.start_key
    );
    let processes = world.processes().await;
    assert_eq!(processes.len(), 1, "one lash process: {processes:?}");
    assert_eq!(processes[0].id, descriptor.process_id);
    assert_eq!(processes[0].start_key.as_ref(), Some(&descriptor.start_key));
    assert_eq!(
        world.tools.executions.load(Ordering::SeqCst),
        0,
        "no ordinary body"
    );
    let pids = world.spawned(1).await;
    assert_eq!(pids.len(), 1, "one OS worker");
    assert_ne!(
        pids[0],
        std::process::id(),
        "the worker is its own OS process"
    );
    assert!(alive(pids[0]));
    let receipt = world
        .engine
        .terminate_worker(&descriptor.process_id)
        .await?;
    assert_eq!(receipt.worker_pid.get(), pids[0]);
    assert!(!alive(pids[0]), "the worker is reaped");
    Ok(())
}

/// L08/D04: an isolated tool its provider binds to no engine refuses at
/// admission: the call answers its typed refusal, and no body, process or
/// worker starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_an_unbound_isolated_tool_refuses_typed_before_any_body() -> Result<()> {
    let world = Isolated::new(false).await;
    let session = world
        .core
        .session(crate::SessionId::parse("isolated-unbound").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let output = tokio::time::timeout(
        WEDGE,
        session
            .send(TurnInput::text("start the worker"))
            .id(crate::TurnId::parse("isolated-unbound-run").expect("nonblank host identity"))
            .await?
            .output(),
    )
    .await
    .expect("the turn settles")?;
    let record = output
        .result
        .tool_calls
        .iter()
        .find(|record| record.tool == "tool:iso_run")
        .unwrap_or_else(|| panic!("the turn records the refused call: {output:?}"));
    let lash_core::ToolCallOutcome::Failure(failure) = &record.output.outcome else {
        panic!("the refused call answers a failure: {record:?}");
    };
    assert_eq!(failure.code, lash_core::ToolAdmissionRefusal::CODE);
    assert_eq!(
        failure.cause.as_deref(),
        Some(&lash_core::ToolFailureCause::Admission {
            refusal: lash_core::ToolAdmissionRefusal::UnsupportedIsolation
        }),
        "{failure:?}"
    );
    assert_eq!(
        world.tools.executions.load(Ordering::SeqCst),
        0,
        "no ordinary body"
    );
    assert!(world.processes().await.is_empty(), "no process");
    assert!(world.pids().is_empty(), "no worker");
    Ok(())
}

/// Run the isolated call with every attempt of the turn dropped after the
/// run named `…{suffix}` ran but before its result is durable, so the turn
/// replays to that point and no further; cancel the turn there, then let the
/// next replay pass it. An `always_replay` double suspends the turn at every
/// await its journal cannot answer, as the replay leg's server does.
async fn cancelled_at(suffix: &str, always_replay: bool) -> (Isolated, Result<crate::TurnOutput>) {
    let world = Isolated::on(
        true,
        lash_restate_test::ServerConfig::default().always_replay(always_replay),
    )
    .await;
    let mut crashes = CrashCount::new();
    assert!(world.double.server().on_crash(crashes.listener()));
    world.double.server().crash_on(
        lash_restate_test::CrashRule::new(CrashPoint::BeforeRunResultEnding {
            suffix: suffix.to_owned(),
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .handler("run")
        .times(u32::MAX),
    );
    let name = format!("isolated-cancel{}", suffix.replace(':', "-"));
    let session = world
        .core
        .session(lash_core::SessionId::fixture(name))
        .created()
        .await
        .open()
        .await
        .unwrap();
    let handle = session
        .send(TurnInput::text("start the worker"))
        .id(crate::TurnId::parse("isolated-cancel-run").expect("nonblank host identity"))
        .await
        .unwrap();
    tokio::time::timeout(WEDGE, crashes.wait_until(1))
        .await
        .expect("the cut lands")
        .unwrap();
    handle
        .cancel()
        .origin("isolated-cancellation-law")
        .await
        .unwrap();
    world.double.server().clear_crashes();
    let output = tokio::time::timeout(WEDGE, handle.output())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{suffix}: the cancelled turn settles: {:#?}",
                world.double.server().invocations()
            )
        });
    (world, output)
}

/// L08/K5: a cancel before the start's admission forbids the launch; a
/// cancel after it recovers the same StartKey and process, and discharging
/// it terminates and reaps the one worker with a physical receipt.
///
/// The after-admission cancel lands while the start's preparation is not yet
/// durable, so the replayed `start:prepare` body discharges the cancel inside
/// the Run's open owner step on a turn that suspends at every await
/// (FIG-5080): the cancel is the body's own registry request and engine
/// delivery, never a journal command of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_isolated_cancellation_forbids_launch_before_admission_or_reaps_the_same_worker_after()
{
    let (world, output) = cancelled_at(":attempt:1", false).await;
    let output = output.unwrap();
    assert_eq!(output.status(), crate::TurnStatus::Cancelled, "{output:?}");
    assert!(world.processes().await.is_empty(), "no process launched");
    assert!(world.pids().is_empty(), "no worker spawned");
    assert_eq!(world.tools.executions.load(Ordering::SeqCst), 0);

    // A turn cancelled while every await suspends settles without the
    // call's transient record, so the law reads the start's facts from the
    // registry and the engine's recorded receipt.
    let (world, output) = cancelled_at(":start:prepare", true).await;
    output.unwrap();
    let processes = world.processes().await;
    assert_eq!(processes.len(), 1, "one process: {processes:?}");
    let process = &processes[0];
    assert!(
        process.start_key.as_ref().is_some_and(isolated_key),
        "{process:?}"
    );
    assert!(
        process.cancel_request.is_some(),
        "the discharge requested the cancel: {process:?}"
    );
    // The discharge may reap the worker before its shell writes the marker.
    let receipt = world.engine.terminate_worker(&process.id).await.unwrap();
    let pid = receipt.worker_pid.get();
    assert!(!alive(pid), "the discharge reaped the worker");
    let pids = world.pids();
    assert!(
        pids.iter().all(|spawned| *spawned == pid),
        "no other worker: {pids:?}"
    );
    assert_eq!(world.tools.executions.load(Ordering::SeqCst), 0);
}

/// L08/K5 (FIG-5011/S19): a cold deployment redelivers the process workflow
/// under the same StartKey, adopting its live worker rather than duplicating it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_cold_process_redelivery_adopts_the_live_worker_without_a_second_launch() -> Result<()>
{
    let world = Isolated::new(true).await;
    let session = world
        .core
        .session("isolated-orphan".parse().unwrap())
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("start"))
        .id("orphan-run".parse().unwrap())
        .await?
        .output()
        .await?;
    let descriptor = descriptor(&output.result);
    let original = world.spawned(1).await[0];
    // Retain the OS supervisor, as SIGKILL leaves its worker alive, but lose
    // every engine slot reachable by the restarted process workflow.
    let old_engine = world.engine;
    let old_core = world.core;
    let fresh = Isolated::over(world.double.restart().await.unwrap(), world.marker, true).await;
    let receipt = fresh
        .engine
        .terminate_worker(&descriptor.process_id)
        .await?;
    assert_eq!(
        receipt.worker_pid.get(),
        original,
        "recovery must terminate the original worker"
    );
    fresh.settled(&descriptor.process_id).await;
    assert_eq!(
        fresh.pids(),
        vec![original],
        "recovery launches no replacement"
    );
    assert!(!alive(original), "the adopted worker is reaped");
    drop((old_core, old_engine));
    Ok(())
}

/// L08/K5 (FIG-5011/S20): losing host-local slots after the physical reap
/// preserves its receipt and never gives that StartKey a replacement worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_cold_process_redelivery_keeps_the_reaped_worker_and_its_receipt() -> Result<()> {
    let world = Isolated::new(true).await;
    let session = world
        .core
        .session("isolated-reaped".parse().unwrap())
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("start"))
        .id("reaped-run".parse().unwrap())
        .await?
        .output()
        .await?;
    let descriptor = descriptor(&output.result);
    let original = world.spawned(1).await[0];
    let mut crashes = CrashCount::new();
    assert!(world.double.server().on_crash(crashes.listener()));
    world.double.server().crash_on(
        lash_restate_test::CrashRule::new(CrashPoint::BeforeRun {
            name: "lash.process.complete".to_owned(),
        })
        .service("LashProcessWorkflow")
        .handler("run")
        .times(u32::MAX),
    );
    let receipt = world
        .engine
        .terminate_worker(&descriptor.process_id)
        .await?;
    assert_eq!(receipt.worker_pid.get(), original);
    tokio::time::timeout(WEDGE, crashes.wait_until(1))
        .await
        .unwrap()
        .unwrap();
    let old_core = world.core;
    let old_engine = world.engine;
    let fresh = Isolated::over(world.double.restart().await.unwrap(), world.marker, true).await;
    fresh.double.server().clear_crashes();
    let recovered = fresh
        .engine
        .terminate_worker(&descriptor.process_id)
        .await?;
    // Waiting for redelivery prevents the marker assertion from missing
    // a replacement launched after cold cancellation recovered the receipt.
    fresh.settled(&descriptor.process_id).await;
    assert_eq!(
        recovered, receipt,
        "cold recovery retains the physical receipt"
    );
    assert_eq!(
        fresh.pids(),
        vec![original],
        "a reaped worker is never respawned"
    );
    assert!(!alive(original));
    drop((old_core, old_engine));
    Ok(())
}
