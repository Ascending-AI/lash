//! D04 on the production route: an RLM cell calls a native tool declared
//! isolated, the turn's Run binds it at admission to a registered
//! [`lash_core::ProcessEngine`] and starts one lash process for it, and no
//! ordinary body runs (L08).
//!
//! Every law runs a real session turn on the in-process Restate double over
//! SQLite memory stores, with the double's process workflow served by the
//! core's own worker. The law's engine records each run and holds it until
//! its cancellation token fires: lash's cancellation is cooperative, and any
//! hard isolation is the host engine's own.

use super::*;
use lash_core::tool_dispatch::IsolatedProcessDescriptor;
use lash_restate_test::{CrashCount, CrashPoint};

const KIND: &str = "fig4997-engine";
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
        "an isolated process",
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
        })
    }
}

/// The law's engine: it records the process of every run and holds that run
/// until the run's cancellation token fires.
#[derive(Default)]
struct HeldEngine {
    runs: std::sync::Mutex<Vec<lash_core::ProcessId>>,
}

impl HeldEngine {
    fn runs(&self) -> Vec<lash_core::ProcessId> {
        self.runs.lock_recover().clone()
    }

    /// Wait until the engine has run `count` processes.
    async fn ran(&self, count: usize) -> Vec<lash_core::ProcessId> {
        tokio::time::timeout(WEDGE, async {
            loop {
                let runs = self.runs();
                if runs.len() >= count {
                    return runs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the engine runs the process")
    }
}

#[async_trait]
impl lash_core::ProcessEngine for HeldEngine {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn run(
        &self,
        context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> std::result::Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        self.runs.lock_recover().push(context.process_id().clone());
        context.cancellation_token().cancelled().await;
        Ok(
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({"held": "cancelled"}),
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> std::result::Result<(), lash_core::PluginError> {
        unreachable!("the held engine stores no artifacts")
    }
}

struct HeldEnginePlugin;

impl lash_core::plugin::SessionPlugin for HeldEnginePlugin {
    fn id(&self) -> &'static str {
        "fig4997-held-engine"
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

/// Contributes the law's one engine instance from every call.
struct HeldEngineFactory(Arc<HeldEngine>);

impl lash_core::plugin::PluginFactory for HeldEngineFactory {
    fn id(&self) -> &'static str {
        "fig4997-held-engine"
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
        Ok(Arc::new(HeldEnginePlugin))
    }
}

impl lash_core::plugin::PluginDefinition for HeldEngineFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("fig4997-held-engine")
    }
}

/// One law's world: the double, the core over it whose isolated tool binds
/// the law's engine, and that engine.
struct Isolated {
    double: lash_restate_test::RestateTestBackend,
    core: LashCore,
    tools: Arc<IsolatedTools>,
    engine: Arc<HeldEngine>,
}

impl Isolated {
    async fn new(bound: bool) -> Self {
        Self::on(bound, lash_restate_test::ServerConfig::default()).await
    }

    async fn on(bound: bool, config: lash_restate_test::ServerConfig) -> Self {
        let double = lash_restate_test::backend(0x4997_0001, config)
            .await
            .expect("build the Restate double");
        let engine = Arc::new(HeldEngine::default());
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
            .plugin(Arc::new(HeldEngineFactory(engine.clone())))
            .build(crate::testing::runtime_lease_owner())
            .expect("build the core");
        super::harness::serve_processes_on(&double, &core);
        Self {
            double,
            core,
            tools,
            engine,
        }
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

/// L08/D04: a supported isolated call starts exactly one lash process, run
/// by its registered engine, before any body runs, and returns its descriptor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_an_isolated_rlm_tool_starts_one_process_before_any_body_and_returns_its_descriptor()
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
            .send(TurnInput::text("start the process"))
            .id(crate::TurnId::parse("isolated-route-run").expect("nonblank host identity"))
            .await?
            .output(),
    )
    .await
    .expect("the turn settles")?;
    assert_eq!(output.status(), crate::TurnStatus::Answered, "{output:?}");
    let descriptor = descriptor(&output.result);
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
    assert_eq!(
        world.engine.ran(1).await,
        vec![descriptor.process_id],
        "the engine runs the one process"
    );
    Ok(())
}

/// L08/D04: an isolated tool its provider binds to no engine refuses at
/// admission: the call answers its typed refusal, and no body, process or
/// engine run starts.
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
            .send(TurnInput::text("start the process"))
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
    assert!(world.engine.runs().is_empty(), "no engine run");
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
        .send(TurnInput::text("start the process"))
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
/// it requests that one process's cooperative cancel.
///
/// The after-admission cancel lands while the start's preparation is not yet
/// durable, so the replayed `start:prepare` body discharges the cancel inside
/// the Run's open owner step on a turn that suspends at every await
/// (FIG-5080): the cancel is the body's own registry request and engine
/// delivery, never a journal command of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l08_isolated_cancellation_forbids_launch_before_admission_or_cancels_the_same_process_after()
 {
    let (world, output) = cancelled_at(":attempt:1", false).await;
    let output = output.unwrap();
    assert_eq!(output.status(), crate::TurnStatus::Cancelled, "{output:?}");
    assert!(world.processes().await.is_empty(), "no process launched");
    assert!(world.engine.runs().is_empty(), "no engine run");
    assert_eq!(world.tools.executions.load(Ordering::SeqCst), 0);

    // A turn cancelled while every await suspends settles without the
    // call's transient record, so the law reads the start's facts from the
    // registry.
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
    // The discharge may cancel the process before its engine runs it.
    let runs = world.engine.runs();
    assert!(
        runs.iter().all(|run| *run == process.id),
        "no other process ran: {runs:?}"
    );
    assert_eq!(world.tools.executions.load(Ordering::SeqCst), 0);
}
