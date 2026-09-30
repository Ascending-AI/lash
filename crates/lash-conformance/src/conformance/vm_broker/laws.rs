//! The FIG-4159 laws. See the parent module for the harness.

use lash_vm_broker::OperationRequestCodec;

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_sansio::{SessionId, ToolCallId, TurnId};
use lash_vm_broker::testing::{Fault, MemoryCheckpoints, ScriptedProgram, Step};
use lash_vm_broker::{BrokerFailure, BrokeredEnd, CodeCallIdentities, Invocation};
use lash_vm_protocol::{EffectKind, InfrastructureOutcome};
use pretty_assertions::assert_eq;

use super::{CELL, HostStop, Phase, Scenario, echo, within_budget};
use crate::{AdmittedScope, ConformanceTurnRunner, ExecutionScope};

fn admitted(prefix: &str, law: &str, case: &str) -> AdmittedScope {
    let session_id = SessionId::from(format!("{prefix}-{law}-{case}"));
    let turn_id = TurnId::from(format!("{prefix}-{law}-{case}-turn"));
    crate::admit(ExecutionScope::turn(&session_id, &turn_id))
}

/// The identities a law's run under `scope` takes: the parent's derivation,
/// computed here independently of the run.
fn identities(scope: &AdmittedScope) -> CodeCallIdentities {
    CodeCallIdentities::cell(
        crate::EffectOpener::for_scope(scope)
            .unwrap_or_else(|error| panic!("the law's scope opens: {error}")),
        CELL,
    )
}

/// The kill-point program: two calls, pure computation, an aggregate of two
/// leaves, and a last call.
fn matrix_program() -> ScriptedProgram {
    ScriptedProgram::new(vec![
        Step::Invoke(echo(1)),
        Step::Compute,
        Step::Invoke(echo(2)),
        Step::Aggregate(vec![echo(31), echo(32)]),
        Step::Invoke(echo(4)),
    ])
}

/// Every logical call of [`matrix_program`], in issue order: one id per
/// call, an aggregate's leaves at their first-appearance index.
fn matrix_calls(identities: &CodeCallIdentities) -> Vec<ToolCallId> {
    vec![
        identities.call_id(0),
        identities.call_id(1),
        identities.child_call_id(2, 0),
        identities.child_call_id(2, 1),
        identities.call_id(3),
    ]
}

/// The calls each result of a completed run names, in program order.
fn result_calls(end: &BrokeredEnd) -> Vec<(String, u64)> {
    let BrokeredEnd::Complete { value, .. } = end else {
        panic!("the run completes: {end:?}");
    };
    let results = lash_vm_broker::authority::decode_value(value)
        .unwrap_or_else(|error| panic!("the run's value decodes: {error}"));
    let mut calls = Vec::new();
    let mut take = |result: &serde_json::Value| {
        calls.push((
            result["call"].as_str().unwrap_or_default().to_string(),
            result["run"].as_u64().unwrap_or_default(),
        ));
    };
    for result in results.as_array().into_iter().flatten() {
        match result.as_array() {
            Some(leaves) => leaves.iter().for_each(&mut take),
            None => take(result),
        }
    }
    calls
}

/// What a crashed attempt must have ended with.
struct ExpectedCrash {
    fault: Fault,
    /// How many tool bodies ran by the time the crashed attempt ended.
    runs_at_crash: usize,
    outcome: fn(&BrokerFailure) -> bool,
}

/// Runs one kill point of the matrix and asserts its guarantees.
async fn kill_point(
    prefix: &str,
    runner: &Arc<dyn ConformanceTurnRunner>,
    law: &str,
    expected: ExpectedCrash,
) -> Arc<Scenario> {
    let scope = admitted(prefix, law, "matrix");
    let calls = matrix_calls(&identities(&scope));
    let scenario = Arc::new(Scenario::new(law, matrix_program()));
    runner
        .run_crashed_then_redriven_turn(
            scope,
            scenario.crashing(Some(expected.fault.clone())),
            scenario.healthy(),
        )
        .await;
    let probe = &scenario.probe;
    // The typed outcome of the crash.
    let crashed = probe
        .end(Phase::Crashing)
        .unwrap_or_else(|| panic!("{law}: the crashing attempt ran"));
    let failure = crashed
        .as_ref()
        .err()
        .unwrap_or_else(|| panic!("{law}: the crashing attempt fails typed: {crashed:?}"));
    assert!(
        (expected.outcome)(failure),
        "{law}: the crashed attempt's typed outcome: {failure:?}"
    );
    assert!(
        failure.is_retryable(),
        "{law}: the failure re-drives: {failure:?}"
    );
    let crash_pool = probe
        .pool(Phase::Crashing)
        .unwrap_or_else(|| panic!("{law}: the crashing attempt's pool"));
    assert_eq!(
        crash_pool.discards, 1,
        "{law}: the lost worker is discarded"
    );
    assert_eq!(
        crash_pool.releases, 0,
        "{law}: a lost worker is never reused"
    );
    // The re-drive completes.
    let redriven = probe
        .end(Phase::Healthy)
        .unwrap_or_else(|| panic!("{law}: the re-drive ran"))
        .unwrap_or_else(|failure| panic!("{law}: the re-drive completes: {failure:?}"));
    // No recorded effect re-executes, and none is lost: every call's body
    // ran exactly once, and the completed run reads that one run.
    let runs = probe.runs();
    let mut per_call = BTreeMap::<String, usize>::new();
    for run in &runs {
        *per_call.entry(run.call_id.clone()).or_default() += 1;
        assert_eq!(
            run.replay_key,
            format!("vm-broker:{}", run.call_id),
            "{law}: every run journals under its call's id"
        );
    }
    let expected_calls = calls
        .iter()
        .map(|call| (call.to_string(), 1_usize))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        per_call, expected_calls,
        "{law}: one run per logical call, under one ToolCallId each"
    );
    assert_eq!(
        result_calls(&redriven),
        calls
            .iter()
            .map(|call| (call.to_string(), 1))
            .collect::<Vec<_>>(),
        "{law}: the completed run reads each call's recorded result, in issue order"
    );
    assert_eq!(
        probe.runs_at_crash(),
        Some(expected.runs_at_crash),
        "{law}: the calls that ran before the crash"
    );
    scenario
}

fn crashed(failure: &BrokerFailure) -> bool {
    matches!(
        failure,
        BrokerFailure::WorkerLost {
            outcome: InfrastructureOutcome::WorkerCrashed { .. },
            ..
        }
    )
}

/// A worker that dies before its run starts runs no effect; the re-drive
/// runs every one once.
pub async fn worker_kill_before_start_runs_no_effect(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "worker-kill-before-start";
    within_budget(law, async {
        let scenario = kill_point(
            prefix,
            &runner,
            law,
            ExpectedCrash {
                fault: Fault::DieBeforeStart,
                runs_at_crash: 0,
                outcome: crashed,
            },
        )
        .await;
        let failure = scenario.probe.end(Phase::Crashing).and_then(Result::err);
        assert_eq!(
            failure.as_ref().and_then(BrokerFailure::settlement),
            Some(&lash_vm_broker::Settlement::default()),
            "{law}: nothing was admitted"
        );
        let pool = scenario.probe.pool(Phase::Crashing).unwrap_or_default();
        assert_eq!(pool.starts, 0, "{law}: the dead worker started no run");
    })
    .await;
}

/// A worker that dies mid-compute is recovered by the substrate re-driving
/// the invocation: the call recorded before the crash is served, the rest
/// run once.
pub async fn worker_kill_mid_compute_redrives_through_the_substrate(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "worker-kill-mid-compute";
    within_budget(law, async {
        let scenario = kill_point(
            prefix,
            &runner,
            law,
            ExpectedCrash {
                fault: Fault::DieMidCompute,
                runs_at_crash: 1,
                outcome: crashed,
            },
        )
        .await;
        let pool = scenario.probe.pool(Phase::Healthy).unwrap_or_default();
        assert_eq!(
            pool.checkouts, 1,
            "{law}: the re-drive rebuilds the run on one fresh worker, from the journal"
        );
    })
    .await;
}

/// A worker that dies after its request reached the parent, before the
/// outcome was recorded: the parent settles the admitted operation within
/// its invocation, once, and the re-drive serves it.
pub async fn worker_kill_after_request_before_record_settles_the_admitted_operation_once(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "worker-kill-after-request";
    within_budget(law, async {
        let scope = admitted(prefix, law, "matrix");
        let calls = matrix_calls(&identities(&scope));
        let scenario = kill_point(
            prefix,
            &runner,
            law,
            ExpectedCrash {
                fault: Fault::DieAfterRequest(1),
                runs_at_crash: 2,
                outcome: crashed,
            },
        )
        .await;
        let failure = scenario
            .probe
            .end(Phase::Crashing)
            .and_then(Result::err)
            .unwrap_or_else(|| panic!("{law}: the crash failed"));
        let settlement = failure
            .settlement()
            .unwrap_or_else(|| panic!("{law}: a lost worker reports its settlement"));
        assert!(
            settlement.parked.is_empty(),
            "{law}: nothing was left unsettled"
        );
        assert_eq!(
            settlement
                .settled
                .iter()
                .map(|settled| (settled.ordinal, settled.call_ids.clone()))
                .collect::<Vec<_>>(),
            vec![(1, vec![calls[1].clone()])],
            "{law}: the admitted operation settled under its identity"
        );
    })
    .await;
}

/// A worker that dies after the parent recorded its outcome, before it was
/// delivered: the re-drive replays the record with zero dispatch.
pub async fn worker_kill_after_record_before_delivery_replays_with_zero_dispatch(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "worker-kill-after-record";
    within_budget(law, async {
        let scenario = kill_point(
            prefix,
            &runner,
            law,
            ExpectedCrash {
                fault: Fault::DieBeforeDelivery(1),
                runs_at_crash: 2,
                outcome: crashed,
            },
        )
        .await;
        // Two calls ran before the crash; the re-drive dispatched only the
        // three that had no record.
        assert_eq!(
            scenario.probe.runs().len(),
            5,
            "{law}: the recorded outcome was served, never dispatched again"
        );
    })
    .await;
}

/// A worker that dies while it serializes its state: the partial frame is
/// refused, nothing is committed from it, and the last committed checkpoint
/// stands. The run it re-drives resumes from that checkpoint, with every
/// call before it served from its record.
pub async fn worker_kill_mid_serialization_keeps_the_last_checkpoint(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "worker-kill-mid-serialization";
    within_budget(law, async {
        // One process body across two segments: its calls root in the
        // process, and its checkpoint crosses from the first segment's
        // invocation to the second's.
        let process_id = crate::ProcessId::fixture(&format!("{prefix}-{law}"));
        let owner = lash_vm_protocol::VmOwner::new(format!("process:{process_id}"));
        let identities = CodeCallIdentities::process_body(process_id);
        let carried = Arc::new(MemoryCheckpoints::default());
        let program = ScriptedProgram::new(vec![
            Step::Invoke(echo(1)),
            Step::Boundary,
            Step::Invoke(echo(2)),
        ]);
        let first = Arc::new(Scenario {
            carried: Some(Arc::clone(&carried)),
            owner: Some(owner.clone()),
            identities: Some(identities.clone()),
            ..Scenario::new(law, program.clone())
        });
        runner
            .run_turn(admitted(prefix, law, "segment-1"), first.healthy())
            .await;
        let parked = match first.probe.end(Phase::Healthy) {
            Some(Ok(BrokeredEnd::Suspended { checkpoint })) => checkpoint,
            other => panic!("{law}: the first segment parks at its boundary: {other:?}"),
        };
        assert_eq!(parked.ledger.next_ordinal, 1, "{law}: the checkpoint's ledger matches its state");
        let second = Arc::new(Scenario {
            carried: Some(Arc::clone(&carried)),
            resume: Some(parked.clone()),
            owner: Some(owner),
            identities: Some(identities.clone()),
            probe: Arc::clone(&first.probe),
            ..Scenario::new(law, program)
        });
        runner
            .run_crashed_then_redriven_turn(
                admitted(prefix, law, "segment-2"),
                second.crashing(Some(Fault::DieMidSerialization)),
                second.healthy(),
            )
            .await;
        let crashed_end = second.probe.end(Phase::Crashing);
        assert!(
            matches!(&crashed_end, Some(Err(failure)) if crashed(failure) && failure.is_retryable()),
            "{law}: a cut-off frame loses the worker, typed and retryable: {crashed_end:?}"
        );
        let commits = carried.commits();
        assert_eq!(
            commits.first(),
            Some(&parked),
            "{law}: the parked checkpoint was committed first"
        );
        assert!(
            commits.iter().all(|checkpoint| {
                checkpoint == &parked
                    || matches!(checkpoint.vm.kind(), lash_vm_protocol::VmStateKind::Snapshot)
            }),
            "{law}: nothing but the parked checkpoint and the re-drive's completion was committed"
        );
        let completed = second
            .probe
            .end(Phase::Healthy)
            .unwrap_or_else(|| panic!("{law}: the re-drive ran"))
            .unwrap_or_else(|failure| panic!("{law}: the re-drive completes: {failure:?}"));
        assert_eq!(
            result_calls(&completed),
            vec![
                (identities.call_id(0).to_string(), 1),
                (identities.call_id(1).to_string(), 1),
            ],
            "{law}: the resumed run keeps the parked call's result and its ordinals"
        );
        let runs = second.probe.runs();
        assert_eq!(runs.len(), 2, "{law}: each call ran once: {runs:?}");
    })
    .await;
}

/// A worker that sent its whole completion and died before the parent
/// committed it: the completion wins over the death, and it is committed
/// once, however often the invocation is re-driven after.
pub async fn worker_kill_after_complete_before_commit_commits_once(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "worker-kill-after-complete";
    within_budget(law, async {
        let scope = admitted(prefix, law, "matrix");
        let calls = matrix_calls(&identities(&scope));
        let carried = Arc::new(MemoryCheckpoints::default());
        let scenario = Arc::new(Scenario {
            carried: Some(Arc::clone(&carried)),
            ..Scenario::new(law, matrix_program())
        });
        // The parent itself dies after the commit: the tier re-drives the
        // invocation, which replays to the same completion.
        runner
            .run_crashed_then_redriven_turn(
                scope,
                scenario.crashing(Some(Fault::DieAfterComplete)),
                scenario.healthy(),
            )
            .await;
        for phase in [Phase::Crashing, Phase::Healthy] {
            let end = scenario.probe.end(phase);
            assert!(
                matches!(&end, Some(Ok(BrokeredEnd::Complete { .. }))),
                "{law}: a whole completion wins over the worker's death ({phase:?}): {end:?}"
            );
        }
        let commits = carried.commits();
        assert_eq!(
            commits.len(),
            1,
            "{law}: the completion is committed once: {commits:?}"
        );
        assert_eq!(
            commits[0].ledger.next_ordinal, 4,
            "{law}: with the ledger it matches"
        );
        let runs = scenario.probe.runs();
        assert_eq!(
            runs.len(),
            calls.len(),
            "{law}: no call ran twice: {runs:?}"
        );
    })
    .await;
}

/// A worker's request for anything its run was not admitted with is refused
/// before any tool is invoked, and takes no ordinal: the worker's ids, bytes
/// and claims confer nothing.
pub async fn unauthorized_worker_effect_request_is_refused_without_invoking_a_tool(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "unauthorized-worker-request";
    within_budget(law, async {
        let scope = admitted(prefix, law, "requests");
        let identities = identities(&scope);
        let raw = |kind: EffectKind, payload: Vec<u8>| Step::Raw { kind, payload };
        let invoke = |binding: &str, operation: &str, arguments: serde_json::Value| {
            Invocation {
                binding: binding.into(),
                operation: operation.into(),
                arguments,
            }
            .request()
            .encode()
            .0
        };
        let forged = vec![
            // A binding the run was never admitted with.
            raw(
                EffectKind::ResourceOperation,
                invoke("secrets", "read", serde_json::json!({ "value": 1 })),
            ),
            // An operation the binding does not expose.
            raw(
                EffectKind::ResourceOperation,
                invoke("tools", "delete", serde_json::json!({ "value": 1 })),
            ),
            // Arguments smuggling a host binding past the contract.
            raw(
                EffectKind::ResourceOperation,
                invoke(
                    "tools",
                    "echo",
                    serde_json::json!({ "value": 1, "execution_binding": "host-secret" }),
                ),
            ),
            // A call id the worker claims for itself.
            raw(
                EffectKind::ResourceOperation,
                rmp_serde::to_vec_named(&serde_json::json!({
                    "invoke": { "binding": "tools", "operation": "echo",
                                "arguments": { "value": 1 }, "call_id": "tc_forged" }
                }))
                .unwrap_or_default(),
            ),
            // A handle the parent never granted.
            raw(
                EffectKind::Await,
                lash_vm_broker::OperationRequest::Await(lashlang::Value::String(
                    "forged-handle".into(),
                ))
                .encode()
                .0,
            ),
            // A payload under another kind's header.
            raw(
                EffectKind::Sleep,
                invoke("tools", "echo", serde_json::json!({ "value": 1 })),
            ),
            // A kind the broker does not broker.
            raw(EffectKind::ProcessEvent, Vec::new()),
        ];
        let refusals = forged.len();
        let mut steps = forged;
        steps.push(Step::Invoke(echo(7)));
        let scenario = Arc::new(Scenario::new(law, ScriptedProgram::new(steps)));
        runner.run_turn(scope, scenario.healthy()).await;
        let end = scenario
            .probe
            .end(Phase::Healthy)
            .unwrap_or_else(|| panic!("{law}: the run ran"))
            .unwrap_or_else(|failure| panic!("{law}: the run completes: {failure:?}"));
        let BrokeredEnd::Complete { value, .. } = &end else {
            panic!("{law}: the run completes: {end:?}");
        };
        let results = lash_vm_broker::authority::decode_value(value)
            .ok()
            .and_then(|results| results.as_array().cloned())
            .unwrap_or_default();
        assert_eq!(results.len(), refusals + 1);
        for (index, result) in results.iter().take(refusals).enumerate() {
            assert_eq!(
                result["failed"]["code"], "lash_vm_request_refused",
                "{law}: request {index} is refused: {result}"
            );
        }
        let runs = scenario.probe.runs();
        assert_eq!(
            runs.iter()
                .map(|run| run.call_id.clone())
                .collect::<Vec<_>>(),
            vec![identities.call_id(0).to_string()],
            "{law}: only the admitted call ran, at the first ordinal: refusals take none"
        );
    })
    .await;
}

/// A message under a stale lease or frame, a replayed frame and a repeated
/// request id are never applied: the worker is discarded typed, and the
/// re-drive runs each call once.
pub async fn stale_epoch_and_duplicate_worker_messages_are_refused(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "stale-and-duplicate-worker-messages";
    within_budget(law, async {
        for (case, fault) in [
            ("stale-lease", Fault::StaleLease(1)),
            ("stale-frame", Fault::StaleFrameEpoch(1)),
            ("replayed-frame", Fault::ReplayedFrame(1)),
            ("repeated-request-id", Fault::RepeatedRequestId(1)),
        ] {
            let name = format!("{law}/{case}");
            let scope = admitted(prefix, law, case);
            let calls = matrix_calls(&identities(&scope));
            let scenario = Arc::new(Scenario::new(name.clone(), matrix_program()));
            runner
                .run_crashed_then_redriven_turn(
                    scope,
                    scenario.crashing(Some(fault)),
                    scenario.healthy(),
                )
                .await;
            let crashed = scenario.probe.end(Phase::Crashing);
            assert!(
                matches!(
                    &crashed,
                    Some(Err(BrokerFailure::WorkerLost {
                        outcome: InfrastructureOutcome::ProtocolViolation { .. },
                        ..
                    }))
                ),
                "{name}: the message is refused as a protocol violation: {crashed:?}"
            );
            let redriven = scenario
                .probe
                .end(Phase::Healthy)
                .unwrap_or_else(|| panic!("{name}: the re-drive ran"))
                .unwrap_or_else(|failure| panic!("{name}: the re-drive completes: {failure:?}"));
            let mut per_call = BTreeMap::<String, usize>::new();
            for run in scenario.probe.runs() {
                *per_call.entry(run.call_id).or_default() += 1;
            }
            assert_eq!(
                per_call,
                calls
                    .iter()
                    .map(|call| (call.to_string(), 1))
                    .collect::<BTreeMap<_, _>>(),
                "{name}: no refused message dispatched anything twice"
            );
            assert_eq!(result_calls(&redriven).len(), calls.len());
            runner.scenario_finished().await;
        }
    })
    .await;
}

/// The winner of a cancellation is the journaled checkpoint observation,
/// across a worker kill.
///
/// - **The host cancels, then the worker is killed.** Checkpoint 1 was
///   observed not cancelled; the host then cancels (durably and live) and
///   the worker dies before checkpoint 2. Nothing the crash did decides: the
///   re-drive replays checkpoint 1's observation unchanged, is cancelled at
///   checkpoint 2, and runs nothing after it.
/// - **An unsolicited stop.** A live stop with nothing durable behind it
///   (a stray `Cancel`) stops or kills the worker; the re-drive observes both
///   checkpoints not cancelled and completes, branching exactly as a run that
///   was never stopped.
pub async fn cancellation_winner_is_the_journaled_checkpoint_across_worker_kill(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "cancellation-winner-across-kill";
    within_budget(law, async {
        let program = ScriptedProgram::new(vec![
            Step::Invoke(echo(1)),
            Step::Checkpoint(1),
            Step::Compute,
            Step::Checkpoint(2),
            Step::Invoke(echo(2)),
        ]);
        for (case, stop, fault) in [
            ("host-cancel-then-kill", HostStop::Cancel, Some(Fault::DieMidCompute)),
            ("unsolicited-stop", HostStop::Unsolicited, None),
        ] {
            let name = format!("{law}/{case}");
            let scope = admitted(prefix, law, case);
            let identities = identities(&scope);
            let scenario = Arc::new(Scenario {
                stop_after_checkpoint: Some((1, stop)),
                ..Scenario::new(name.clone(), program.clone())
            });
            runner
                .run_crashed_then_redriven_turn(scope, scenario.crashing(fault), scenario.healthy())
                .await;
            let crashed = scenario.probe.end(Phase::Crashing);
            let calls: Vec<String> = scenario
                .probe
                .runs()
                .iter()
                .map(|run| run.call_id.clone())
                .collect();
            match stop {
                HostStop::Cancel => {
                    assert!(
                        matches!(
                            &crashed,
                            Some(Err(
                                BrokerFailure::WorkerLost { .. } | BrokerFailure::Interrupted { .. }
                            ))
                        ),
                        "{name}: the kill decides nothing: {crashed:?}"
                    );
                    assert_eq!(
                        scenario.probe.observations(Phase::Crashing),
                        BTreeMap::from([(1, false)]),
                        "{name}: the crashed run observed checkpoint 1 not cancelled"
                    );
                    assert_eq!(
                        scenario.probe.end(Phase::Healthy),
                        Some(Ok(BrokeredEnd::Cancelled)),
                        "{name}: the re-drive is cancelled"
                    );
                    assert_eq!(
                        scenario.probe.observations(Phase::Healthy),
                        BTreeMap::from([(1, false), (2, true)]),
                        "{name}: the replay keeps checkpoint 1's journaled answer; checkpoint 2 decides"
                    );
                    assert_eq!(
                        calls,
                        vec![identities.call_id(0).to_string()],
                        "{name}: nothing after the winning checkpoint ran"
                    );
                }
                HostStop::Unsolicited => {
                    assert!(
                        !matches!(&crashed, Some(Ok(BrokeredEnd::Cancelled))),
                        "{name}: an unsolicited stop never ends a run cancelled: {crashed:?}"
                    );
                    assert!(
                        matches!(scenario.probe.end(Phase::Healthy), Some(Ok(BrokeredEnd::Complete { .. }))),
                        "{name}: the re-drive completes"
                    );
                    assert_eq!(
                        scenario.probe.observations(Phase::Healthy),
                        BTreeMap::from([(1, false), (2, false)]),
                        "{name}: the stop changed no observation the replay branches on"
                    );
                    assert_eq!(
                        calls,
                        vec![identities.call_id(0).to_string(), identities.call_id(1).to_string()],
                        "{name}: each call ran once"
                    );
                }
            }
            runner.scenario_finished().await;
        }
    })
    .await;
}

/// Opening a frame (F5) fences the old frame's responses, resets its
/// persisted state atomically, then retires its live state: a run of the old
/// frame still on a worker is retired where it stands and commits nothing,
/// and the new frame's run finds the old frame's global `undefined`.
pub async fn frame_open_retires_worker_state_and_old_globals_are_undefined(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "frame-open-retires-worker-state";
    within_budget(law, async {
        let scenario = Arc::new(Scenario::new(law, ScriptedProgram::new(Vec::new())));
        let observed = Arc::new(std::sync::Mutex::new(None));
        let attempt: crate::ConformanceTurnAttempt = {
            let scenario = Arc::clone(&scenario);
            let observed = Arc::clone(&observed);
            Arc::new(move |scoped| {
                let scenario = Arc::clone(&scenario);
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    let outcome = super::frames::open_frame_under(&scenario, scoped).await;
                    *observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
                    crate::ConformanceTurnEnd::Settled
                })
            })
        };
        runner
            .run_turn(admitted(prefix, law, "frames"), attempt)
            .await;
        let outcome = observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| panic!("{law}: the frames ran"));
        assert!(
            matches!(outcome.straggler, Err(BrokerFailure::FrameRetired)),
            "{law}: the old frame's run in flight is retired: {:?}",
            outcome.straggler
        );
        assert!(
            outcome.straggler_discarded,
            "{law}: its worker is discarded, never reused"
        );
        assert!(
            outcome
                .commits
                .iter()
                .all(
                    |checkpoint| checkpoint.frame_epoch == lash_vm_protocol::FrameEpoch(0)
                        && checkpoint == &outcome.first_frame
                ),
            "{law}: the old frame committed only its own completion: {:?}",
            outcome.commits
        );
        assert!(
            outcome.persisted_after_open.is_none(),
            "{law}: the reset left nothing"
        );
        assert_eq!(
            outcome.read,
            vec![serde_json::json!("undefined")],
            "{law}: the old frame's global is undefined in the new frame"
        );
    })
    .await;
}

/// A run awaiting a parent effect that needs a worker of its own parks and
/// releases its slot: with a single slot, the nested run gets it, and the
/// parked run resumes and completes.
pub async fn one_slot_nested_effect_does_not_deadlock(
    prefix: &str,
    runner: Arc<dyn ConformanceTurnRunner>,
) {
    let law = "one-slot-nested-effect";
    within_budget(law, async {
        let scope = admitted(prefix, law, "nested");
        let identities = identities(&scope);
        let scenario = Arc::new(Scenario::new(
            law,
            ScriptedProgram::new(vec![
                Step::Invoke(echo(1)),
                Step::Invoke(Invocation {
                    binding: "tools".into(),
                    operation: "compile".into(),
                    arguments: serde_json::json!({ "source": "finish(await tools.echo({ value: 10 }))" }),
                }),
                Step::Invoke(echo(3)),
            ]),
        ));
        runner.run_turn(scope, scenario.healthy()).await;
        let end = scenario
            .probe
            .end(Phase::Healthy)
            .unwrap_or_else(|| panic!("{law}: the run ran"))
            .unwrap_or_else(|failure| panic!("{law}: the run completes, never deadlocks: {failure:?}"));
        let BrokeredEnd::Complete { value, .. } = &end else {
            panic!("{law}: the run completes: {end:?}");
        };
        let results = lash_vm_broker::authority::decode_value(value).unwrap_or_default();
        assert_eq!(results.as_array().map(Vec::len), Some(3), "{law}: {results}");
        let pool = scenario
            .probe
            .pool(Phase::Healthy)
            .unwrap_or_else(|| panic!("{law}: the pool"));
        assert_eq!(pool.max_active, 1, "{law}: one slot, never more");
        assert_eq!(
            pool.checkouts, 3,
            "{law}: the outer run, the nested run on the released slot, and the resumed run"
        );
        let runs = scenario.probe.runs();
        assert_eq!(runs.len(), 3, "{law}: each call ran once: {runs:?}");
        assert_eq!(runs[0].call_id, identities.call_id(0).to_string());
        assert_eq!(runs[2].call_id, identities.call_id(2).to_string());
    })
    .await;
}
