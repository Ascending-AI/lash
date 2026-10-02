//! The `max_tool_calls` laws (FIG-4546): the session's recorded tool-call
//! limit admits every call under it untouched and refuses the call past it
//! as the program's own failure, naming the limit.
//!
//! They run over the same producers and tiers as the barrier laws, because
//! the limit is enforced where a group forms and every product surface forms
//! its groups there. What the limit counts is the surface's
//! [`ToolCallLimitUnit`]: every call one cell makes, one step's group on a
//! protocol without cells, or the calls a process holds at once.
//!
//! A refusal is observed where it matters: in what the model is shown. The
//! scenario's model reads each request it is asked to answer and records the
//! refusal's own sentence, so a law compares the limit, the count and the
//! size of the refused request, and not merely that something failed. That a
//! refused call never ran is read off the rendezvous log, which records every
//! leaf that started.

use super::*;

use pretty_assertions::assert_eq;

/// The `max_tool_calls` every scenario here records.
const LIMIT: usize = 4;

/// `schedule` under a session that records [`LIMIT`].
fn limited(schedule: Schedule) -> Schedule {
    Schedule {
        max_tool_calls: Some(LIMIT),
        ..schedule
    }
}

/// The refusal sentence in `text`: from its opening words through the count
/// of calls it refused. `None` when `text` shows no refusal.
pub(super) fn refusal_in(text: &str) -> Option<String> {
    let start = text.find("tool call limit exceeded")?;
    let refusal = &text[start..];
    let end = refusal.find(" more")? + " more".len();
    Some(refusal[..end].to_string())
}

/// The sentence a surface counting in `unit` is refused with after `counted`
/// calls, asking for `requested` more.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the refusal's own wording carries the sentence"
)]
fn expected_refusal(unit: ToolCallLimitUnit, counted: usize, requested: usize) -> String {
    let exceeded = crate::ToolCallLimitExceeded {
        scope: match unit {
            ToolCallLimitUnit::Cell | ToolCallLimitUnit::Step => crate::ToolCallLimitScope::Cell,
            ToolCallLimitUnit::Process => crate::ToolCallLimitScope::Process,
        },
        limit: crate::MaxToolCalls::new(LIMIT),
        counted,
        requested,
    };
    let sentence = refusal_in(&exceeded.to_string()).expect("a refusal words its own sentence");
    assert!(
        sentence.contains(&format!("max_tool_calls = {LIMIT}")),
        "the refusal names the limit: {sentence}"
    );
    sentence
}

/// `producer` issuing each plan as two groups in sequence, the first `first`
/// leaves and then the rest.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registration macro hands these laws staged producers only"
)]
fn staged(producer: &ToolBatchProducer, first: usize) -> ToolBatchProducer {
    let script = producer
        .staged
        .clone()
        .expect("a staged law runs over a producer that can stage its calls");
    ToolBatchProducer {
        script: Arc::new(move |plan| script(plan, first)),
        ..producer.clone()
    }
}

/// The producers of a tier that can issue two groups in sequence.
pub fn staged_producers(producers: Vec<ToolBatchProducer>) -> Vec<ToolBatchProducer> {
    let staged = producers
        .into_iter()
        .filter(|producer| producer.staged.is_some())
        .collect::<Vec<_>>();
    assert!(
        !staged.is_empty(),
        "a tier registers at least one producer that stages its calls, or the staged \
         `max_tool_calls` laws run on nothing"
    );
    staged
}

/// The producers of a tier whose staged calls are issued by the turn itself,
/// so a crash of the turn's execution is a crash of what counts them.
pub fn turn_staged_producers(producers: Vec<ToolBatchProducer>) -> Vec<ToolBatchProducer> {
    let staged = staged_producers(producers)
        .into_iter()
        .filter(|producer| producer.limit_unit != ToolCallLimitUnit::Process)
        .collect::<Vec<_>>();
    assert!(
        !staged.is_empty(),
        "a tier registers at least one producer that stages its calls from the turn, or the \
         `max_tool_calls` crash law runs on nothing"
    );
    staged
}

/// The producers of a tier whose surface holds a group past its consumer.
pub fn holding_producers(producers: Vec<ToolBatchProducer>) -> Vec<ToolBatchProducer> {
    let holding = producers
        .into_iter()
        .filter(|producer| producer.holding.is_some())
        .collect::<Vec<_>>();
    assert!(
        !holding.is_empty(),
        "a tier registers at least one producer that holds a group past its consumer, or \
         the `max_tool_calls` holding law runs on nothing"
    );
    holding
}

fn sorted(mut leaves: Vec<String>) -> Vec<String> {
    leaves.sort();
    leaves
}

/// A group of exactly `max_tool_calls` calls runs as it always did, and a
/// group of one more is refused whole: none of its calls starts, and the
/// model is shown the refusal naming the limit.
pub async fn tool_call_limit_admits_the_limit_and_refuses_the_group_past_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);

    // At the limit: the barrier law's own shape, so the limit changed nothing
    // about how the calls under it run.
    let at = plan("limitat", &leaf_routes(LIMIT));
    let observed = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &at,
        limited(Schedule::GATED),
        BTreeMap::new(),
    )
    .await;
    assert_activation_shape(&format!("{context} at the limit"), &at, &observed);
    assert_eq!(
        observed.refusals,
        Vec::<String>::new(),
        "{context}: a group of exactly max_tool_calls calls is not refused",
    );

    // One past it.
    let past = plan("limitpast", &leaf_routes(LIMIT + 1));
    let observed = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &past,
        limited(Schedule::SERIAL_SAFE),
        BTreeMap::new(),
    )
    .await;
    assert_eq!(
        observed.started(),
        Vec::<String>::new(),
        "{context}: no call of a refused group runs; the turn ended {:?}",
        observed.end,
    );
    assert_eq!(
        observed.refusals.first(),
        Some(&expected_refusal(producer.limit_unit, 0, LIMIT + 1)),
        "{context}: the model is shown the refusal, naming the limit; the turn ended {:?}",
        observed.end,
    );
}

/// Calls issued in sequence are counted the way the surface's unit says:
///
/// * a cell's total: after `max_tool_calls` calls, the cell's next call is
///   refused, and the calls before it ran untouched;
/// * a step's group: each step starts its own count, so a second step of
///   `max_tool_calls` calls runs, and one of a call more is refused;
/// * a process's holding: a group it consumed is no longer held, so a second
///   group of `max_tool_calls` calls runs, and one of a call more is refused.
pub async fn tool_call_limit_staged_calls(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);
    let unit = producer.limit_unit;
    let producer = staged(&producer, LIMIT);

    if unit != ToolCallLimitUnit::Cell {
        // Twice the limit, half at a time: the count is not a total.
        let twice = plan("limittwice", &leaf_routes(2 * LIMIT));
        let observed = run_scenario(
            prefix,
            Arc::clone(&effect_host),
            &stores,
            &runner,
            &producer,
            &twice,
            limited(Schedule::SERIAL_SAFE),
            BTreeMap::new(),
        )
        .await;
        assert_eq!(
            sorted(observed.answered()),
            sorted(twice.tools()),
            "{context}: both groups of max_tool_calls calls run; the turn ended {:?}",
            observed.end,
        );
        assert_eq!(
            observed.refusals,
            Vec::<String>::new(),
            "{context}: two groups of max_tool_calls calls in sequence are not refused",
        );
    }

    // The first group fills the limit; what the second asks for is past it.
    let (rest, counted) = match unit {
        ToolCallLimitUnit::Cell => (1, LIMIT),
        ToolCallLimitUnit::Step | ToolCallLimitUnit::Process => (LIMIT + 1, 0),
    };
    let past = plan("limitstaged", &leaf_routes(LIMIT + rest));
    let observed = run_scenario(
        prefix,
        Arc::clone(&effect_host),
        &stores,
        &runner,
        &producer,
        &past,
        limited(Schedule::SERIAL_SAFE),
        BTreeMap::new(),
    )
    .await;
    assert_eq!(
        sorted(observed.answered()),
        sorted(past.tools()[..LIMIT].to_vec()),
        "{context}: the calls before the refused one run untouched, and no refused call \
         runs; the turn ended {:?}",
        observed.end,
    );
    assert_eq!(
        observed.refusals.first(),
        Some(&expected_refusal(unit, counted, rest)),
        "{context}: the model is shown the refusal at the call past the limit; the turn \
         ended {:?}",
        observed.end,
    );
}

/// A turn that crashes while its first group is still open is redriven, and
/// the redrive refuses the same call the same way: the calls the first group
/// made are counted once, however many executions formed the group, and the
/// refused calls run in neither execution.
///
/// The crash fires once every call of the first group but one has answered
/// and that one is in flight, so the redrive re-forms a group the journal
/// holds part of before it reaches the call past the limit.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn tool_call_limit_refuses_the_same_call_across_a_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);
    let unit = producer.limit_unit;
    let producer = staged(&producer, LIMIT);
    let (rest, counted) = match unit {
        ToolCallLimitUnit::Cell => (1, LIMIT),
        ToolCallLimitUnit::Step | ToolCallLimitUnit::Process => (LIMIT + 1, 0),
    };
    let plan = plan("limitcrash", &leaf_routes(LIMIT + rest));
    let leaves = plan.tools();
    let (first, refused) = leaves.split_at(LIMIT);
    let (settled, held) = first.split_at(LIMIT - 1);
    let held = held[0].clone();
    // The first group's calls answer at once, but for the held one: it waits
    // for a refused call to start, which none ever does, so it is in flight
    // until the redriving attempt releases it.
    let mut dependencies = settled
        .iter()
        .map(|leaf| (leaf.clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();
    dependencies.insert(held.clone(), vec![refused[0].clone()]);
    let schedule = limited(Schedule::GATED);

    let session_id =
        lash_sansio::SessionId::fixture(format!("{prefix}-{}-limit-crash", producer.label));
    let admitted = admit(crate::ExecutionScope::turn(
        &session_id,
        tool_batch_turn_id(&session_id),
    ));
    let state = Arc::new(ScenarioState::new(&plan, schedule, dependencies));
    let crash = crate::ConformanceCrash::new();
    let (observations, mut observation) = tokio::sync::mpsc::unbounded_channel();
    let attempt = |crashing: bool| -> crate::ConformanceTurnAttempt {
        let session_id = session_id.clone();
        let effect_host = Arc::clone(&effect_host);
        let stores = Arc::clone(&stores);
        let tier = Arc::clone(&runner);
        let producer = producer.clone();
        let plan = plan.clone();
        let state = Arc::clone(&state);
        let crash = crash.clone();
        let observations = observations.clone();
        Arc::new(move |turn_controller| {
            let session_id = session_id.clone();
            let effect_host = Arc::clone(&effect_host);
            let stores = Arc::clone(&stores);
            let tier = Arc::clone(&tier);
            let producer = producer.clone();
            let plan = plan.clone();
            let state = Arc::clone(&state);
            let crash = crash.clone();
            let observations = observations.clone();
            Box::pin(async move {
                if !crashing {
                    state.rendezvous.release();
                }
                let scenario = run_scenario_on_session(
                    session_id,
                    effect_host,
                    stores,
                    Some(&tier),
                    Some(turn_controller),
                    &producer,
                    &plan,
                    schedule,
                    state,
                );
                if crashing {
                    tokio::select! {
                        biased;
                        () = crash.fired() => panic!("the max_tool_calls crash turn crashes here"),
                        ended = scenario => panic!(
                            "the crashing turn ended ({:?}) before its crash fired",
                            ended.end
                        ),
                    }
                }
                let _ = observations.send(scenario.await);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    let fire = {
        let rendezvous = Arc::clone(&state.rendezvous);
        let settled = settled.to_vec();
        let held = held.clone();
        let crash = crash.clone();
        crate::task::spawn(async move {
            loop {
                let events = rendezvous.events();
                let answered =
                    |leaf: &String| events.contains(&RendezvousEvent::Answered(leaf.clone()));
                if settled.iter().all(answered)
                    && events.contains(&RendezvousEvent::Started(held.clone()))
                {
                    assert!(!answered(&held), "the held call is in flight at the crash");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            crash.fire();
        })
    };
    runner
        .run_crashed_then_redriven_turn(admitted, attempt(true), attempt(false))
        .await;
    fire.await.expect("the crash trigger's task");
    let observed = observation
        .recv()
        .await
        .expect("the redriven turn reports its observations");
    runner.scenario_finished().await;

    assert!(
        matches!(observed.end, ScenarioEnd::Finished { .. }),
        "{context}: the redriven turn finishes: {:?}",
        observed.end,
    );
    let started = observed.started();
    for leaf in refused {
        assert!(
            !started.contains(leaf),
            "{context}: refused call {leaf} ran in neither execution; log: {:?}",
            observed.events,
        );
    }
    assert!(
        observed.answered().contains(&held),
        "{context}: the call in flight at the crash settles in the redrive; log: {:?}",
        observed.events,
    );
    assert_eq!(
        observed.refusals.last(),
        Some(&expected_refusal(unit, counted, rest)),
        "{context}: the redrive refuses the same call, with the first group's calls \
         counted once; every refusal shown: {:?}",
        observed.refusals,
    );
}

/// The gate the holding law's race winner waits at.
const WINNER_GATE: &str = "the race's winner may answer";

/// A process that holds `max_tool_calls` calls is refused one more while it
/// holds them, and the calls it holds are counted once however many
/// executions of its segment formed their group.
///
/// The process races `max_tool_calls` calls and then issues one more. Every
/// raced call is in flight when the process's worker is killed; the tier
/// replays the segment, which forms the same group again. Only then does the
/// race's winner answer: the race settles, its losers stay in flight and the
/// process still holds all of them, so the one further call is refused, with
/// a count of exactly `max_tool_calls`. A group counted once per execution
/// would read twice that, or refuse the race itself.
///
/// That a process is admitted again once it has consumed what it held is
/// [`tool_call_limit_staged_calls`]'s second group.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn tool_call_limit_counts_what_a_process_holds_across_a_worker_kill(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: ToolBatchProducer,
) {
    let context = format!("{prefix}/{}", producer.label);
    let holding = producer
        .holding
        .clone()
        .expect("the registration macro hands this law holding producers only");
    let producer = ToolBatchProducer {
        script: Arc::new(move |plan| holding(plan, LIMIT)),
        ..producer
    };
    let plan = plan("limitheld", &leaf_routes(LIMIT + 1));
    let leaves = plan.tools();
    let (raced, refused) = leaves.split_at(LIMIT);
    let (winner, losers) = raced.split_first().expect("a race of at least one call");
    // The winner waits for the law's gate. The losers wait for the refused
    // call to start, which it never does: they are in flight for as long as
    // the process lives.
    let mut dependencies = losers
        .iter()
        .map(|leaf| (leaf.clone(), vec![refused[0].clone()]))
        .collect::<BTreeMap<_, _>>();
    dependencies.insert(winner.clone(), vec![WINNER_GATE.to_string()]);
    let schedule = limited(Schedule::GATED);
    let state = Arc::new(ScenarioState::new(&plan, schedule, dependencies));
    let session_id =
        lash_sansio::SessionId::fixture(format!("{prefix}-{}-limit-held", producer.label));

    let kill = {
        let rendezvous = Arc::clone(&state.rendezvous);
        let raced = raced.to_vec();
        let runner = Arc::clone(&runner);
        crate::task::spawn(async move {
            loop {
                let events = rendezvous.events();
                if raced
                    .iter()
                    .all(|leaf| events.contains(&RendezvousEvent::Started(leaf.clone())))
                {
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, RendezvousEvent::Answered(_))),
                        "every raced call is in flight at the kill: {events:?}"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let killed = runner.kill_process_workers().await;
            rendezvous.open_gate(WINNER_GATE);
            killed
        })
    };
    let admitted = admit(crate::ExecutionScope::turn(
        &session_id,
        tool_batch_turn_id(&session_id),
    ));
    let (observations, mut observation) = tokio::sync::mpsc::unbounded_channel();
    let attempt: crate::ConformanceTurnAttempt = {
        let tier = Arc::clone(&runner);
        let state = Arc::clone(&state);
        let plan = plan.clone();
        Arc::new(move |turn_controller| {
            let session_id = session_id.clone();
            let effect_host = Arc::clone(&effect_host);
            let stores = Arc::clone(&stores);
            let tier = Arc::clone(&tier);
            let producer = producer.clone();
            let plan = plan.clone();
            let state = Arc::clone(&state);
            let observations = observations.clone();
            Box::pin(async move {
                let observed = run_scenario_on_session(
                    session_id,
                    effect_host,
                    stores,
                    Some(&tier),
                    Some(turn_controller),
                    &producer,
                    &plan,
                    schedule,
                    state,
                )
                .await;
                let _ = observations.send(observed);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner.run_turn(admitted, attempt).await;
    // Every execution of the turn that ran to its end reports; the last one
    // read the whole scenario.
    let mut observed = observation
        .recv()
        .await
        .expect("the turn reports its observations");
    while let Ok(later) = observation.try_recv() {
        observed = later;
    }
    let killed = kill.await.expect("the worker kill's task");
    // The process ended; nothing waits for its losers any more.
    state.rendezvous.release();
    runner.scenario_finished().await;

    assert!(
        matches!(observed.end, ScenarioEnd::Finished { .. }),
        "{context}: the turn that started the process finishes: {:?}",
        observed.end,
    );
    assert_eq!(
        killed, 1,
        "{context}: the process's one segment was running its race when its worker was killed",
    );
    assert!(
        observed.answered().contains(winner),
        "{context}: the race's winner answers after the kill; log: {:?}",
        observed.events,
    );
    assert!(
        !observed.started().contains(&refused[0]),
        "{context}: the refused call ran in no execution; log: {:?}",
        observed.events,
    );
    for loser in losers {
        assert!(
            !observed.answered().contains(loser),
            "{context}: held call {loser} was still in flight at the refusal; log: {:?}",
            observed.events,
        );
    }
    assert_eq!(
        observed.refusals.last(),
        Some(&expected_refusal(ToolCallLimitUnit::Process, LIMIT, 1)),
        "{context}: the process is refused one call past the max_tool_calls it holds, \
         each held call counted once; every refusal shown: {:?}",
        observed.refusals,
    );
}
