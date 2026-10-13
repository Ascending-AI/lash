//! Capacity verdicts on engine steps (FIG-6155): a step the host lacked
//! capacity for records nothing and runs again uncounted at its ordinal,
//! after a backoff; a process whose claims keep meeting the verdict parks
//! through the activation-loop budget, and a redrive runs it again. Only a
//! failure of the step's own spends its `Repeatable` attempts. A kernel run
//! that meets a full worker pool waits for capacity instead of ending its
//! process.

use super::*;

/// The engine whose one `saturating` step answers capacity verdicts.
const SATURATING_ENGINE: &str = "core-node-saturating-engine";

/// The body that answers a capacity verdict while its host is saturated.
const SATURATING: &str = "saturating";

/// What the `saturating` body answers once it has capacity.
#[derive(Clone, Copy)]
enum Then {
    Complete,
    Fail,
}

/// The `saturating` body: it answers a capacity verdict while `verdicts`
/// says the host is saturated (each run spends one; `usize::MAX` until a
/// law frees it), and then what `then` names. It counts its runs.
struct Saturating {
    runs: Arc<AtomicUsize>,
    verdicts: Arc<AtomicUsize>,
    then: Then,
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for Saturating {
    fn kinds(&self) -> Vec<lash_core::EngineStepKind> {
        vec![lash_core::EngineStepKind::new(SATURATING)]
    }

    fn execution(&self, _kind: &lash_core::EngineStepKind) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

    fn retry(&self, _kind: &lash_core::EngineStepKind) -> lash_core::ExecutionPolicy {
        lash_core::ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN.saturating_add(2), 0, 0)
    }

    async fn run(
        &self,
        run: lash_core::EngineStepRun,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> lash_core::EngineStepAnswer {
        use lash_core::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
        self.runs.fetch_add(1, Ordering::SeqCst);
        let saturated = self
            .verdicts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| match left {
                0 => None,
                usize::MAX => Some(usize::MAX),
                left => Some(left - 1),
            })
            .is_ok();
        if saturated {
            return lash_core::EngineStepAnswer::Capacity(lash_core::CapacityVerdict::new(
                lash_core::CapacityCause::QueueFull,
                Duration::from_millis(1),
                "the law's host is saturated",
            ));
        }
        let material = |text: String| {
            lash_core::Material::journal_local(
                MaterialOwner::Process {
                    process_id: run.process.clone(),
                },
                MaterialRole::AttemptOutput,
                text,
            )
        };
        match self.then {
            Then::Complete => {
                lash_core::SettledOutput::Completed(material("\"capacity\"".to_owned())).into()
            }
            Then::Fail => {
                let output = lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
                    lash_core::ToolFailureClass::Internal,
                    "saturating_failed",
                    "the saturating body failed on its own",
                ));
                lash_core::SettledOutput::Failed(
                    material(serde_json::to_string(&output).expect("a tool output encodes"))
                        .failure(KnownFailureReason::Reported, None),
                )
                .into()
            }
        }
    }
}

fn saturating_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { .. } => lash_core::EngineAction::Steps {
            steps: vec![lash_core::StepRequest::Engine {
                step: lash_core::StepName(SATURATING.to_owned()),
                kind: lash_core::EngineStepKind::new(SATURATING),
                input: serde_json::Value::Null,
            }],
            wake: None,
        },
        lash_core::EngineEvent::StepSettled { outcome, .. } => {
            answer(serde_json::json!({ "step": settled(&outcome) }))
        }
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// A plugin that contributes [`SATURATING_ENGINE`] with its body.
struct SaturatingPlugin {
    runs: Arc<AtomicUsize>,
    verdicts: Arc<AtomicUsize>,
    then: Then,
}

struct NoSaturatingSessionPlugin;

impl lash_core::plugin::SessionPlugin for NoSaturatingSessionPlugin {
    fn id(&self) -> &'static str {
        "core-node-saturating-plugin"
    }
}

impl lash_core::plugin::PluginFactory for SaturatingPlugin {
    fn id(&self) -> &'static str {
        "core-node-saturating-plugin"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash_core::plugin::PluginRecipe, lash_core::plugin::PluginError> {
        let reg = lash_core::plugin::PluginRegistrar::<NoSaturatingSessionPlugin>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        let factory = Arc::clone(&self);
        Ok(
            lash_core::plugin::PluginRecipe::new(self.id(), reg, |_ctx, _state| {
                Ok(Arc::new(NoSaturatingSessionPlugin))
            })
            .with_process_engine(SATURATING_ENGINE, move |_context| {
                Ok(
                    lash_core::ProcessEngineRegistration::accepting(Arc::new(ScriptEngine {
                        kind: SATURATING_ENGINE,
                        advance: saturating_advance,
                        performs: no_tools,
                    }))
                    .with_engine_steps(Arc::new(Saturating {
                        runs: Arc::clone(&factory.runs),
                        verdicts: Arc::clone(&factory.verdicts),
                        then: factory.then,
                    })),
                )
            }),
        )
    }
}

impl lash_core::plugin::PluginMetadata for SaturatingPlugin {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("core-node-saturating-plugin")
    }
}

/// A deployment of the saturating engine whose body answers `verdicts`
/// capacity verdicts, then `then`, under `settings`; and its run count.
async fn saturating_deployment(
    tier: Tier,
    settings: lash_core_execution::DurableSettings,
    verdicts: usize,
    then: Then,
) -> (Deployment, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let verdicts = Arc::new(AtomicUsize::new(verdicts));
    let plugin = Arc::new(SaturatingPlugin {
        runs: Arc::clone(&runs),
        verdicts: Arc::clone(&verdicts),
        then,
    });
    let deployment = deploy_configured(tier, settings, Vec::new(), |backend| {
        lash::LashCore::standard_builder(backend.clone()).plugin(plugin)
    })
    .await;
    (deployment, runs, verdicts)
}

/// The kinds of `process`'s run facts, in order.
async fn fact_kinds(
    deployment: &Deployment,
    process: &lash_core::ProcessId,
) -> Vec<lash_durable::domain::RunFactKind> {
    deployment
        .backend
        .durable()
        .run_facts(&lash_durable::domain::OwnerKey::Process(process.clone()))
        .await
        .expect("the process's run facts are read")
        .rows
        .iter()
        .map(|row| row.kind)
        .collect()
}

/// A step the host lacked capacity for runs again at its ordinal, uncounted:
/// two verdicts and then its answer leave one start, no retry and one
/// outcome, and the process completes with the answer.
async fn a_capacity_verdict_reruns_its_step_at_the_same_ordinal(tier: Tier) {
    use lash_durable::domain::RunFactKind;
    let (deployment, runs, _) = saturating_deployment(
        tier,
        lash_core_execution::DurableSettings::default(),
        2,
        Then::Complete,
    )
    .await;
    let process = start(&deployment.core, SATURATING_ENGINE, serde_json::Value::Null).await;
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "step": "capacity" }));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        3,
        "two verdicts, then its answer"
    );
    assert_eq!(
        fact_kinds(&deployment, &process).await,
        [
            RunFactKind::Admit,
            RunFactKind::XStart,
            RunFactKind::XOutcome
        ],
        "the reruns started nothing new and recorded no retry"
    );
}

on_every_tier!(a_capacity_verdict_reruns_its_step_at_the_same_ordinal);

/// A failure of the step's own is recorded and spends its `Repeatable`
/// attempts, which the verdicts before it did not: two verdicts, then three
/// failed attempts, the last of which the process ends with.
async fn a_failure_after_capacity_verdicts_still_spends_its_attempts(tier: Tier) {
    use lash_durable::domain::RunFactKind;
    let (deployment, runs, _) = saturating_deployment(
        tier,
        lash_core_execution::DurableSettings::default(),
        2,
        Then::Fail,
    )
    .await;
    let process = start(&deployment.core, SATURATING_ENGINE, serde_json::Value::Null).await;
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(
        answer["step"]["code"], "saturating_failed",
        "the process ends with the recorded failure: {answer}"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        5,
        "two uncounted verdicts and three counted attempts"
    );
    let kinds = fact_kinds(&deployment, &process).await;
    let count = |kind| kinds.iter().filter(|seen| **seen == kind).count();
    assert_eq!(
        (
            count(RunFactKind::XStart),
            count(RunFactKind::Retry),
            count(RunFactKind::XOutcome)
        ),
        (3, 2, 1),
        "each failed attempt is recorded: {kinds:?}"
    );
}

on_every_tier!(a_failure_after_capacity_verdicts_still_spends_its_attempts);

/// A host that stays saturated parks the process through the
/// activation-loop budget, never ending it; once capacity frees, a redrive
/// runs the step again and the process completes.
async fn persistent_saturation_parks_and_a_redrive_completes(tier: Tier) {
    let settings = lash_core_execution::DurableSettings {
        activation_loop_budget: 3,
        ..lash_core_execution::DurableSettings::default()
    };
    let (deployment, runs, verdicts) =
        saturating_deployment(tier, settings, usize::MAX, Then::Complete).await;
    let process = start(&deployment.core, SATURATING_ENGINE, serde_json::Value::Null).await;
    let (reason, observed) = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let observed = deployment
                .core
                .processes()
                .get(&process)
                .await
                .expect("the process is read")
                .expect("the process exists");
            if let lash_core::ProcessParkState::Parked(reason) = observed.park.clone() {
                break (reason, observed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the process parks within a minute");
    assert!(
        matches!(reason, lash_core::ProcessParkReason::ActivationLoop { .. }),
        "the process parked through the activation-loop budget: {reason:?}"
    );
    assert!(
        !matches!(
            observed.lifecycle,
            lash_core::ProcessLifecycleState::Terminal { .. }
        ),
        "a capacity verdict never ends the process: {observed:?}"
    );
    let parked_runs = runs.load(Ordering::SeqCst);
    assert!(
        parked_runs >= 3,
        "the step ran on each claim: {parked_runs}"
    );
    verdicts.store(0, Ordering::SeqCst);
    assert!(
        deployment
            .core
            .processes()
            .redrive(&process, "capacity law")
            .await
            .expect("the redrive is accepted"),
        "the process was parked"
    );
    let answer = success(&ended(&deployment.core, &process).await);
    assert_eq!(answer, serde_json::json!({ "step": "capacity" }));
    assert_eq!(runs.load(Ordering::SeqCst), parked_runs + 1);
}

on_every_tier!(persistent_saturation_parks_and_a_redrive_completes);

/// A kernel run whose worker pool is full waits for capacity: while the
/// pool's one worker is held and its queue admits nothing, the run meets the
/// pool's refusal on each claim and the process stays live; once the worker
/// is free, the process runs to its answer. Before FIG-6155 the refusal
/// spent the run's three immediate attempts and ended the process with
/// `kernel_run_fault`.
async fn a_kernel_run_waits_for_a_full_worker_pool(tier: Tier) {
    use lash_vm_client::WorkerPoolRuntimeOps as _;
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.min_workers = 1;
    config.max_workers = 1;
    config.max_queue_items = 0;
    let workers = lash::vm::WorkerService::new(config);
    let held = workers
        .pool()
        .expect("the pool starts")
        .checkout(
            0,
            lash_vm_client::OwnerEpoch(0),
            lash_vm_client::FrameEpoch(0),
            lash_vm_client::ExecutionBudget::default(),
        )
        .expect("the law holds the pool's one worker");
    let service = workers.clone();
    let deployment = deploy_with(tier, Vec::new(), move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash::rlm::RlmChannel::Cell)
                    .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                    .build(),
                lash::rlm::CellDialect::typescript(),
            )
            .with_worker_service(service),
        )
    })
    .await;
    let payload = worker_payload(&deployment.backend).await;
    let process = start(
        &deployment.core,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        payload,
    )
    .await;
    let actor = lash_durable::ActorKey::process(process.as_str()).expect("a process actor key");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let observed = deployment
                .core
                .processes()
                .get(&process)
                .await
                .expect("the process is read")
                .expect("the process exists");
            assert!(
                !matches!(
                    observed.lifecycle,
                    lash_core::ProcessLifecycleState::Terminal { .. }
                ),
                "a full worker pool ended the process: {observed:?}"
            );
            let snapshot = deployment
                .backend
                .durable()
                .actor(&actor)
                .await
                .expect("the actor is read")
                .expect("the actor exists");
            if snapshot.failed_activations >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run meets the full pool on a later claim within a minute");
    held.release().expect("the held worker returns to the pool");
    let output = ended(&deployment.core, &process).await;
    assert_eq!(success(&output), serde_json::json!("done"));
}

on_every_tier!(a_kernel_run_waits_for_a_full_worker_pool);
