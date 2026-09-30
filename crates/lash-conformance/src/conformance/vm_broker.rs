//! FIG-4159: the parent brokers every worker effect and recovers a lost
//! worker through the substrate, never by local rewind (ADR 0123).
//!
//! Every law runs model code on the in-process fake worker behind the real
//! protocol types (`lash_vm_broker::testing`), under a real
//! [`Broker`](lash_vm_broker::Broker) whose parent effects journal on the
//! tier's own controller: each admitted call is a tool attempt the tier
//! records and replays under the call's parent-derived `ToolCallId`, each
//! request's retention and each cancel-checkpoint observation a recorded step
//! of its own. A worker is killed at one point of the kill-point matrix; the
//! broker settles what it admitted and fails typed and retryable; the law
//! hands that failure to the tier, which re-drives the invocation over its
//! journal, as it re-drives any failed attempt. Nothing re-runs the model
//! code inside the live invocation.
//!
//! Each kill-point law asserts, across the crash and the re-drive:
//!
//! - no recorded effect re-executes, and no completed effect is lost: every
//!   call's tool body ran exactly once, and the completed run reads each
//!   call's first (recorded) result;
//! - the typed outcome of the crashed attempt, and of the re-drive;
//! - one `ToolCallId` per logical call, derived by the parent from the
//!   call's ordinal (aggregate leaves from their first-appearance index), the
//!   same across the crash and the re-drive.
//!
//! Every execution of an attempt builds its own worker pool, broker and
//! checkpoint store (unless the law carries a checkpoint across invocations
//! on purpose), so a tier that runs an attempt more than once before it ends
//! (the replaying Restate double) replays it to the same point.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use lash_vm_broker::testing::{
    FAKE_STATE_FORMAT, FAKE_VM_CONTRACT, FakeWorkerPool, Fault, MemoryCheckpoints, PoolStats,
    ScriptedProgram, Step,
};
use lash_vm_broker::{
    AdmittedContext, AdmittedKind, AdmittedOperation, ArgumentContract, BoundOperation, Broker,
    BrokerBounds, BrokerFailure, BrokeredEnd, CodeCallIdentities, FrameFence, FrozenBindings,
    Invocation, ParentEffects, ParentFault, Performed, RequestFingerprint, RunStart, StateContract,
    ToolRoute,
};
use lash_vm_protocol::{
    BuildIdentity, DecodeLimits, EffectOutcome, FrameCodec, FrameEpoch, OwnerEpoch, VmLimits,
    VmOwner,
};
use tokio_util::sync::CancellationToken;

use crate::{
    ConformanceTurnAttempt, ConformanceTurnEnd, EffectAddress, ExecutionScope, RuntimeAttribution,
    RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, ScopedEffectController,
};

mod frames;
mod laws;
pub use laws::*;

/// The cell every law's model code runs as, inside its turn.
const CELL: &str = "vm-broker-law";

/// How long a checkout waits for a slot before it is refused: long enough
/// for a healthy nested run, short enough that a deadlock fails the law.
const CHECKOUT_WAIT: Duration = Duration::from_secs(10);

/// How long a law's turn gets before the law fails rather than hangs.
const TURN_BUDGET: Duration = Duration::from_secs(120);

fn codec() -> FrameCodec {
    FrameCodec::new(
        BuildIdentity::new("lash conformance vm-broker law build"),
        DecodeLimits::standard(),
    )
}

fn contract() -> StateContract {
    StateContract {
        vm_contract: FAKE_VM_CONTRACT.into(),
        continuation_format: FAKE_STATE_FORMAT,
        snapshot_format: FAKE_STATE_FORMAT,
    }
}

fn bounds() -> BrokerBounds {
    BrokerBounds {
        settle_deadline: Duration::from_secs(30),
        cancel_grace: Duration::from_millis(200),
        ..BrokerBounds::standard()
    }
}

fn limits() -> VmLimits {
    VmLimits {
        instruction_budget: None,
        memory_limit_bytes: None,
        max_frame_depth: 64,
    }
}

/// The bindings every law's run is admitted with.
fn bindings() -> FrozenBindings {
    let route = |name: &str| ToolRoute {
        tool_id: format!("tool:vm_broker_{name}"),
        tool_name: format!("vm_broker_{name}"),
    };
    FrozenBindings::new()
        .bind(
            "tools",
            "echo",
            BoundOperation {
                tool: route("echo"),
                arguments: ArgumentContract::Object {
                    required: ["value".to_string()].into(),
                    properties: BTreeSet::new(),
                    additional: false,
                },
            },
        )
        .bind(
            "tools",
            "compile",
            BoundOperation {
                tool: route("compile"),
                arguments: ArgumentContract::Object {
                    required: ["source".to_string()].into(),
                    properties: BTreeSet::new(),
                    additional: false,
                },
            },
        )
}

/// `tools.echo({ value })`.
pub(crate) fn echo(value: i64) -> Invocation {
    Invocation {
        binding: "tools".into(),
        operation: "echo".into(),
        arguments: serde_json::json!({ "value": value }),
    }
}

/// Which execution of a law's turn observed something.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// The attempt whose worker the law kills.
    Crashing,
    /// The tier's re-drive of it, or a law's healthy turn.
    Healthy,
}

/// One run of a tool body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolRun {
    pub(crate) call_id: String,
    pub(crate) replay_key: String,
}

/// What every execution of a law's turns observed.
#[derive(Default)]
pub(crate) struct Probe {
    /// Every tool body run, in order: a served record runs none.
    runs: Mutex<Vec<ToolRun>>,
    /// How each phase's last execution ended.
    ends: Mutex<BTreeMap<Phase, Result<BrokeredEnd, BrokerFailure>>>,
    /// Each phase's last execution's pool.
    pools: Mutex<BTreeMap<Phase, PoolStats>>,
    /// Every cancel-checkpoint observation each phase's executions read, the
    /// served ones included.
    observations: Mutex<BTreeMap<Phase, BTreeMap<u64, bool>>>,
    /// The host's cancellation: what a checkpoint observation records when
    /// its journal holds none.
    cancelled: AtomicBool,
    /// How many tool bodies had run when the crashing attempt last ended.
    runs_at_crash: Mutex<Option<usize>>,
}

impl Probe {
    fn ran(&self, run: ToolRun) -> usize {
        let mut runs = self.runs.lock_recover();
        runs.push(run.clone());
        runs.iter()
            .filter(|earlier| earlier.call_id == run.call_id)
            .count()
    }

    pub(crate) fn runs(&self) -> Vec<ToolRun> {
        self.runs.lock_recover().clone()
    }

    pub(crate) fn end(&self, phase: Phase) -> Option<Result<BrokeredEnd, BrokerFailure>> {
        self.ends.lock_recover().get(&phase).cloned()
    }

    pub(crate) fn pool(&self, phase: Phase) -> Option<PoolStats> {
        self.pools.lock_recover().get(&phase).cloned()
    }

    pub(crate) fn runs_at_crash(&self) -> Option<usize> {
        *self.runs_at_crash.lock_recover()
    }

    pub(crate) fn observations(&self, phase: Phase) -> BTreeMap<u64, bool> {
        self.observations
            .lock_recover()
            .get(&phase)
            .cloned()
            .unwrap_or_default()
    }
}

/// How the host stops a run, in the crashing attempt only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostStop {
    /// The host cancels: its durable cancellation is set, and the run is
    /// stopped live.
    Cancel,
    /// Only a live stop, with nothing durable behind it: a process-local
    /// shutdown, a stray `Cancel`.
    Unsolicited,
}

/// How one scenario's runs are shaped.
#[derive(Clone)]
pub(crate) struct Scenario {
    pub(crate) name: String,
    pub(crate) program: ScriptedProgram,
    /// Slots in each execution's pool.
    pub(crate) slots: usize,
    /// The crashing attempt's host stops the run this way right after its
    /// journal observes this checkpoint.
    pub(crate) stop_after_checkpoint: Option<(u64, HostStop)>,
    /// The store the law reads commits back from, across its invocations;
    /// `None` gives every execution a store of its own.
    pub(crate) carried: Option<Arc<MemoryCheckpoints>>,
    /// The committed checkpoint every execution of the scenario's
    /// invocation starts from: what a segment's handover hands it.
    pub(crate) resume: Option<lash_vm_broker::Checkpoint>,
    /// Whose state the run's is; `None` names the law's turn.
    pub(crate) owner: Option<VmOwner>,
    /// Whose calls the run's identities root in: a process body carries its
    /// ordinals across segments, so a law that resumes from a committed
    /// checkpoint roots them in one process.
    pub(crate) identities: Option<CodeCallIdentities>,
    pub(crate) probe: Arc<Probe>,
}

impl Scenario {
    pub(crate) fn new(name: impl Into<String>, program: ScriptedProgram) -> Self {
        Self {
            name: name.into(),
            program,
            slots: 1,
            stop_after_checkpoint: None,
            carried: None,
            resume: None,
            owner: None,
            identities: None,
            probe: Arc::new(Probe::default()),
        }
    }

    /// The identities the run's calls take under `scoped`'s turn.
    pub(crate) fn identities(&self, scoped: &ScopedEffectController<'_>) -> CodeCallIdentities {
        self.identities.clone().unwrap_or_else(|| {
            CodeCallIdentities::cell(
                crate::EffectOpener::for_scope(scoped.admitted_scope()).unwrap_or_else(|error| {
                    panic!("{}: the law's scope opens: {error}", self.name)
                }),
                CELL,
            )
        })
    }

    fn context(&self, scoped: &ScopedEffectController<'_>) -> AdmittedContext {
        AdmittedContext {
            owner: self
                .owner
                .clone()
                .unwrap_or_else(|| VmOwner::new(format!("vm-broker-law:{}", scoped.scope_id()))),
            owner_epoch: OwnerEpoch(0),
            identities: self.identities(scoped),
            bindings: Arc::new(bindings()),
        }
    }

    /// One execution of the scenario's run on `scoped`, with `fault` planned
    /// for its first worker.
    pub(crate) async fn run(
        &self,
        scoped: ScopedEffectController<'_>,
        phase: Phase,
        fault: Option<Fault>,
    ) -> Result<BrokeredEnd, BrokerFailure> {
        let pool = Arc::new(FakeWorkerPool::new(codec(), self.slots, CHECKOUT_WAIT));
        pool.plan(fault);
        let own_checkpoints = MemoryCheckpoints::default();
        let checkpoints = self.carried.as_deref().unwrap_or(&own_checkpoints);
        let stop = CancellationToken::new();
        let context = self.context(&scoped);
        let effects = TierEffects {
            scoped,
            probe: Arc::clone(&self.probe),
            phase,
            pool: Arc::clone(&pool),
            stop_after_checkpoint: self
                .stop_after_checkpoint
                .filter(|_| phase == Phase::Crashing),
            stop: stop.clone(),
        };
        let broker = Broker {
            context: &context,
            effects: &effects,
            checkpoints,
            slots: pool.as_ref(),
            codec: codec(),
            contract: contract(),
            bounds: bounds(),
            frames: FrameFence::new(FrameEpoch(0)),
        };
        let from = self.resume.clone();
        let end = broker
            .run(
                RunStart {
                    program: self.program.source(),
                    contexts: Vec::new(),
                    limits: limits(),
                    from,
                },
                &stop,
            )
            .await;
        self.probe.pools.lock_recover().insert(phase, pool.stats());
        if phase == Phase::Crashing {
            *self.probe.runs_at_crash.lock_recover() = Some(self.probe.runs().len());
        }
        self.probe.ends.lock_recover().insert(phase, end.clone());
        end
    }

    /// The attempt whose worker dies at `fault`. It hands the broker's typed
    /// failure to the tier by failing the attempt, as a product handler's
    /// retryable error does; a crashing attempt that nonetheless ended
    /// records its end for the law to refuse.
    pub(crate) fn crashing(self: &Arc<Self>, fault: Option<Fault>) -> ConformanceTurnAttempt {
        let scenario = Arc::clone(self);
        Arc::new(move |scoped| {
            let scenario = Arc::clone(&scenario);
            let fault = fault.clone();
            Box::pin(async move {
                let end = scenario.run(scoped, Phase::Crashing, fault).await;
                panic!(
                    "{}: the worker was lost ({end:?}); the substrate re-drives the invocation",
                    scenario.name
                )
            })
        })
    }

    /// A healthy attempt: the tier's re-drive, or a law's plain turn.
    pub(crate) fn healthy(self: &Arc<Self>) -> ConformanceTurnAttempt {
        let scenario = Arc::clone(self);
        Arc::new(move |scoped| {
            let scenario = Arc::clone(&scenario);
            Box::pin(async move {
                scenario.run(scoped, Phase::Healthy, None).await.ok();
                ConformanceTurnEnd::Settled
            })
        })
    }
}

/// The law's parent effects: every admitted operation journals on the
/// tier's controller.
struct TierEffects<'run> {
    scoped: ScopedEffectController<'run>,
    probe: Arc<Probe>,
    phase: Phase,
    /// The run's own pool: a nested run checks its worker out of it.
    pool: Arc<FakeWorkerPool>,
    stop_after_checkpoint: Option<(u64, HostStop)>,
    stop: CancellationToken,
}

impl TierEffects<'_> {
    fn envelope(
        &self,
        replay_key: String,
        call_id: lash_sansio::ToolCallId,
        tool: &str,
        args: serde_json::Value,
    ) -> RuntimeEffectEnvelope {
        let scope: ExecutionScope = self.scoped.execution_scope().clone();
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(scope, replay_key.clone())
                    .unwrap_or_else(|error| panic!("address {replay_key}: {error}")),
                RuntimeAttribution::none(),
                tool,
            ),
            RuntimeEffectCommand::ToolAttempt {
                call: Box::new(crate::PreparedToolCall {
                    call_id,
                    provider_call_id: None,
                    tool_id: crate::ToolId::from(format!("tool:{tool}")),
                    tool_name: tool.into(),
                    args: args.clone(),
                    replay: None,
                    prepared_payload: args,
                }),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        )
    }

    /// Runs one journaled step whose body answers `output`, and returns what
    /// the journal answers: the body's output on the first run, the recorded
    /// one on every replay.
    async fn journaled(
        &self,
        replay_key: String,
        call_id: lash_sansio::ToolCallId,
        tool: &str,
        args: serde_json::Value,
        body: impl FnOnce(&RuntimeEffectEnvelope) -> serde_json::Value + Send + 'static,
    ) -> Result<serde_json::Value, ParentFault> {
        let envelope = self.envelope(replay_key, call_id.clone(), tool, args.clone());
        let tool_name = tool.to_string();
        let outcome = self
            .scoped
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |envelope| async move {
                    let output = body(&envelope);
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(crate::ToolAttemptLaunch::Done {
                            record: Box::new(crate::ToolCallRecord {
                                call_id,
                                provider_call_id: None,
                                tool: tool_name,
                                args,
                                output: crate::ToolCallOutput::success(output),
                            }),
                            intents: crate::ToolIntents::default(),
                        }),
                        triggers: Vec::new(),
                        capture: None,
                    })
                }),
            )
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
            return Err(ParentFault(format!("a tool step answered {outcome:?}")));
        };
        let crate::ToolAttemptLaunch::Done { record, .. } = *launch else {
            return Err(ParentFault("a tool step did not complete".into()));
        };
        Ok(record.output.value_for_projection())
    }

    /// One admitted call's tool body, journaled under its call id.
    async fn call(
        &self,
        call_id: &lash_sansio::ToolCallId,
        operation: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, ParentFault> {
        let probe = Arc::clone(&self.probe);
        let id = call_id.to_string();
        self.journaled(
            format!("vm-broker:{call_id}"),
            call_id.clone(),
            &format!("vm_broker_{operation}"),
            arguments.clone(),
            move |envelope| {
                let run = probe.ran(ToolRun {
                    call_id: id.clone(),
                    replay_key: envelope.invocation.effect_replay_key().to_owned(),
                });
                serde_json::json!({ "call": id, "run": run })
            },
        )
        .await
    }

    /// The nested run a `compile` needs: a run of its own on a worker of the
    /// same pool.
    async fn compile(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault> {
        let AdmittedKind::Invoke(call) = &operation.kind else {
            return Err(ParentFault("a compile is one call".into()));
        };
        let nested = AdmittedContext {
            owner: VmOwner::new(format!("vm-broker-law:{}:nested", self.scoped.scope_id())),
            owner_epoch: OwnerEpoch(0),
            identities: CodeCallIdentities::cell(
                crate::EffectOpener::for_scope(self.scoped.admitted_scope())
                    .map_err(|error| ParentFault(error.to_string()))?,
                format!("{CELL}/nested/{}", call.call_id),
            ),
            bindings: Arc::new(bindings()),
        };
        let effects = TierEffects {
            scoped: self.scoped.clone(),
            probe: Arc::clone(&self.probe),
            phase: self.phase,
            pool: Arc::clone(&self.pool),
            stop_after_checkpoint: None,
            stop: CancellationToken::new(),
        };
        let checkpoints = MemoryCheckpoints::default();
        let broker = Broker {
            context: &nested,
            effects: &effects,
            checkpoints: &checkpoints,
            slots: self.pool.as_ref(),
            codec: codec(),
            contract: contract(),
            bounds: bounds(),
            frames: FrameFence::new(FrameEpoch(0)),
        };
        let program = ScriptedProgram::new(vec![Step::Invoke(echo(10))]);
        match broker
            .run(
                RunStart {
                    program: program.source(),
                    contexts: Vec::new(),
                    limits: limits(),
                    from: None,
                },
                &CancellationToken::new(),
            )
            .await
        {
            Ok(BrokeredEnd::Complete { value, .. }) => {
                Ok(Performed::outcome(EffectOutcome::Value(value)))
            }
            other => Err(ParentFault(format!(
                "the nested run did not complete: {other:?}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl ParentEffects for TierEffects<'_> {
    async fn retain(
        &self,
        operation: &AdmittedOperation,
    ) -> Result<RequestFingerprint, ParentFault> {
        let fingerprint = operation.fingerprint;
        let command = self.scoped_command_id(operation);
        let retained = self
            .journaled(
                format!("vm-broker:{command}:retain"),
                command,
                "vm_broker_retain",
                serde_json::json!({ "ordinal": operation.ordinal }),
                move |_| serde_json::json!({ "fingerprint": fingerprint }),
            )
            .await?;
        serde_json::from_value(retained["fingerprint"].clone())
            .map_err(|error| ParentFault(format!("the retained request is unreadable: {error}")))
    }

    async fn perform(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault> {
        let value = match &operation.kind {
            AdmittedKind::Invoke(call) if call.call.operation == "compile" => {
                return self.compile(operation).await;
            }
            AdmittedKind::Invoke(call) => {
                self.call(&call.call_id, &call.call.operation, &call.call.arguments)
                    .await?
            }
            AdmittedKind::Aggregate(calls) => {
                let mut values = Vec::with_capacity(calls.len());
                for call in calls {
                    values.push(
                        self.call(&call.call_id, &call.call.operation, &call.call.arguments)
                            .await?,
                    );
                }
                serde_json::Value::Array(values)
            }
            AdmittedKind::Await { .. } | AdmittedKind::Sleep { .. } => serde_json::Value::Null,
        };
        Ok(Performed::outcome(EffectOutcome::Value(
            lash_vm_broker::authority::encode_value(&value),
        )))
    }

    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
        let probe = Arc::clone(&self.probe);
        let call_id = lash_sansio::ToolCallId::fixture(&format!(
            "{}-cancel-{checkpoint}",
            self.scoped.scope_id()
        ));
        let observed = self
            .journaled(
                format!("vm-broker:cancel-checkpoint:{checkpoint}"),
                call_id,
                "vm_broker_cancel_checkpoint",
                serde_json::json!({ "checkpoint": checkpoint }),
                move |_| serde_json::json!({ "cancelled": probe.cancelled.load(Ordering::SeqCst) }),
            )
            .await?;
        let cancelled = observed["cancelled"].as_bool().unwrap_or(false);
        self.probe
            .observations
            .lock_recover()
            .entry(self.phase)
            .or_default()
            .insert(checkpoint, cancelled);
        match self.stop_after_checkpoint {
            Some((after, HostStop::Cancel)) if after == checkpoint => {
                self.probe.cancelled.store(true, Ordering::SeqCst);
                self.stop.cancel();
            }
            Some((after, HostStop::Unsolicited)) if after == checkpoint => self.stop.cancel(),
            _ => {}
        }
        Ok(cancelled)
    }

    fn needs_worker(&self, operation: &AdmittedOperation) -> bool {
        matches!(&operation.kind, AdmittedKind::Invoke(call) if call.call.operation == "compile")
    }
}

impl TierEffects<'_> {
    /// The id the admission record of an operation's command journals under:
    /// the record is the parent's, not a call of the program's.
    fn scoped_command_id(&self, operation: &AdmittedOperation) -> lash_sansio::ToolCallId {
        lash_sansio::ToolCallId::fixture(&format!(
            "{}-retain-{}",
            self.scoped.scope_id(),
            operation.ordinal
        ))
    }
}

/// Runs `law` within the turn budget.
pub(crate) async fn within_budget(name: &str, law: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(TURN_BUDGET, law)
        .await
        .unwrap_or_else(|_| panic!("{name}: the law finishes within {TURN_BUDGET:?}"));
}
