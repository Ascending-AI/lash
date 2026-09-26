//! FIG-3547: a segment re-drive never re-executes a recorded effect, and a
//! segment whose engine lost its record ends `Abandoned(SubstrateLost)`.
//!
//! The law runs one process segment per scenario on the tier's engine, through
//! the engine-neutral effect interface only: the segment's body issues its
//! effects on the process-scoped controller the engine lends it, and the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) starts the segment,
//! kills its execution where the law's crash fires, and recovers it. Every
//! effect's executor counts its runs, records the identity it ran under and
//! answers an outcome naming the run that produced it, so a served record is
//! told from a fresh run.
//!
//! Four effect kinds, each crashed two ways:
//!
//! - a scalar tool call;
//! - a tool batch of two calls, partially recorded when the crash lands;
//! - a trigger registration;
//! - a child process start.
//!
//! **Recorded** (case 1): the effect's result was recorded — the segment
//! observed it — and then the execution dies. **Unrecorded** (case 2): the
//! effect ran, and the execution dies before the segment observed a result.
//! Each case asserts that precondition at the crash.
//!
//! Each crash is recovered two ways ([`SegmentRecovery`](crate::SegmentRecovery)):
//!
//! - **Replay**: the engine delivers the execution again over its surviving
//!   record. A recorded effect never runs again, and the re-drive observes the
//!   recorded result. An unrecorded effect runs exactly once more, under the
//!   identity its first run had: the same call id and replay key, which is the
//!   idempotency key an effect implementor relies on. The process completes.
//! - **Substrate lost**: the engine's record of the execution is gone, and a
//!   fresh execution of the segment arrives. A started segment is never re-run
//!   from scratch: the process ends `Abandoned` with
//!   `ResumeRefused { SubstrateLost }`, the segment body never runs again, and
//!   no effect runs again.
//!
//! The expectations are the final recovery rules (FIG-3588): the
//! registration's recovery contract grants no re-run, so the law registers a
//! process that would once have been re-runnable and still expects the
//! refusal.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::ProcessId;
use lash_sansio::sync::MutexExt;
use pretty_assertions::assert_eq;

use crate::{
    ConformanceCrash, ConformanceTurnAttempt, ConformanceTurnEnd, EffectAddress, ExecutionScope,
    ProcessAwaitOutput, ProcessRegistration, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, ScopedEffectController, SegmentRecovery,
};

/// How long the law waits for a child it started to settle.
const CHILD_SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The effect a scenario crashes around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectKind {
    ToolCall,
    ToolBatch,
    Trigger,
    ChildStart,
}

impl EffectKind {
    const ALL: [Self; 4] = [
        Self::ToolCall,
        Self::ToolBatch,
        Self::Trigger,
        Self::ChildStart,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::ToolCall => "tool-call",
            Self::ToolBatch => "tool-batch",
            Self::Trigger => "trigger",
            Self::ChildStart => "child-start",
        }
    }

    /// The effects the segment issues, in order.
    fn effects(self) -> &'static [&'static str] {
        match self {
            Self::ToolCall => &[TOOL],
            Self::ToolBatch => &[BATCH_FIRST, BATCH_SECOND],
            Self::Trigger => &[TRIGGER],
            Self::ChildStart => &[CHILD],
        }
    }
}

const TOOL: &str = "tool";
const BATCH_FIRST: &str = "batch-0";
const BATCH_SECOND: &str = "batch-1";
const TRIGGER: &str = "trigger";
const CHILD: &str = "child";

/// Whether the crashed effect's result was recorded before the crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrashCase {
    /// The segment observed the effect's result, then its execution died.
    Recorded,
    /// The effect ran, and its execution died before the segment observed a
    /// result.
    Unrecorded,
}

impl CrashCase {
    const ALL: [Self; 2] = [Self::Recorded, Self::Unrecorded];

    fn label(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Unrecorded => "unrecorded",
        }
    }
}

fn recovery_label(recovery: SegmentRecovery) -> &'static str {
    match recovery {
        SegmentRecovery::Replay => "replay",
        SegmentRecovery::SubstrateLost => "substrate-lost",
    }
}

/// One run of an effect's executor: the identity it ran under.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    call_id: String,
    replay_key: String,
}

/// Which execution of the segment observed something.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    /// The execution the law crashes.
    Crashing,
    /// Any execution after the crash.
    Recovery,
}

/// The result an execution observed for an effect: the run that produced it,
/// or the error the effect answered.
type Observed = Result<usize, String>;

/// What one scenario observed, across every execution of its segment and of
/// its child.
#[derive(Default)]
struct Probe {
    /// Every run of each effect's executor, in order.
    runs: Mutex<BTreeMap<&'static str, Vec<Run>>>,
    /// The result each phase observed for each effect: the run that produced
    /// it, or the error the effect answered.
    observed: Mutex<BTreeMap<(Phase, &'static str), Observed>>,
    /// Executions of the segment body after the crash.
    recovery_bodies: AtomicUsize,
}

impl Probe {
    /// Records one run of `effect` and returns its 1-based ordinal.
    fn ran(&self, effect: &'static str, run: Run) -> usize {
        let mut runs = self.runs.lock_recover();
        let effect_runs = runs.entry(effect).or_default();
        effect_runs.push(run);
        effect_runs.len()
    }

    fn runs(&self, effect: &'static str) -> Vec<Run> {
        self.runs
            .lock_recover()
            .get(effect)
            .cloned()
            .unwrap_or_default()
    }

    fn observe(&self, phase: Phase, effect: &'static str, result: Observed) {
        self.observed.lock_recover().insert((phase, effect), result);
    }

    fn observed(&self, phase: Phase, effect: &'static str) -> Option<Observed> {
        self.observed.lock_recover().get(&(phase, effect)).cloned()
    }
}

/// One scenario's fixed inputs, shared by every execution of its segment.
#[derive(Clone)]
struct Scenario {
    kind: EffectKind,
    case: CrashCase,
    process_id: ProcessId,
    /// The key the segment starts its child under: the start mints the
    /// child's id, so the law names the child by its key (ADR 0107).
    child_start_key: crate::StartKey,
    /// The id the child's start minted, once the law has seen it.
    child_id: Arc<Mutex<Option<ProcessId>>>,
    registry: Arc<dyn crate::ProcessRegistry>,
    probe: Arc<Probe>,
}

impl Scenario {
    fn scope(&self) -> ExecutionScope {
        ExecutionScope::process(&self.process_id)
    }

    fn replay_key(&self, effect: &str) -> String {
        format!("segment-redrive:{}:{effect}", self.process_id)
    }

    fn call_id(&self, effect: &str) -> String {
        format!("{}-{effect}-call", self.process_id)
    }

    fn invocation(&self, effect: &str) -> RuntimeEffectInvocation {
        let replay_key = self.replay_key(effect);
        RuntimeEffectInvocation::new(
            EffectAddress::new(self.scope(), replay_key.clone())
                .unwrap_or_else(|error| panic!("address {replay_key}: {error}")),
            RuntimeAttribution::none(),
            effect,
        )
    }

    fn tool_envelope(&self, effect: &str) -> RuntimeEffectEnvelope {
        let call_id = self.call_id(effect);
        RuntimeEffectEnvelope::new(
            self.invocation(effect),
            RuntimeEffectCommand::ToolAttempt {
                call: crate::PreparedToolCall::from_parts(
                    call_id.clone(),
                    crate::ToolId::from("tool:segment_redrive_effect"),
                    "segment_redrive_effect",
                    serde_json::json!({ "call": call_id }),
                    None,
                    serde_json::json!({ "prepared": effect }),
                ),
                execution_grant: None,
                attempt: 1,
                max_attempts: 1,
            },
        )
    }

    fn trigger_envelope(&self) -> RuntimeEffectEnvelope {
        let owner_scope = crate::TriggerOwnerScope::session(format!("{}-owner", self.process_id));
        RuntimeEffectEnvelope::new(
            self.invocation(TRIGGER),
            RuntimeEffectCommand::Trigger {
                command: Box::new(crate::TriggerCommand::Register {
                    owner_scope,
                    actor: crate::ProcessOriginator::host_scoped("segment-redrive-law"),
                    draft: crate::TriggerSubscriptionDraft::for_process(
                        self.call_id(TRIGGER),
                        crate::ProcessExecutionEnvRef::new("process-env:segment-redrive"),
                        "segment-redrive.source",
                        "segment-redrive-source-key",
                        crate::ProcessInput::External {
                            metadata: serde_json::json!({ "law": "segment-redrive" }),
                        },
                        crate::ProcessIdentity::new("segment-redrive"),
                    ),
                }),
            },
        )
    }

    fn child_registration(&self) -> ProcessRegistration {
        segment_registration().with_start_key(Some(self.child_start_key.clone()))
    }

    /// Records the id the child's start minted, and refuses a second one.
    fn saw_child(&self, process_id: &ProcessId) {
        let mut child_id = self.child_id.lock_recover();
        match child_id.as_ref() {
            Some(seen) => assert_eq!(
                seen, process_id,
                "every execution names the one child the start minted"
            ),
            None => *child_id = Some(process_id.clone()),
        }
    }

    fn child_id(&self) -> Option<ProcessId> {
        self.child_id.lock_recover().clone()
    }

    fn start_envelope(&self) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            self.invocation(CHILD),
            RuntimeEffectCommand::Process {
                command: Box::new(crate::ProcessCommand::Start {
                    registration: self.child_registration(),
                    observers: Vec::new(),
                    env_spec: None,
                    execution_context: Box::default(),
                }),
            },
        )
    }

    /// The executor of a tool call: it counts its run, and on the crashing
    /// execution of an unrecorded scenario it fires the crash where it
    /// stands, after the tool ran and before it answers.
    fn tool_executor(
        &self,
        effect: &'static str,
        crash_after_running: Option<ConformanceCrash>,
    ) -> RuntimeEffectLocalExecutor<'static> {
        let probe = Arc::clone(&self.probe);
        RuntimeEffectLocalExecutor::testing(move |envelope| async move {
            let RuntimeEffectCommand::ToolAttempt { call, .. } = &envelope.command else {
                panic!("the tool executor runs a tool attempt: {envelope:?}");
            };
            let run = Run {
                call_id: call.call_id.clone(),
                replay_key: envelope.invocation.replay_key().to_owned(),
            };
            let ordinal = probe.ran(effect, run.clone());
            if let Some(crash) = crash_after_running {
                crash.fire();
                std::future::pending::<()>().await;
            }
            Ok(RuntimeEffectOutcome::ToolAttempt {
                launch: Box::new(crate::ToolAttemptLaunch::Done {
                    record: Box::new(crate::ToolCallRecord {
                        call_id: Some(run.call_id.clone()),
                        tool: "segment_redrive_effect".to_string(),
                        args: serde_json::json!({ "call": run.call_id }),
                        output: crate::ToolCallOutput::success(serde_json::json!({
                            "run": ordinal,
                        })),
                    }),
                    intents: crate::ToolIntents::default(),
                }),
                triggers: Vec::new(),
                capture: None,
            })
        })
    }

    /// The executor of the trigger registration, shaped like
    /// [`tool_executor`](Self::tool_executor).
    fn trigger_executor(
        &self,
        crash_after_running: Option<ConformanceCrash>,
    ) -> RuntimeEffectLocalExecutor<'static> {
        let probe = Arc::clone(&self.probe);
        let call_id = self.call_id(TRIGGER);
        RuntimeEffectLocalExecutor::testing(move |envelope| async move {
            let ordinal = probe.ran(
                TRIGGER,
                Run {
                    call_id,
                    replay_key: envelope.invocation.replay_key().to_owned(),
                },
            );
            if let Some(crash) = crash_after_running {
                crash.fire();
                std::future::pending::<()>().await;
            }
            // A typed answer that names its run: the registration's outcome is
            // what a replay must serve back, whatever it says.
            Ok(RuntimeEffectOutcome::Trigger {
                result: Box::new(Err(crate::TriggerOperationError::Invalid {
                    message: format!("{TRIGGER_RUN_PREFIX}{ordinal}"),
                })),
            })
        })
    }

    /// The child's segment body runs a journaled effect under the child's
    /// scope, then settles the child. Restate may re-enter this body after a
    /// suspension; only the effect executor counts as another run.
    fn child_body(&self) -> ConformanceTurnAttempt {
        let scenario = self.clone();
        Arc::new(move |scoped: ScopedEffectController<'_>| {
            let scenario = scenario.clone();
            Box::pin(async move {
                let scope = scoped.execution_scope().clone();
                if let ExecutionScope::Process { process_id } = &scope {
                    scenario.saw_child(process_id);
                }
                let call_id = scope.id().to_owned();
                let replay_key = format!("segment-redrive:{call_id}:child-body");
                let envelope = RuntimeEffectEnvelope::new(
                    RuntimeEffectInvocation::new(
                        EffectAddress::new(scope, replay_key.clone())
                            .unwrap_or_else(|error| panic!("address {replay_key}: {error}")),
                        RuntimeAttribution::none(),
                        CHILD,
                    ),
                    RuntimeEffectCommand::ToolAttempt {
                        call: crate::PreparedToolCall::from_parts(
                            call_id.clone(),
                            crate::ToolId::from("tool:segment_redrive_effect"),
                            "segment_redrive_effect",
                            serde_json::json!({ "call": call_id }),
                            None,
                            serde_json::json!({ "prepared": CHILD }),
                        ),
                        execution_grant: None,
                        attempt: 1,
                        max_attempts: 1,
                    },
                );
                let outcome = scoped
                    .execute_effect(envelope, scenario.tool_executor(CHILD, None))
                    .await
                    .unwrap_or_else(|error| {
                        panic!("the child's journaled effect completes: {error}")
                    });
                assert_eq!(
                    tool_run(&outcome),
                    1,
                    "the child observes its first effect run"
                );
                ConformanceTurnEnd::Settled
            })
        })
    }
}

const TRIGGER_RUN_PREFIX: &str = "segment-redrive trigger run ";

/// The run a tool outcome names.
fn tool_run(outcome: &RuntimeEffectOutcome) -> usize {
    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("a tool attempt answers a tool-attempt outcome: {outcome:?}");
    };
    let crate::ToolAttemptLaunch::Done { record, .. } = launch.as_ref() else {
        panic!("the tool attempt completed: {launch:?}");
    };
    record
        .output
        .value_for_projection()
        .get("run")
        .and_then(serde_json::Value::as_u64)
        .and_then(|run| usize::try_from(run).ok())
        .unwrap_or_else(|| panic!("the tool outcome names its run: {record:?}"))
}

/// The run a trigger outcome names.
fn trigger_run(outcome: &RuntimeEffectOutcome) -> usize {
    let RuntimeEffectOutcome::Trigger { result } = outcome else {
        panic!("a trigger answers a trigger outcome: {outcome:?}");
    };
    match result.as_ref() {
        Err(crate::TriggerOperationError::Invalid { message }) => message
            .strip_prefix(TRIGGER_RUN_PREFIX)
            .and_then(|run| run.parse().ok())
            .unwrap_or_else(|| panic!("the trigger outcome names its run: {message}")),
        other => panic!("the trigger outcome names its run: {other:?}"),
    }
}

/// A process the law runs segments of: re-runnable by its once-granted
/// contract, which the final recovery rules no longer honour.
fn segment_registration() -> ProcessRegistration {
    ProcessRegistration::new(
        crate::ProcessInput::External {
            metadata: serde_json::json!({ "law": "segment-redrive" }),
        },
        crate::RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
}

fn effect_error(result: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>) -> String {
    match result {
        Ok(outcome) => format!("unexpected outcome {outcome:?}"),
        Err(error) => format!("{}: {}", error.code.as_str(), error.message),
    }
}

/// Fires `crash` and never returns: the execution dies here.
async fn die(crash: &ConformanceCrash) -> ConformanceTurnEnd {
    crash.fire();
    std::future::pending().await
}

/// Waits until the child the segment started has run and settled, so the
/// crash lands on a quiet child.
async fn child_settled(scenario: &Scenario) {
    tokio::time::timeout(CHILD_SETTLE_TIMEOUT, async {
        loop {
            // The child's id is known once its start answered or its body ran.
            if let Some(child_id) = scenario.child_id() {
                let record = scenario
                    .registry
                    .get_process(&child_id)
                    .await
                    .unwrap_or_else(|error| panic!("read the child: {error}"));
                if record.is_some_and(|record| record.outcome.is_some()) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the child started under `{}` settled",
            scenario.child_start_key
        )
    });
}

/// One execution of the segment body. The crashing execution dies at the
/// scenario's crash point; every other execution issues the same effects in
/// the same order and settles.
async fn segment_body(
    scenario: Scenario,
    phase: Phase,
    crash: Option<ConformanceCrash>,
    scoped: ScopedEffectController<'_>,
) -> ConformanceTurnEnd {
    if phase == Phase::Recovery {
        scenario
            .probe
            .recovery_bodies
            .fetch_add(1, Ordering::SeqCst);
    }
    let unrecorded_crash = crash
        .clone()
        .filter(|_| scenario.case == CrashCase::Unrecorded);
    match scenario.kind {
        EffectKind::ToolCall => {
            let outcome = scoped
                .execute_effect(
                    scenario.tool_envelope(TOOL),
                    scenario.tool_executor(TOOL, unrecorded_crash),
                )
                .await;
            scenario.probe.observe(
                phase,
                TOOL,
                outcome.as_ref().map(tool_run).map_err(|e| e.to_string()),
            );
        }
        EffectKind::Trigger => {
            let outcome = scoped
                .execute_effect(
                    scenario.trigger_envelope(),
                    scenario.trigger_executor(unrecorded_crash),
                )
                .await;
            scenario.probe.observe(
                phase,
                TRIGGER,
                outcome.as_ref().map(trigger_run).map_err(|e| e.to_string()),
            );
        }
        EffectKind::ToolBatch => {
            // The batch's two calls are independent work: the engine drives
            // them the way it drives any batch, and the crash lands after the
            // first call's result is recorded — before the second call is
            // issued (recorded), or after it ran (unrecorded).
            let first = {
                let scenario = scenario.clone();
                let scoped = &scoped;
                Box::pin(async move {
                    let outcome = scoped
                        .execute_effect(
                            scenario.tool_envelope(BATCH_FIRST),
                            scenario.tool_executor(BATCH_FIRST, None),
                        )
                        .await;
                    scenario.probe.observe(
                        phase,
                        BATCH_FIRST,
                        outcome.as_ref().map(tool_run).map_err(|e| e.to_string()),
                    );
                }) as crate::IndependentEffectWork<'_>
            };
            let second = {
                let scenario = scenario.clone();
                let scoped = &scoped;
                let crash = crash.clone();
                Box::pin(async move {
                    if scenario.case == CrashCase::Recorded
                        && let Some(crash) = &crash
                    {
                        die(crash).await;
                    }
                    let unrecorded_crash = crash.filter(|_| scenario.case == CrashCase::Unrecorded);
                    let outcome = scoped
                        .execute_effect(
                            scenario.tool_envelope(BATCH_SECOND),
                            scenario.tool_executor(BATCH_SECOND, unrecorded_crash),
                        )
                        .await;
                    scenario.probe.observe(
                        phase,
                        BATCH_SECOND,
                        outcome.as_ref().map(tool_run).map_err(|e| e.to_string()),
                    );
                }) as crate::IndependentEffectWork<'_>
            };
            scoped
                .controller()
                .drive_independent_effect_work(vec![first, second])
                .await;
        }
        EffectKind::ChildStart => {
            // The start registers the child and schedules it on the engine.
            // Unrecorded: the execution dies at the storage write that
            // completes the start, after the child was registered and
            // scheduled and the child settled, so the start answers no
            // result. (A transient failure of that write is retried inside a
            // journaled start, so the law kills the execution there instead
            // of failing the write.)
            let registry =
                crate::testing::ProcessRegistryFaults::new(Arc::clone(&scenario.registry));
            if let Some(crash) = unrecorded_crash {
                let settled = scenario.clone();
                registry.hold_next_external_ref_write(async move {
                    child_settled(&settled).await;
                    crash.fire();
                });
            }
            let registry: Arc<dyn crate::ProcessRegistry> = Arc::new(registry);
            let outcome = scoped
                .execute_effect(
                    scenario.start_envelope(),
                    RuntimeEffectLocalExecutor::processes(
                        Arc::clone(&registry),
                        Arc::new(crate::NativeProcessWork::for_registry(registry)),
                    ),
                )
                .await;
            let observed = match &outcome {
                Ok(RuntimeEffectOutcome::Process {
                    result: crate::ProcessEffectOutcome::Start { record },
                }) => {
                    assert_eq!(
                        record.start_key.as_ref(),
                        Some(&scenario.child_start_key),
                        "the start answers the child it was asked for"
                    );
                    scenario.saw_child(&record.id);
                    Ok(1)
                }
                _ => Err(effect_error(outcome)),
            };
            scenario.probe.observe(phase, CHILD, observed);
            if crash.is_some() {
                child_settled(&scenario).await;
            }
        }
    }
    match crash {
        Some(crash) => die(&crash).await,
        None => ConformanceTurnEnd::Settled,
    }
}

fn body(
    scenario: &Scenario,
    phase: Phase,
    crash: Option<ConformanceCrash>,
) -> ConformanceTurnAttempt {
    let scenario = scenario.clone();
    Arc::new(move |scoped: ScopedEffectController<'_>| {
        Box::pin(segment_body(scenario.clone(), phase, crash.clone(), scoped))
    })
}

/// Asserts each case's precondition at the crash and returns every effect's
/// runs at that point.
fn assert_crash_precondition(scenario: &Scenario) -> BTreeMap<&'static str, usize> {
    let probe = &scenario.probe;
    let name = format!("{:?}/{:?}", scenario.kind, scenario.case);
    let (recorded, unrecorded, unissued): (&[&str], &[&str], &[&str]) =
        match (scenario.kind, scenario.case) {
            (EffectKind::ToolCall, CrashCase::Recorded) => (&[TOOL], &[], &[]),
            (EffectKind::ToolCall, CrashCase::Unrecorded) => (&[], &[TOOL], &[]),
            (EffectKind::Trigger, CrashCase::Recorded) => (&[TRIGGER], &[], &[]),
            (EffectKind::Trigger, CrashCase::Unrecorded) => (&[], &[TRIGGER], &[]),
            (EffectKind::ToolBatch, CrashCase::Recorded) => (&[BATCH_FIRST], &[], &[BATCH_SECOND]),
            (EffectKind::ToolBatch, CrashCase::Unrecorded) => {
                (&[BATCH_FIRST], &[BATCH_SECOND], &[])
            }
            (EffectKind::ChildStart, CrashCase::Recorded) => (&[CHILD], &[], &[]),
            (EffectKind::ChildStart, CrashCase::Unrecorded) => (&[], &[CHILD], &[]),
        };
    for effect in recorded {
        assert_eq!(
            probe.runs(effect).len(),
            1,
            "{name}: `{effect}` ran once before the crash"
        );
        assert!(
            matches!(probe.observed(Phase::Crashing, effect), Some(Ok(1))),
            "{name}: the segment observed `{effect}`'s result before the crash: {:?}",
            probe.observed(Phase::Crashing, effect)
        );
    }
    for effect in unrecorded {
        assert_eq!(
            probe.runs(effect).len(),
            1,
            "{name}: `{effect}` ran before the crash"
        );
        assert!(
            !matches!(probe.observed(Phase::Crashing, effect), Some(Ok(_))),
            "{name}: the segment observed no result of `{effect}` before the crash: {:?}",
            probe.observed(Phase::Crashing, effect)
        );
    }
    for effect in unissued {
        assert!(
            probe.runs(effect).is_empty(),
            "{name}: `{effect}` was not issued before the crash"
        );
    }
    scenario
        .kind
        .effects()
        .iter()
        .map(|effect| (*effect, probe.runs(effect).len()))
        .collect()
}

/// The result the recovery observes for `effect`, and how many runs it may
/// add, given what the crash left.
fn expected_after_replay(scenario: &Scenario, effect: &'static str, at_crash: usize) -> usize {
    match (scenario.kind, scenario.case, effect) {
        // The start is keyed by its start key: a re-drive of either case
        // starts no second run of the child.
        (EffectKind::ChildStart, _, _) => at_crash,
        // The first call of the batch was recorded in both cases.
        (EffectKind::ToolBatch, _, BATCH_FIRST) => at_crash,
        (_, CrashCase::Recorded, _) => {
            // The unissued second call runs for the first time.
            if at_crash == 0 { 1 } else { at_crash }
        }
        // The unrecorded effect never answered, so the re-drive runs it once
        // more.
        (_, CrashCase::Unrecorded, _) => at_crash + 1,
    }
}

async fn run_scenario(
    prefix: &str,
    stores: &Arc<dyn crate::StoreSet>,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    kind: EffectKind,
    case: CrashCase,
    recovery: SegmentRecovery,
) {
    let name = format!(
        "{}-{}-{}",
        kind.label(),
        case.label(),
        recovery_label(recovery)
    );
    let registry = stores.process_registry();
    let registration = segment_registration();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .unwrap_or_else(|error| panic!("{name}: register the segment's process: {error}"))
        .id;
    let scenario = Scenario {
        kind,
        case,
        child_start_key: crate::StartKey::for_host(
            crate::StartKeyOwner::HOST,
            format!("{prefix}-{name}-child"),
        ),
        child_id: Arc::new(Mutex::new(None)),
        process_id,
        registry: Arc::clone(&registry),
        probe: Arc::new(Probe::default()),
    };
    if kind == EffectKind::ChildStart {
        runner
            .serve_segments(&scenario.child_start_key, scenario.child_body())
            .await;
    }

    let crash = ConformanceCrash::new();
    runner
        .run_segment_until_crash(
            &scenario.process_id,
            registration,
            body(&scenario, Phase::Crashing, Some(crash.clone())),
            crash.clone(),
        )
        .await;
    assert!(crash.has_fired(), "{name}: the crash fired");
    let at_crash = assert_crash_precondition(&scenario);

    runner
        .recover_segment(
            &scenario.process_id,
            recovery,
            body(&scenario, Phase::Recovery, None),
        )
        .await;

    let outcome = registry
        .get_process(&scenario.process_id)
        .await
        .unwrap_or_else(|error| panic!("{name}: read the recovered process: {error}"))
        .unwrap_or_else(|| panic!("{name}: the process exists"))
        .outcome
        .unwrap_or_else(|| panic!("{name}: the recovered process reached its terminal"));
    let probe = &scenario.probe;
    match recovery {
        SegmentRecovery::Replay => {
            assert_eq!(
                outcome.terminal_status(),
                Some(crate::ProcessStatus::Completed),
                "{name}: the replayed segment completes: {outcome:?}"
            );
            for effect in kind.effects() {
                let runs = probe.runs(effect);
                let expected = expected_after_replay(&scenario, effect, at_crash[effect]);
                assert_eq!(
                    runs.len(),
                    expected,
                    "{name}: `{effect}` runs across the re-drive: {runs:?}"
                );
                assert!(
                    runs.windows(2).all(|pair| pair[0] == pair[1]),
                    "{name}: every run of `{effect}` has one identity: {runs:?}"
                );
                if *effect == CHILD {
                    let child_id = scenario
                        .child_id()
                        .unwrap_or_else(|| panic!("{name}: the child's id was seen"));
                    assert!(
                        runs.iter()
                            .all(|run| run.call_id == ExecutionScope::process(&child_id).id()),
                        "{name}: the child ran as the process the start named: {runs:?}"
                    );
                    assert_eq!(
                        probe.observed(Phase::Recovery, effect),
                        Some(Ok(1)),
                        "{name}: the re-drive's start answers the child"
                    );
                } else {
                    assert_eq!(
                        runs.first()
                            .map(|run| (run.call_id.clone(), run.replay_key.clone())),
                        Some((scenario.call_id(effect), scenario.replay_key(effect))),
                        "{name}: `{effect}` runs under the call id and replay key it was issued with"
                    );
                    assert_eq!(
                        probe.observed(Phase::Recovery, effect),
                        Some(Ok(expected)),
                        "{name}: the re-drive observes the result of `{effect}`'s last run, \
                         the recorded one where it was recorded"
                    );
                }
            }
        }
        SegmentRecovery::SubstrateLost => {
            assert!(
                matches!(
                    &outcome,
                    ProcessAwaitOutput::Abandoned { evidence, .. }
                        if evidence.writer
                            == crate::AbandonWriter::ResumeRefused {
                                reason: crate::ProcessResumeRefusal::SubstrateLost,
                            }
                ),
                "{name}: a segment whose record is gone ends SubstrateLost: {outcome:?}"
            );
            assert_eq!(
                probe.recovery_bodies.load(Ordering::SeqCst),
                0,
                "{name}: the started segment is never re-run from scratch"
            );
            for effect in kind.effects() {
                assert_eq!(
                    probe.runs(effect).len(),
                    at_crash[effect],
                    "{name}: `{effect}` runs no further after the substrate loss"
                );
            }
        }
    }
}

/// See the module docs.
pub async fn segment_redrive_never_reexecutes_a_recorded_effect(
    prefix: &str,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for kind in EffectKind::ALL {
        for case in CrashCase::ALL {
            for recovery in [SegmentRecovery::Replay, SegmentRecovery::SubstrateLost] {
                Box::pin(run_scenario(prefix, &stores, &runner, kind, case, recovery)).await;
            }
        }
    }
}
