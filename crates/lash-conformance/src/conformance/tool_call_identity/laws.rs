//! The tool-call identity laws. See the module documentation of
//! [`super`] for the world they run in.

use crate::ProcessEventLogTestSupport as _;
use crate::SessionId;
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

/// Every execution of one logical call saw one call id.
pub(super) fn assert_one_identity(label: &str, executions: &[Execution]) -> lash_core::ToolCallId {
    assert!(!executions.is_empty(), "`{label}` ran");
    let first = executions[0].identity.call_id.clone();
    for execution in executions {
        assert_eq!(
            execution.identity.call_id, first,
            "every re-run of `{label}` sees the call id its first run saw: {executions:?}"
        );
    }
    first
}

/// The label a settled probe call answered with.
fn answered_label(output: &serde_json::Value) -> Option<&str> {
    output.get("label").and_then(serde_json::Value::as_str)
}

/// The idempotency key two logical calls saw must differ.
pub(super) fn assert_distinct_keys(what: &str, one: &AttemptIdentity, other: &AttemptIdentity) {
    assert_ne!(
        one.call_id, other.call_id,
        "{what}: two logical calls share their idempotency key"
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
    let assembled = match first_attempt_key(&tier, &world, &turn, "call_effect").await {
        Some(key) => {
            let (report, reported) = tokio::sync::mpsc::unbounded_channel();
            let attempt = world.attempt(&turn, report);
            world
                .runner()
                .run_cut_then_redriven_turn(
                    world.admitted(&turn),
                    crate::JournalCut {
                        replay_key: key,
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

/// The replay key `world`'s run of `target` will journal for the first
/// attempt of its one call, the provider's `provider_call_id`, or `None` when
/// the tier cannot read the keys it journaled.
///
/// A probe run of the same script in the sibling session `{session}-probe`
/// journals the key under its own session. The call's `ToolCallId` is rooted
/// in its session's turn (ADR 0117 §2), so the key is carried over by
/// locating the probe call's positions under the probe's root and naming the
/// same positions under `world`'s.
async fn first_attempt_key(
    tier: &ToolCallIdentityTier,
    world: &World,
    target: &super::ScriptedTurn,
    provider_call_id: &str,
) -> Option<String> {
    let law = world
        .session_id
        .as_str()
        .strip_prefix(&format!("{}-", tier.prefix))
        .unwrap_or(world.session_id.as_str())
        .to_string();
    let probe = World::new(tier, &format!("{law}-probe"));
    let turn = probe.turn(
        "turn",
        vec![
            calls(&[(provider_call_id, PROBE, ProbeArgs::label("probe"))]),
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
    let probe_id = only(&probe, "probe").identity.call_id;
    let attempt = format!("{probe_id}:attempt:1");
    let key = keys
        .iter()
        .find(|key| key.ends_with(&attempt))
        .cloned()
        .unwrap_or_else(|| panic!("the probe journaled `{attempt}`: {keys:#?}"));
    let admission = |session: &lash_sansio::SessionId, turn: &super::ScriptedTurn| {
        lash_core::EffectOpener::turn(session.clone(), turn.turn_id.clone()).tool_call_admission()
    };
    let target_id = same_position(
        &admission(&probe.session_id, &turn),
        &probe_id,
        &admission(&world.session_id, target),
    )
    .unwrap_or_else(|| panic!("the probe call `{probe_id}` is a first-response model call"));
    Some(
        // The turn ids spell their session's id, so the session swap carries
        // them over too.
        key.replace(probe_id.as_str(), target_id.as_str())
            .replace(probe.session_id.as_str(), world.session_id.as_str()),
    )
}

/// The id at `under` of the model call `id` names under `from`: its
/// positions — continuation, iteration, response effect ordinal and content
/// index — found by trying the small positions a one-call turn reaches.
fn same_position(
    from: &lash_core::ToolCallAdmission,
    id: &lash_core::ToolCallId,
    under: &lash_core::ToolCallAdmission,
) -> Option<lash_core::ToolCallId> {
    use lash_core::ToolCallPosition::{ContentIndex, Continuation, EffectOrdinal, Iteration};
    for continuation in 0..8 {
        for iteration in 0..4 {
            for ordinal in 0..32 {
                let positions = [
                    Continuation(continuation),
                    Iteration(iteration),
                    EffectOrdinal(ordinal),
                    ContentIndex(0),
                ];
                if &from.call_id(&positions) == id {
                    return Some(under.call_id(&positions));
                }
            }
        }
    }
    None
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
        quick.identity.call_id, held,
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

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn one_turn_commits_history_once_and_never_replaces_existing_nodes(
    tier: ToolCallIdentityTier,
) {
    let world = World::new(&tier, "single-history-commit");
    let seed = world.turn("seed", vec![text("immutable prefix")]);
    assert_finished("seed", &world.run(&seed).await);
    let store = world.store().await;
    let before = store
        .load_session_window(&world.session_id, crate::store::WindowSelector::Current)
        .await
        .expect("read seed")
        .expect("seed window");
    let prefix = serde_json::to_value(&before.window.nodes).expect("serialize immutable prefix");
    let turn = world.turn(
        "progress",
        vec![
            calls(&[("call_0", PROBE, ProbeArgs::label("first-progress"))]),
            calls(&[("call_1", PROBE, ProbeArgs::label("second-progress"))]),
            text("all progress completes in one turn"),
        ],
    );
    assert_finished("progress", &world.run(&turn).await);
    only(&world, "first-progress");
    only(&world, "second-progress");
    let after = store
        .load_session_window(&world.session_id, crate::store::WindowSelector::Current)
        .await
        .expect("read completed turn")
        .expect("completed window");
    assert_eq!(
        after.head_revision,
        before.head_revision + 1,
        "model and tool progress commits one graph append"
    );
    let old_ids = before
        .window
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let retained = after
        .window
        .nodes
        .iter()
        .filter(|node| old_ids.contains(&node.node_id))
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(retained).expect("serialize retained prefix"),
        prefix
    );
    assert!(after.window.nodes.len() > before.window.nodes.len());
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn suspended_tool_keeps_turn_and_history_head_until_resolution(
    tier: ToolCallIdentityTier,
) {
    let world = World::new(&tier, "suspended-history");
    assert_finished(
        "seed",
        &world
            .run(&world.turn("seed", vec![text("before suspension")]))
            .await,
    );
    let store = world.store().await;
    let before = store
        .load_session_head_meta(&world.session_id)
        .await
        .expect("head before suspended tool")
        .expect("committed head");
    let turn = world.turn(
        "held-turn",
        vec![
            calls(&[("held", DEFERRED, ProbeArgs::held("suspended"))]),
            text("after resolution"),
        ],
    );
    let (assembled, ()) = tokio::join!(world.run(&turn), async {
        while world.witness.of("suspended").is_empty() {
            tokio::task::yield_now().await;
        }
        let execution = only(&world, "suspended");
        let key = execution.completion_key.expect("pending completion key");
        assert!(
            tier.effect_host
                .peek_await_event(&key)
                .await
                .expect("unsettled wait")
                .is_none()
        );
        assert_eq!(
            format!(
                "{:?}",
                store
                    .load_session_head_meta(&world.session_id)
                    .await
                    .expect("head while waiting")
            ),
            format!("{:?}", Some(before.clone()))
        );
        world.witness.gate.open();
    });
    assert_finished("resolved suspended turn", &assembled);
    let after = store
        .load_session_head_meta(&world.session_id)
        .await
        .expect("head after resolution")
        .expect("head");
    assert_eq!(after.head_revision, before.head_revision + 1);
    assert_eq!(world.witness.of("suspended").len(), 1);
    assert_eq!(outputs(&assembled).len(), 1);
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn external_completion_without_observer_writes_nothing(tier: ToolCallIdentityTier) {
    let mut world = World::new(&tier, "external-observer");
    let registry = tier.stores.process_registry();
    world.process_registry = Some(registry.clone());
    let runtime = world.runtime(None).await;
    let process = registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::json!("foreign"),
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        ))
        .await
        .expect("register foreign external process");
    let before = serde_json::to_value(&process).expect("process before");
    let events = serde_json::to_value(
        registry
            .full_event_window(&process.id, 0)
            .await
            .expect("events before"),
    )
    .expect("encode events");
    let service = runtime.process_service().expect("session service");
    let scoped = tier
        .effect_host
        .scoped_static(crate::admit(crate::ExecutionScope::runtime_operation(
            "external-observer-law",
        )))
        .expect("scope")
        .expect("static controller");
    let output = crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!("terminal"),
    ));
    let refused = service
        .complete_external(
            &world.session_id,
            &process.id,
            output.clone(),
            crate::ProcessOpScope::new(scoped.clone()),
        )
        .await;
    assert!(
        matches!(refused, Err(crate::PluginError::Session(ref message)) if message.contains("is not visible")),
        "{refused:?}"
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .get_process(&process.id)
                .await
                .expect("process after")
                .expect("process retained")
        )
        .expect("encode row"),
        before
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .full_event_window(&process.id, 0)
                .await
                .expect("events after")
        )
        .expect("encode events"),
        events
    );
    assert!(
        registry
            .terminal_publication(&process.id)
            .await
            .expect("publication")
            .is_none()
    );
    registry
        .add_observer(
            &world.session_id,
            &process.id,
            crate::ProcessObserverBy::host("observer-law"),
        )
        .await
        .expect("add observer");
    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let id = process.id.clone();
    let owned_service = service.clone();
    let session = world.session_id.clone();
    tier.runner
        .run_turn(
            crate::admit(crate::ExecutionScope::runtime_operation(
                "observed-external-completion",
            )),
            Arc::new(move |scope| {
                let (service, id, session, output, send) = (
                    owned_service.clone(),
                    id.clone(),
                    session.clone(),
                    output.clone(),
                    send.clone(),
                );
                Box::pin(async move {
                    send.send(
                        service
                            .complete_external(
                                &session,
                                &id,
                                output,
                                crate::ProcessOpScope::new(scope),
                            )
                            .await,
                    )
                    .expect("report completion");
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    received
        .recv()
        .await
        .expect("handler attempted completion")
        .expect("observed external process completes");
    runtime.park().await.expect("park runtime");
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn tool_restore_policy_survives_every_rebuild_and_rollback(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "tool-restore-policy");
    assert_finished(
        "seed",
        &world
            .run(&world.turn("seed", vec![text("persisted tool surface")]))
            .await,
    );
    let store = world.store().await;
    let mut seed_state = crate::conformance::helpers::load_window_state(&store, &world.session_id)
        .await
        .expect("seed checkpoint read")
        .expect("seed checkpoint exists");
    let mut surface = seed_state
        .tool_state_snapshot()
        .cloned()
        .expect("seed surface populated");
    surface.generation = 42;
    seed_state.set_tool_state_snapshot(Some(surface));
    let commit = crate::RuntimeCommit::persisted_state_for_test(&seed_state, &[]);
    crate::testing::store_fixtures::commit_runtime_state_for_test(
        &store,
        commit,
        "distinct-persisted-surface",
    )
    .await
    .expect("commit a surface different from the fresh live registry");
    let mut runtime = world
        .runtime_with_tool_open_mode(None, crate::ToolSurfaceOpenMode::PreservePersisted)
        .await;
    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("load persisted surface before observing it");
    let reference = runtime
        .state()
        .tool_state_ref()
        .cloned()
        .expect("persisted surface reference");
    assert!(runtime.state().preserve_tool_state_snapshot);
    let request = crate::AppendSessionNodesRequest {
        operation_id: "preserving-append".into(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "policy-pin",
            serde_json::json!("once"),
        )],
        requires_ancestor_node_id: None,
    };
    for phase in 0..4 {
        if phase == 0 {
            crate::testing::invalidate_resident_session_state_for_testing(&mut runtime);
        }
        if phase == 1 {
            runtime
                .refresh_session_graph_from_store()
                .await
                .expect("head rebuild");
        }
        if phase < 3 {
            Box::pin(runtime.append_session_nodes(request.clone()))
                .await
                .expect("append or receipt replay");
        } else {
            let mut refused = request.clone();
            refused.nodes = vec![crate::SessionAppendNode::plugin(
                "policy-pin",
                serde_json::json!("drift"),
            )];
            assert!(
                Box::pin(runtime.append_session_nodes(refused))
                    .await
                    .is_err(),
                "same operation with drift rolls back"
            );
        }
        runtime.stamp_live_plugin_state();
        assert!(
            runtime.state().preserve_tool_state_snapshot,
            "phase {phase}"
        );
        assert_eq!(
            runtime.state().tool_state_ref(),
            Some(&reference),
            "phase {phase}"
        );
    }
    Box::pin(runtime.park())
        .await
        .expect("commit after rollback");
    let mut reopened = world
        .runtime_with_tool_open_mode(None, crate::ToolSurfaceOpenMode::PreservePersisted)
        .await;
    reopened
        .refresh_session_graph_from_store()
        .await
        .expect("cold read loads the persisted surface");
    assert_eq!(reopened.state().tool_state_ref(), Some(&reference));
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn live_and_durable_queue_paths_share_results_and_capability_refusals(
    tier: ToolCallIdentityTier,
) {
    let world = World::new(&tier, "queue-rails");
    let runtime = crate::RuntimeHandle::new(world.runtime(None).await);
    let store = crate::store::SessionStore::new(world.store().await, world.session_id.clone())
        .expect("binding view");
    let backend =
        crate::conformance::LawBackend::over_stores(tier.stores.clone(), tier.effect_host.clone())
            .into_backend();
    let ops = crate::facade_support::DurableSessionOps::new(
        world.session_id.clone(),
        lash_core::drive::IngressRelay::over_backend(
            &backend,
            Arc::new(crate::NoSessionWork::new()),
            backend.clock(),
        ),
        Arc::new(crate::facade_support::InMemoryLiveReplayStore::default()),
    );
    let input = crate::TurnInput::text("same request on both rails");
    let first = runtime
        .enqueue_turn_input(
            input.clone(),
            crate::TurnInputIngress::NextTurn,
            Some("same-source".into()),
        )
        .await
        .expect("live enqueue");
    let replay = ops
        .enqueue_turn_input(
            &store,
            input,
            crate::TurnInputIngress::NextTurn,
            Some("same-source".into()),
            crate::RunSpec::default(),
        )
        .await
        .expect("durable replay");
    assert_eq!(
        serde_json::to_value(&first).expect("live result"),
        serde_json::to_value(&replay).expect("durable result")
    );
    assert_eq!(
        ops.pending_turn_inputs(&store)
            .await
            .expect("pending rail")
            .len(),
        1
    );
    assert!(
        runtime
            .cancel_queued_work_batch("unknown-batch")
            .await
            .expect("live absent batch")
            .is_none()
    );
    assert!(
        ops.cancel_queued_work_batch(&store, "unknown-batch")
            .await
            .expect("durable absent batch")
            .is_none()
    );
    assert!(
        ops.cancel_pending_turn_input(&store, first.input_id.as_str())
            .await
            .expect("durable cancellation")
            .is_cancelled()
    );
    assert!(
        ops.pending_turn_inputs(&store)
            .await
            .expect("queue empty")
            .is_empty()
    );
    let missing = crate::PendingTurnInputCancelTarget::input_id("unknown-input");
    let targets = [
        missing.clone(),
        crate::PendingTurnInputCancelTarget::source_key("unknown-source"),
    ];
    assert_eq!(
        serde_json::to_value(
            runtime
                .cancel_pending_turn_input("unknown-input")
                .await
                .expect("live unknown input")
        )
        .expect("encode live cancel"),
        serde_json::to_value(
            ops.cancel_pending_turn_input(&store, "unknown-input")
                .await
                .expect("durable unknown input")
        )
        .expect("encode durable cancel")
    );
    assert_eq!(
        serde_json::to_value(
            runtime
                .cancel_pending_turn_inputs(&targets)
                .await
                .expect("live selected targets")
        )
        .expect("encode live selected"),
        serde_json::to_value(
            ops.cancel_pending_turn_inputs(&store, &targets)
                .await
                .expect("durable selected targets")
        )
        .expect("encode durable selected")
    );
    assert_eq!(
        serde_json::to_value(
            runtime
                .cancel_pending_turn_input_suffix(&missing)
                .await
                .expect("live missing suffix")
        )
        .expect("encode live suffix"),
        serde_json::to_value(
            ops.cancel_pending_turn_input_suffix(&store, &missing)
                .await
                .expect("durable missing suffix")
        )
        .expect("encode durable suffix")
    );
    assert!(
        ops.turn_input_applications(&store)
            .await
            .expect("settled input read")
            .is_empty()
    );
    assert!(
        ops.queued_work(&store)
            .await
            .expect("queued work read")
            .is_empty()
    );
    let batch = store
        .enqueue_queued_work(crate::runtime::QueuedWorkBatchDraft::new(
            world.session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::facade_support::SessionCommand::RefreshToolCatalog {
                reason: "rail parity".into(),
            },
        ))
        .await
        .expect("populate command queue");
    let cancelled = runtime
        .cancel_queued_work_batch(batch.batch_id.as_str())
        .await
        .expect("live populated command cancel")
        .expect("same batch");
    assert_eq!(cancelled.batch_id, batch.batch_id);
    assert!(
        ops.cancel_queued_work_batch(&store, batch.batch_id.as_str())
            .await
            .expect("durable observes command cancellation")
            .is_none()
    );
    let input = runtime
        .enqueue_turn_input(
            crate::TurnInput::text("selected cancellation"),
            crate::TurnInputIngress::NextTurn,
            Some("selected-source".into()),
        )
        .await
        .expect("live selected input");
    let selected = ops
        .cancel_pending_turn_inputs(
            &store,
            &[crate::PendingTurnInputCancelTarget::source_key(
                "selected-source",
            )],
        )
        .await
        .expect("durable source-key cancellation");
    assert!(
        matches!(&selected[0].outcome, crate::PendingTurnInputCancelOutcome::Cancelled(row) if row.input_id == input.input_id)
    );
    assert!(
        ops.pending_turn_inputs(&store)
            .await
            .expect("both mutations drained")
            .is_empty()
    );

    let absent =
        crate::store::SessionStore::new(world.store().await, SessionId::from("absent-rail"))
            .expect("noncreating view");
    assert!(
        ops.enqueue_turn_input(
            &absent,
            crate::TurnInput::text("foreign request"),
            crate::TurnInputIngress::NextTurn,
            None,
            crate::RunSpec::default()
        )
        .await
        .is_err()
    );
    let blocked: Arc<dyn crate::RuntimeStore> =
        Arc::new(QueueCapabilityRefusal(world.store().await));
    let blocked_runtime = crate::RuntimeHandle::new(
        world
            .runtime_on_store(None, crate::ToolSurfaceOpenMode::Reconcile, blocked.clone())
            .await,
    );
    let blocked_view = crate::store::SessionStore::new(blocked, world.session_id.clone())
        .expect("capability-refusing view");
    let live = blocked_runtime
        .enqueue_turn_input(
            crate::TurnInput::text("unsupported"),
            crate::TurnInputIngress::NextTurn,
            None,
        )
        .await
        .expect_err("live capability refusal");
    let durable = ops
        .enqueue_turn_input(
            &blocked_view,
            crate::TurnInput::text("unsupported"),
            crate::TurnInputIngress::NextTurn,
            None,
            crate::RunSpec::default(),
        )
        .await
        .expect_err("durable capability refusal");
    assert_eq!(live.code, durable.code);
    assert_eq!(live.message, durable.message);
    assert!(live.message.contains("admit_pending_turn_inputs"));
    macro_rules! refused {
        ($live:expr, $durable:expr, $operation:literal) => {{
            let live = $live.await.expect_err("live capability refusal");
            let durable = $durable.await.expect_err("durable capability refusal");
            assert_eq!(live.code, durable.code);
            assert_eq!(live.message, durable.message);
            assert!(live.message.contains($operation), "{}", live.message);
        }};
    }
    refused!(
        blocked_runtime.cancel_pending_turn_input("unknown"),
        ops.cancel_pending_turn_input(&blocked_view, "unknown"),
        "cancel_pending_turn_inputs"
    );
    refused!(
        blocked_runtime.cancel_pending_turn_inputs(&targets),
        ops.cancel_pending_turn_inputs(&blocked_view, &targets),
        "cancel_pending_turn_inputs"
    );
    refused!(
        blocked_runtime.cancel_pending_turn_input_suffix(&missing),
        ops.cancel_pending_turn_input_suffix(&blocked_view, &missing),
        "cancel_pending_turn_input_suffix"
    );
    refused!(
        blocked_runtime.cancel_queued_work_batch("unknown"),
        ops.cancel_queued_work_batch(&blocked_view, "unknown"),
        "cancel_queued_work_batch"
    );
    for (operation, error) in [
        (
            "list_pending_turn_inputs",
            ops.pending_turn_inputs(&blocked_view)
                .await
                .expect_err("pending read capability"),
        ),
        (
            "list_turn_input_applications",
            ops.turn_input_applications(&blocked_view)
                .await
                .expect_err("settled read capability"),
        ),
        (
            "list_open_queued_work",
            ops.queued_work(&blocked_view)
                .await
                .expect_err("command read capability"),
        ),
    ] {
        assert!(error.message.contains(operation), "{}", error.message);
    }
}

struct QueueCapabilityRefusal(Arc<dyn crate::RuntimeStore>);
#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for QueueCapabilityRefusal {
    type Inner = dyn crate::RuntimeStore;
    fn inner(&self) -> &Self::Inner {
        self.0.as_ref()
    }
    async fn admit_pending_turn_inputs(
        &self,
        _batch: crate::PendingTurnInputBatch,
        _ttl: u64,
    ) -> Result<crate::TurnInputAdmission, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "admit_pending_turn_inputs",
        })
    }
    async fn cancel_pending_turn_inputs(
        &self,
        _session: &SessionId,
        _targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "cancel_pending_turn_inputs",
        })
    }
    async fn cancel_pending_turn_input_suffix(
        &self,
        _session: &SessionId,
        _anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "cancel_pending_turn_input_suffix",
        })
    }
    async fn cancel_queued_work_batch(
        &self,
        _session: &SessionId,
        _batch: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "cancel_queued_work_batch",
        })
    }
    async fn list_pending_turn_inputs(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_pending_turn_inputs",
        })
    }
    async fn list_turn_input_applications(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<crate::TurnInputApplication>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_turn_input_applications",
        })
    }
    async fn list_open_queued_work(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_open_queued_work",
        })
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn fork_inherits_history_without_execution_queues_waits_or_journals_on_engine(
    tier: ToolCallIdentityTier,
) {
    let factory = tier.stores.session_store_factory();
    let world = World::new(&tier, "fork-journal-isolation");
    assert_finished(
        "seed",
        &world
            .run(&world.turn("seed", vec![text("fork journal prefix")]))
            .await,
    );
    let store = world.store().await;
    let before = store
        .load_session_window(&world.session_id, crate::store::WindowSelector::Current)
        .await
        .expect("source window")
        .expect("source");
    let leaf = before.window.leaf_node_id.clone().expect("source leaf");
    let branch = SessionId::from(format!("{}-branch", world.session_id));
    let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for session in [&world.session_id, &branch] {
        if session == branch {
            factory
                .fork_session(&crate::ForkSessionRequest {
                    pending_observer_intents: Vec::new(),
                    session_id: branch.clone(),
                    node_id: leaf.clone(),
                    relation: crate::SessionRelation::Fork {
                        source_session_id: world.session_id.clone(),
                        source_node_id: leaf.clone(),
                    },
                    policy: crate::testing::mock_session_policy(),
                })
                .await
                .expect("fork after source journal settled");
        }
        let scope = crate::ExecutionScope::turn(session.clone(), "same-turn");
        assert_eq!(
            scope.session_id(),
            Some(session),
            "journal admission retains the branch session identity"
        );
        assert_ne!(
            crate::ExecutionScope::turn(world.session_id.clone(), "same-turn")
                .journal_identity()
                .expect("source journal"),
            crate::ExecutionScope::turn(branch.clone(), "same-turn")
                .journal_identity()
                .expect("branch journal"),
            "shared history never shares a journal address"
        );
        let envelope = super::super::effect_host::journaled_conformance_envelope(
            &scope,
            "same-effect",
            "same-replay-key",
        );
        let counter = executed.clone();
        let expected = serde_json::json!(session);
        tier.runner.run_turn(crate::admit(scope), Arc::new(move |controller| {
            let (envelope, counter, expected) = (envelope.clone(), counter.clone(), expected.clone());
            Box::pin(async move {
                let output = expected.clone();
                let result = controller.execute_effect(envelope, crate::RuntimeEffectLocalExecutor::testing(move |_| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue { value: output })
                })).await.expect("journaled effect in its own handler");
                assert!(matches!(result, crate::RuntimeEffectOutcome::LanguageRuntimeValue { value } if value == expected), "fork cannot read its source outcome");
                crate::ConformanceTurnEnd::Settled
            })
        })).await;
    }
    assert_eq!(
        executed.load(Ordering::SeqCst),
        2,
        "both journals execute their first admission"
    );
    tier.runner.scenario_finished().await;
}
