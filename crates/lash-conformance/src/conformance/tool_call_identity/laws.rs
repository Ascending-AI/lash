//! The tool-call identity laws. See the module documentation of
//! [`super`] for the world they run in.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{
    AttemptIdentity, DEFERRED, Execution, PROBE, ProbeArgs, SETTLEMENT_GRACE, ToolCallIdentityTier,
    World, assert_finished, calls, outputs, raw_call, text,
};

/// Panics when the effect loop ends: every tool call settled and the turn
/// has not committed.
pub(super) struct PanicBeforeTurnCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicBeforeTurnCommit {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            panic!("injected crash after the tool calls settled and before the turn commit");
        }
    }

    fn begin_named(&self, _phase: &str) {}
}

/// The one execution of `label`'s body.
pub(super) fn only(world: &World, label: &str) -> Execution {
    let executions = world.witness.of(label);
    assert_eq!(
        executions.len(),
        1,
        "`{label}` ran exactly once: {executions:?}"
    );
    executions
        .into_iter()
        .next()
        .unwrap_or_else(|| unreachable!())
}

/// Every execution of one logical call saw one never-missing call id.
pub(super) fn assert_one_identity(label: &str, executions: &[Execution]) -> String {
    assert!(!executions.is_empty(), "`{label}` ran");
    let first = executions[0]
        .identity
        .call_id
        .clone()
        .unwrap_or_else(|| panic!("`{label}`'s attempt carries a call id: {executions:?}"));
    for execution in executions {
        assert_eq!(
            execution.identity.call_id.as_deref(),
            Some(first.as_str()),
            "every re-run of `{label}` sees the call id its first run saw: {executions:?}"
        );
    }
    first
}

/// The label a settled probe call answered with.
fn answered_label(output: &serde_json::Value) -> Option<&str> {
    output.get("label").and_then(serde_json::Value::as_str)
}

/// The keys two logical calls saw must all be present and all differ.
pub(super) fn assert_distinct_keys(what: &str, one: &AttemptIdentity, other: &AttemptIdentity) {
    let mut collisions = Vec::new();
    for ((name, one), (_, other)) in one.keys().into_iter().zip(other.keys()) {
        assert!(
            one.is_some() && other.is_some(),
            "{what}: `{name}` is never missing ({one:?}, {other:?})"
        );
        if one == other {
            collisions.push(format!("{name} = {one:?}"));
        }
    }
    assert!(
        collisions.is_empty(),
        "{what}: two logical calls share their idempotency keys: {}",
        collisions.join("; ")
    );
}

/// FIG-4073's first law. Two turns of one session whose model provider emits
/// the same call id, `call_0`, are two logical calls: every key the tool can
/// key idempotency on differs between them, or an idempotent tool answers the
/// second call with the first call's result.
pub async fn repeated_provider_id_across_turns_is_distinct(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "repeated-provider-id");
    for name in ["first", "second"] {
        let turn = world.turn(
            name,
            vec![
                calls(&[("call_0", PROBE, ProbeArgs::label(name))]),
                text(&format!("{name} turn done")),
            ],
        );
        let assembled = world.run(&turn).await;
        assert_finished(name, &assembled);
        assert_eq!(
            outputs(&assembled)
                .iter()
                .map(|(_, _, output)| answered_label(output).map(str::to_owned))
                .collect::<Vec<_>>(),
            vec![Some(name.to_string())],
            "the {name} turn's call answered for itself"
        );
    }
    let first = only(&world, "first");
    let second = only(&world, "second");
    assert_distinct_keys(
        "two turns whose provider emitted `call_0` each",
        &first.identity,
        &second.identity,
    );
}

/// Two deferred calls in one execution scope — one turn, two model steps —
/// whose provider gave both the id `call_0` never consume each other's
/// completion. Each call parks on its own completion key and resolves it
/// with its own label; the second call's recorded outcome must be its own
/// label, and its key must not be the first call's.
pub async fn same_scope_completion_collision(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "same-scope-completion");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_0", DEFERRED, ProbeArgs::label("first"))]),
            calls(&[("call_0", DEFERRED, ProbeArgs::label("second"))]),
            text("both deferred calls settled"),
        ],
    );
    let assembled = world.run(&turn).await;
    assert_finished("the two-step deferred turn", &assembled);
    let answered = outputs(&assembled)
        .iter()
        .map(|(_, _, output)| answered_label(output).map(str::to_owned))
        .collect::<Vec<_>>();
    let started_second = world.witness.started("second");
    let first = world.witness.of("first");
    let second = world.witness.of("second");
    let keys = |executions: &[Execution]| {
        executions
            .iter()
            .map(|execution| execution.completion_key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        answered,
        vec![Some("first".to_string()), Some("second".to_string())],
        "each deferred call settles with its own resolution (the second call's body ran {} \
         times; completion keys: first {:?}, second {:?})",
        started_second,
        keys(&first),
        keys(&second),
    );
    let first = only(&world, "first");
    let second = only(&world, "second");
    assert_ne!(
        first.completion_key, second.completion_key,
        "two calls in one scope park on distinct completion keys"
    );
    assert_distinct_keys(
        "two calls in one turn whose provider emitted `call_0` each",
        &first.identity,
        &second.identity,
    );
}

/// The unrecorded-effect window (ADR 0110 §3): the probe's effect happened
/// and its outcome was never recorded, the execution died, and the call runs
/// again. The re-run sees the call id the first run saw.
///
/// A tier that can cut a journal cuts the probe's first attempt after its
/// body ran and before its result is durable, so the call provably runs
/// twice; the cut's replay key comes from a probe run of the same turn in a
/// sibling session. A tier that cannot holds the body after its effect and
/// kills the turn around it; the call then finishes by the held execution
/// outliving the crash or by a fresh run, and every run sees one call id.
pub async fn tool_identity_survives_unrecorded_effect_crash(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "unrecorded-effect-crash");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_effect", PROBE, ProbeArgs::label("effect"))]),
            text("the effect settled"),
        ],
    );
    let assembled = match first_attempt_key(&tier, "unrecorded-effect-crash", "call_effect").await {
        Some(key) => {
            let (report, reported) = tokio::sync::mpsc::unbounded_channel();
            let attempt = world.attempt(&turn, report);
            world
                .runner()
                .run_cut_then_redriven_turn(
                    world.admitted(&turn),
                    crate::JournalCut {
                        replay_key: key.replace(
                            &format!("{}-probe", world.session_id),
                            world.session_id.as_str(),
                        ),
                        at: crate::JournalCutPoint::BeforeResult,
                    },
                    Arc::clone(&attempt),
                    attempt,
                )
                .await;
            let executions = world.witness.of("effect");
            assert!(
                executions.len() >= 2,
                "the cut attempt's effect ran and the call ran again: {executions:?}"
            );
            last_report(reported).await
        }
        None => {
            let held = world.turn(
                "held",
                vec![
                    calls(&[("call_effect", PROBE, ProbeArgs::held("effect"))]),
                    text("the effect settled"),
                ],
            );
            crash_while_held(&world, &held, "effect").await
        }
    };
    assert_finished("the recovered turn", &assembled);
    let executions = world.witness.of("effect");
    let call_id = assert_one_identity("effect", &executions);
    eprintln!(
        "tool_identity_survives_unrecorded_effect_crash: {} run(s) of the call, all under `{call_id}`",
        executions.len()
    );
}

/// The replay key of the first attempt of the call `call_id` in a probe run
/// of a one-call turn, in the sibling session `{law}-probe`, or `None` when
/// the tier cannot read the keys it journaled.
async fn first_attempt_key(
    tier: &ToolCallIdentityTier,
    law: &str,
    call_id: &str,
) -> Option<String> {
    let probe = World::new(tier, &format!("{law}-probe"));
    let turn = probe.turn(
        "turn",
        vec![
            calls(&[(call_id, PROBE, ProbeArgs::label("probe"))]),
            text("the probe settled"),
        ],
    );
    assert_finished("the probe turn", &probe.run_kept(&turn).await);
    let keys = probe
        .runner()
        .recorded_replay_keys(&crate::ExecutionScope::turn(
            &probe.session_id,
            &turn.turn_id,
        ))
        .await?;
    let attempt = format!("{call_id}:attempt:1");
    Some(
        keys.iter()
            .find(|key| key.ends_with(&attempt))
            .cloned()
            .unwrap_or_else(|| panic!("the probe journaled `{attempt}`: {keys:#?}")),
    )
}

async fn last_report(
    reported: tokio::sync::mpsc::UnboundedReceiver<
        Result<crate::AssembledTurn, crate::RuntimeError>,
    >,
) -> crate::AssembledTurn {
    last_result(reported)
        .await
        .unwrap_or_else(|error| panic!("the recovered turn runs: {error}"))
}

/// The last execution's report: a replaying tier reports once per execution
/// that reaches the end.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the tier's runner ran the attempt it was handed"
)]
async fn last_result(
    mut reported: tokio::sync::mpsc::UnboundedReceiver<
        Result<crate::AssembledTurn, crate::RuntimeError>,
    >,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let mut last = reported.recv().await.expect("the recovered turn reports");
    while let Ok(next) = reported.try_recv() {
        last = next;
    }
    last
}

/// Runs `turn`, whose probe call `held` holds on the law's gate, kills the
/// turn's execution once that call started, and recovers it.
pub(super) async fn crash_while_held(
    world: &World,
    turn: &super::ScriptedTurn,
    held: &'static str,
) -> crate::AssembledTurn {
    crash_while_held_result(world, turn, held)
        .await
        .unwrap_or_else(|error| panic!("the recovered turn runs: {error}"))
}

/// [`crash_while_held`], answering how the recovered turn ended.
pub(super) async fn crash_while_held_result(
    world: &World,
    turn: &super::ScriptedTurn,
    held: &'static str,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    crash_when(world, turn, "the held probe starts", move |witness| {
        witness.started(held) >= 1
    })
    .await
}

/// Runs `turn`, kills its execution once `ready` holds, and recovers it.
///
/// The gate a held probe waits on opens as the crash fires. A tier that
/// keeps the crashing execution running dies at its next poll, and the held
/// call then finishes in whichever execution outlives the crash — or runs
/// again. A tier that suspends a turn at every await it cannot answer from
/// its journal has no execution left to kill: the held call finishes, the
/// turn resumes, and the resumed execution finds the crash fired and dies
/// there, so the recovery still follows a crash rather than waiting on a
/// gate that only the recovery would open.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn crash_when(
    world: &World,
    turn: &super::ScriptedTurn,
    what: &'static str,
    ready: impl Fn(&super::Witness) -> bool + Send + Sync + 'static,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let turn = turn.clone();
    let crash = crate::ConformanceCrash::new();
    let (report, reported) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let world = world.clone();
        let turn = turn.clone();
        let crash = crash.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let turn = turn.clone();
            let crash = crash.clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    () = crash.fired() => panic!("the law kills the turn's execution here"),
                    ended = world.drive(&turn, scope, None) => panic!(
                        "the crashing turn ended ({ended:?}) before its crash fired"
                    ),
                }
            })
        })
    };
    let fire = {
        let world = world.clone();
        let crash = crash.clone();
        crate::task::spawn(async move {
            world.witness.until(what, ready).await;
            // Whatever settled by now has had time to become durable.
            tokio::time::sleep(SETTLEMENT_GRACE).await;
            crash.fire();
            world.witness.open_gate();
        })
    };
    let run = world.runner().run_crashed_then_redriven_turn(
        world.admitted(&turn),
        crashing,
        world.attempt(&turn, report),
    );
    tokio::pin!(run);
    let mut fire = fire;
    // A trigger that never fires fails the law with its own message at
    // once, rather than leaving the crashing turn to run out the law's bound.
    tokio::select! {
        () = &mut run => fire.await.expect("the crash trigger's task"),
        fired = &mut fire => {
            if let Err(failed) = fired {
                std::panic::resume_unwind(failed.into_panic());
            }
            run.await;
        }
    }
    last_result(reported).await
}

/// A reported failure after the effect — a timeout, say — is retried under
/// the same call id: the first attempt's effect may have happened, so a key
/// that changed would defeat the tool's deduplication. The attempt number
/// advances.
pub async fn reported_failure_retry_preserves_call_id(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "reported-failure-retry");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_retry", PROBE, ProbeArgs::failing_first("retried"))]),
            text("the retried call settled"),
        ],
    );
    let assembled = world.run(&turn).await;
    assert_finished("the retried call's turn", &assembled);
    let executions = world.witness.of("retried");
    assert_eq!(
        executions
            .iter()
            .map(|execution| execution.identity.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the reported failure is retried once, and the attempt number advances"
    );
    assert_one_identity("retried", &executions);
    assert_eq!(
        outputs(&assembled)
            .iter()
            .map(|(_, _, output)| answered_label(output).map(str::to_owned))
            .collect::<Vec<_>>(),
        vec![Some("retried".to_string())],
        "the retry's success is the call's outcome"
    );
}

/// A call whose outcome is recorded is never executed again: the turn dies
/// after its call settled and before it commits, and the recovery reads the
/// recorded outcome back.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn recorded_outcome_skips_execution(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "recorded-outcome");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_once", PROBE, ProbeArgs::label("once"))]),
            text("the recorded call settled"),
        ],
    );
    let (report, mut reported) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let world = world.clone();
        let turn = turn.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let turn = turn.clone();
            Box::pin(async move {
                let ended = world
                    .drive(&turn, scope, Some(Arc::new(PanicBeforeTurnCommit)))
                    .await;
                panic!("the crash probe did not fire before the turn commit: {ended:?}");
            })
        })
    };
    world
        .runner()
        .run_crashed_then_redriven_turn(
            world.admitted(&turn),
            crashing,
            world.attempt(&turn, report),
        )
        .await;
    let assembled = reported
        .recv()
        .await
        .expect("the redriven turn reports")
        .unwrap_or_else(|error| panic!("the redriven turn runs: {error}"));
    assert_finished("the redriven turn", &assembled);
    let _ = only(&world, "once");
    assert_eq!(
        outputs(&assembled)
            .iter()
            .map(|(_, _, output)| answered_label(output).map(str::to_owned))
            .collect::<Vec<_>>(),
        vec![Some("once".to_string())],
        "the redrive reads the recorded outcome back"
    );
    assert_eq!(
        world.model_calls.load(Ordering::SeqCst),
        2,
        "the redrive reads the recorded model responses back instead of asking again"
    );
}

/// One model step calls a tool that does not exist, then two probes; one
/// probe settles while the other is held, and the turn dies. Neither the
/// refused sibling nor the completion order renumbers a call: the settled
/// probe is not run again, the held probe's every run sees the call id its
/// first run saw, the two probes' ids differ, and each recorded outcome
/// belongs to its own call.
pub async fn refusals_and_parallel_completion_never_renumber_identity(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "parallel-identity");
    // The quick probe comes first so a tier that runs a step's calls one at
    // a time still settles it before the held one starts.
    let mut step = calls(&[
        ("call_quick", PROBE, ProbeArgs::label("quick")),
        ("call_held", PROBE, ProbeArgs::held("held")),
    ]);
    step.parts.insert(
        0,
        raw_call(
            "call_refused",
            "identity_no_such_tool",
            serde_json::json!({}),
        ),
    );
    let turn = world.turn("turn", vec![step, text("the parallel step settled")]);
    let assembled = crash_when(
        &world,
        &turn,
        "the quick probe settles while the held one runs",
        |witness| !witness.of("quick").is_empty() && witness.started("held") >= 1,
    )
    .await
    .unwrap_or_else(|error| panic!("the recovered turn runs: {error}"));
    assert_finished("the recovered parallel turn", &assembled);
    let quick = only(&world, "quick");
    let held = assert_one_identity("held", &world.witness.of("held"));
    assert_ne!(
        quick.identity.call_id.as_deref(),
        Some(held.as_str()),
        "two probes of one step have distinct call ids"
    );
    let settled = outputs(&assembled);
    let by_call = |call_id: &str| {
        settled
            .iter()
            .find(|(recorded, _, _)| recorded.as_deref() == Some(call_id))
            .map(|(_, _, output)| output.clone())
            .unwrap_or_else(|| panic!("the turn records `{call_id}`: {settled:?}"))
    };
    assert_eq!(answered_label(&by_call("call_held")), Some("held"));
    assert_eq!(answered_label(&by_call("call_quick")), Some("quick"));
    assert!(
        answered_label(&by_call("call_refused")).is_none(),
        "the unknown tool is refused, and its refusal is its own row: {settled:?}"
    );
}
