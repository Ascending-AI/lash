//! Real-worker allocation failures recovered by the Restate journal test host.

use super::*;
use lash_core_execution::{
    EffectAddress, EffectOpener, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    ScopedEffectController, SessionId, ToolCallId, TurnId,
};
use lash_vm_broker::testing::MemoryCheckpoints;
use lash_vm_broker::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    ComputeBeforeEffectDispatch,
    RecordedEffectBeforeDelivery,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::ComputeBeforeEffectDispatch => "compute_before_effect_dispatch",
            Self::RecordedEffectBeforeDelivery => "recorded_effect_before_delivery",
        }
    }

    fn mode(self) -> &'static str {
        match self {
            Self::ComputeBeforeEffectDispatch => "native_oom_compute",
            Self::RecordedEffectBeforeDelivery => "native_oom_recorded",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::ComputeBeforeEffectDispatch => {
                "let n = 0; while (n < 200000) { n++; } finish(n);"
            }
            Self::RecordedEffectBeforeDelivery => "finish(await tools.echo({ value: 7 }));",
        }
    }
}

#[derive(Default)]
struct Probe {
    dispatches: Mutex<Vec<(ToolCallId, String)>>,
    performed: Mutex<Vec<(ToolCallId, String)>>,
    checkpoints: Mutex<Vec<u64>>,
    crashed: Mutex<Option<BrokerFailure>>,
    completed: Mutex<Option<BrokeredEnd>>,
    pids: Mutex<Vec<u32>>,
}

struct JournalEffects<'a> {
    scoped: ScopedEffectController<'a>,
    identities: CodeCallIdentities,
    probe: Arc<Probe>,
}

impl JournalEffects<'_> {
    async fn recorded(
        &self,
        key: String,
        call_id: ToolCallId,
        args: serde_json::Value,
        body: impl FnOnce() -> serde_json::Value + Send + 'static,
    ) -> Result<serde_json::Value, ParentFault> {
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(self.scoped.execution_scope().clone(), key)
                    .expect("journal address"),
                RuntimeAttribution::none(),
                "native_oom_law",
            ),
            RuntimeEffectCommand::ToolAttempt {
                call: Box::new(lash_core_execution::PreparedToolCall {
                    call_id: call_id.clone(),
                    provider_call_id: None,
                    tool_id: "native_oom_law".into(),
                    tool_name: "native_oom_law".into(),
                    args: args.clone(),
                    replay: None,
                    prepared_payload: args.clone(),
                }),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        );
        let outcome = self
            .scoped
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core_execution::ToolAttemptLaunch::Done {
                            record: Box::new(lash_core_execution::ToolCallRecord {
                                call_id,
                                provider_call_id: None,
                                tool: "native_oom_law".into(),
                                args,
                                output: lash_core_execution::ToolCallOutput::success(body()),
                            }),
                            intents: lash_core_execution::ToolIntents::default(),
                        }),
                        triggers: Vec::new(),
                        capture: None,
                    })
                }),
            )
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
            panic!("journal returned an unexpected outcome");
        };
        let lash_core_execution::ToolAttemptLaunch::Done { record, .. } = *launch else {
            panic!("journaled operation completed");
        };
        Ok(record.output.value_for_projection())
    }
}

#[async_trait::async_trait]
impl ParentEffects for JournalEffects<'_> {
    async fn retain(
        &self,
        operation: &AdmittedOperation,
    ) -> Result<RequestFingerprint, ParentFault> {
        let fingerprint = operation.fingerprint;
        let command = self.identities.call_id(operation.ordinal);
        let recorded = self
            .recorded(
                format!("native-oom:{command}:retain"),
                command,
                serde_json::json!({"ordinal": operation.ordinal}),
                move || serde_json::json!(fingerprint),
            )
            .await?;
        serde_json::from_value(recorded).map_err(|error| ParentFault(error.to_string()))
    }

    async fn perform(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault> {
        let value = match &operation.kind {
            AdmittedKind::Invoke(call) => {
                let id = call.call_id.clone();
                let key = format!("native-oom:{id}");
                let probe = self.probe.clone();
                let dispatch = (id.clone(), key.clone());
                let value = self
                    .recorded(
                        key.clone(),
                        id.clone(),
                        call.call.arguments.clone(),
                        move || {
                            probe.dispatches.lock().expect("dispatches").push(dispatch);
                            serde_json::json!({"value": 7, "dispatch": 1})
                        },
                    )
                    .await?;
                self.probe
                    .performed
                    .lock()
                    .expect("performed")
                    .push((id, key));
                lashlang::from_json(value)
            }
            AdmittedKind::Control { payload, .. } => {
                let OperationRequest::Finish(value) =
                    OperationRequest::decode(payload).expect("finish")
                else {
                    panic!("the only control effect is finish");
                };
                value
            }
            other => panic!("unexpected operation {other:?}"),
        };
        Ok(Performed::outcome(EffectOutcome::Value(EncodedPayload(
            rmp_serde::to_vec_named(&AbilityOutcome::Value(value)).expect("worker result"),
        ))))
    }

    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
        assert!(checkpoint > 0, "guest instructions have executed");
        self.probe
            .checkpoints
            .lock()
            .expect("checkpoints")
            .push(checkpoint);
        let recorded = self
            .recorded(
                format!("native-oom:checkpoint:{checkpoint}"),
                ToolCallId::fixture(&format!("native-oom-checkpoint-{checkpoint}")),
                serde_json::json!({"checkpoint": checkpoint}),
                || serde_json::json!(false),
            )
            .await?;
        Ok(recorded.as_bool().expect("recorded cancellation"))
    }
}

struct Scenario {
    phase: Phase,
    pool: WorkerPool,
    budget: ExecutionBudget,
    witness: PathBuf,
    probe: Arc<Probe>,
    parent_pid: u32,
}

impl Scenario {
    #[expect(
        clippy::disallowed_methods,
        reason = "law checks the child's retained native allocation-failure evidence"
    )]
    fn attempt(self: &Arc<Self>, crashing: bool) -> lash_restate_test::HandlerAttempt {
        let scenario = self.clone();
        Arc::new(move |scoped| {
            let scenario = scenario.clone();
            Box::pin(async move {
                let identities = CodeCallIdentities::cell(
                    EffectOpener::for_scope(scoped.admitted_scope()).expect("opener"),
                    "native-oom-cell",
                );
                let context = AdmittedContext {
                    owner: VmOwner::new("native-oom"),
                    owner_epoch: OwnerEpoch(1),
                    identities: identities.clone(),
                    bindings: Arc::new(FrozenBindings::new().bind(
                        "tools",
                        "echo",
                        BoundOperation {
                            tool: ToolRoute {
                                tool_id: "echo".into(),
                                tool_name: "echo".into(),
                            },
                            arguments: ArgumentContract::Any,
                        },
                    )),
                };
                let effects = JournalEffects {
                    scoped,
                    identities,
                    probe: scenario.probe.clone(),
                };
                let checkpoints = MemoryCheckpoints::default();
                checkpoints
                    .open_frame(FrameEpoch(1))
                    .await
                    .expect("open frame");
                let slots = PoolSlots {
                    pool: scenario.pool.clone(),
                    owner_epoch: OwnerEpoch(1),
                    frame_epoch: FrameEpoch(1),
                    budget: scenario.budget.clone(),
                    recovery: None,
                };
                let broker = Broker {
                    context: &context,
                    effects: &effects,
                    checkpoints: &checkpoints,
                    slots: &slots,
                    codec: FrameCodec::new(scenario.pool.config().protocol.decode),
                    contract: lashlang::vm_contract_reads(),
                    bounds: BrokerBounds::standard(),
                    frames: FrameFence::new(FrameEpoch(1)),
                };
                let input = start(scenario.phase.source(), ExecutionMode::Foreground);
                let result = broker
                    .run(
                        RunStart {
                            program: input.program,
                            contexts: input.contexts,
                            limits: input.limits,
                            from: None,
                        },
                        &CancellationToken::new(),
                    )
                    .await;
                assert_eq!(
                    std::process::id(),
                    scenario.parent_pid,
                    "the parent survives"
                );
                if crashing {
                    let Err(failure) = result else {
                        panic!("native allocation failure must crash the worker before completion");
                    };
                    assert!(
                        matches!(
                            failure,
                            BrokerFailure::WorkerLost {
                                outcome: InfrastructureOutcome::WorkerCrashed { .. },
                                ..
                            }
                        ),
                        "supervisor crash, separate from GuestError: {failure:?}"
                    );
                    assert!(failure.is_retryable());
                    let witness: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(&scenario.witness)
                            .expect("native allocation failure witness"),
                    )
                    .expect("allocation witness");
                    assert_eq!(witness["phase"], scenario.phase.name());
                    assert_eq!(witness["allocation_failed"], true);
                    assert_eq!(witness["address_space_ceiling_bytes"], 0);
                    assert_eq!(witness["requested_bytes"], 16 * 1024 * 1024);
                    assert_eq!(witness["errno"], libc::ENOMEM);
                    let dead = scenario.probe.pids.lock().expect("pids")[0];
                    assert_eq!(witness["pid"], dead);
                    assert_reaped(dead);
                    let stats = scenario.pool.stats();
                    assert_eq!(
                        (
                            stats.workers,
                            stats.idle,
                            stats.queued_items,
                            stats.queued_bytes
                        ),
                        (0, 0, 0, 0)
                    );
                    let dispatched = scenario.probe.dispatches.lock().expect("dispatches").len();
                    assert_eq!(
                        dispatched,
                        usize::from(scenario.phase == Phase::RecordedEffectBeforeDelivery)
                    );
                    if scenario.phase == Phase::ComputeBeforeEffectDispatch {
                        assert!(
                            !scenario
                                .probe
                                .checkpoints
                                .lock()
                                .expect("checkpoints")
                                .is_empty()
                        );
                    }
                    *scenario.probe.crashed.lock().expect("crashed") = Some(failure);
                    let pool = scenario.pool.clone();
                    let budget = scenario.budget.clone();
                    let replacement = tokio::task::spawn_blocking(move || {
                        let worker = pool
                            .checkout(1024, OwnerEpoch(1), FrameEpoch(1), budget)
                            .expect("replacement");
                        let pid = worker.pid().expect("replacement pid");
                        assert_ne!(pid, dead, "reaped before replacement");
                        worker.release().expect("replacement reset");
                        pid
                    })
                    .await
                    .expect("replacement task");
                    scenario.probe.pids.lock().expect("pids").push(replacement);
                    panic!("native worker allocation failure; the substrate must redrive");
                }
                *scenario.probe.completed.lock().expect("completed") =
                    Some(result.expect("substrate redrive completes"));
            })
        })
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "law emits allocation and replay receipts into the test runner's output directory"
)]
async fn law(phase: Phase) {
    let directory = tempfile::tempdir().expect("fault evidence directory");
    let witness = directory.path().join("allocation.json");
    let mut cfg = config(phase.mode());
    cfg.min_workers = 0;
    cfg.entry
        .args
        .push(witness.to_str().expect("UTF-8 fixture path").into());
    let pool = WorkerPool::new(cfg).expect("pool");
    let worker = checkout(&pool);
    let pid = worker.pid().expect("initial pid");
    worker.release().expect("initial reset");
    let probe = Arc::new(Probe::default());
    probe.pids.lock().expect("pids").push(pid);
    let scenario = Arc::new(Scenario {
        phase,
        pool,
        witness,
        probe: probe.clone(),
        budget: ExecutionBudget::default(),
        parent_pid: std::process::id(),
    });
    let backend = lash_restate_test::backend(4482, lash_restate_test::ServerConfig::default())
        .await
        .expect("journal test host");
    let session = SessionId::fixture(format!("native-oom-{}", phase.name()));
    let turn = TurnId::from("turn");
    tokio::time::timeout(
        Duration::from_secs(30),
        backend.run_crashed_then_redriven(
            lash_core_execution::AdmittedScope::turn(session, turn),
            scenario.attempt(true),
            scenario.attempt(false),
        ),
    )
    .await
    .expect("bounded redrive")
    .expect("journal substrate redrive");
    assert!(probe.crashed.lock().expect("crashed").is_some());
    let end = probe
        .completed
        .lock()
        .expect("completed")
        .clone()
        .expect("completed redrive");
    let BrokeredEnd::Complete { value, .. } = end else {
        panic!("successful redrive: {end:?}");
    };
    let outcome: lashlang::ExecutionOutcome = rmp_serde::from_slice(&value.0).expect("completion");
    let expected = match phase {
        Phase::ComputeBeforeEffectDispatch => serde_json::json!(200000),
        Phase::RecordedEffectBeforeDelivery => serde_json::json!({"value": 7, "dispatch": 1}),
    };
    assert_eq!(
        outcome,
        lashlang::ExecutionOutcome::Finished(lashlang::from_json(expected))
    );
    let dispatches = probe.dispatches.lock().expect("dispatches");
    let performed = probe.performed.lock().expect("performed");
    match phase {
        Phase::ComputeBeforeEffectDispatch => assert!(dispatches.is_empty()),
        Phase::RecordedEffectBeforeDelivery => {
            assert_eq!(dispatches.len(), 1, "recorded effects dispatch once");
            assert_eq!(
                performed.len(),
                2,
                "live and replay each return the recorded result"
            );
            assert_eq!(performed[0], performed[1], "same ToolCallId and replay key");
            assert_eq!(performed[0], dispatches[0]);
        }
    }
    let stats = scenario.pool.stats();
    assert_eq!(
        (
            stats.workers,
            stats.idle,
            stats.queued_items,
            stats.queued_bytes
        ),
        (1, 1, 0, 0)
    );
    assert!(!stats.restart_storm);
    assert_eq!(
        scenario.budget.totals().0,
        2,
        "redrive preserves the consumed attempt"
    );
    let receipt = serde_json::json!({
        "ticket": "FIG-4482", "phase": phase.name(), "allocation": serde_json::from_slice::<serde_json::Value>(&std::fs::read(&scenario.witness).expect("witness")).expect("witness JSON"),
        "parent_pid": scenario.parent_pid, "worker_pids": *probe.pids.lock().expect("pids"),
        "instruction_checkpoints": *probe.checkpoints.lock().expect("checkpoints"),
        "effect_dispatches": dispatches.len(), "effect_returns": performed.len(), "attempts": scenario.budget.totals().0,
        "supervisor_crash": probe.crashed.lock().expect("crashed").as_ref().map(ToString::to_string),
        "reaped_before_replacement": true, "substrate_redrive_completed": true,
    });
    eprintln!("{receipt}");
    if let Some(output) = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR") {
        std::fs::write(
            PathBuf::from(output).join(format!("native-oom-{}.json", phase.name())),
            receipt.to_string(),
        )
        .expect("law receipt");
    }
}

macro_rules! native_oom_laws {
    ($($name:ident => $phase:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() { law(Phase::$phase).await; }
    )*};
}

native_oom_laws! {
    compute_before_effect_dispatch => ComputeBeforeEffectDispatch,
    recorded_effect_before_delivery => RecordedEffectBeforeDelivery,
}
