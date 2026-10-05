//! Early aggregate selection, loser ownership and logical Closing laws.

use super::*;
use lash_core::facade_support::SystemClock;
use lash_core::tool_dispatch::RunAggregateOutcome;
use lash_core::tool_run::{AggregateConsumer, AggregateLeaf, AggregatePlan};

/// A second program tool completes while the main aggregate owes V1.
/// It enters through the coordinator's normal owned X and recorded D.
pub(super) struct ProgramRelease {
    pub(super) call_id: ToolCallId,
    ranked: [ToolCallId; 3],
    server: lash_restate_test::RestateTestServer,
}

impl ProgramRelease {
    pub(super) async fn run(&self, probe: &Probe) {
        loop {
            if probe
                .seen()
                .contains(&Seen::RealizeBegin(self.ranked[0].clone()))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !probe
                .seen()
                .contains(&Seen::RealizeBegin(self.ranked[2].clone())),
            "L18: higher declarations cannot bypass the blocked lower drain"
        );
        assert_eq!(
            probe
                .executions
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| self.ranked.contains(id))
                .map(|(id, _)| id.clone())
                .collect::<BTreeSet<_>>()
                .len(),
            3,
            "all bodies entered before rank 1 drains"
        );
        assert_eq!(
            probe.parallel_completed.load(Ordering::SeqCst),
            1,
            "rank 1 drains before either higher body can finish"
        );
        probe.gate_open.store(true, Ordering::SeqCst);
        probe.gate_wake.notify_waiters();
        // S10 keeps rank 1's protected callback held while
        // ranks 2 and 3 commit. Rank 2 seats at its intent-free
        // decision; rank 3 must still owe its declaration.
        loop {
            let decided: BTreeSet<_> = self
                .server
                .invocations()
                .iter()
                .flat_map(|view| self.server.journal(&view.id).unwrap())
                .filter_map(|entry| {
                    let Some(Ok(bytes)) = entry.run_completion() else {
                        return None;
                    };
                    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                    serde_json::from_value::<RunRecord>(value.get("record")?.clone()).ok()
                })
                .flat_map(|record| {
                    record.events.into_iter().filter_map(|event| match event {
                        RunEvent::Decided { call_id, .. } => Some(call_id),
                        _ => None,
                    })
                })
                .collect();
            if decided.contains(&self.ranked[1]) && decided.contains(&self.ranked[2]) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !probe
                .seen()
                .contains(&Seen::RealizeBegin(self.ranked[2].clone())),
            "rank 3 remains behind the blocked lower drain after its D"
        );
    }
}

/// FIG-4975 / L18: Promise.all must issue rank 1's protected declaration
/// while the higher bodies are held. The unrelated effect releases those
/// bodies only after that declaration begins; waiting for all X deadlocks.
#[tokio::test]
async fn l18_all_drains_a_committed_operand_before_the_remaining_bodies_finish() {
    let calls = Arc::new(vec![
        (
            call(
                "all-rank-one",
                &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        ),
        (call("all-rank-two", &Kind::IntentFree), Kind::IntentFree),
        (
            call(
                "all-rank-three",
                &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        ),
    ]);
    let ids: Vec<_> = calls.iter().map(|(call, _)| call.call_id.clone()).collect();
    let mut cuts = vec![
        Some(name(&ids[2], "declare")),
        Some(name(&ids[0], "declare")),
        None,
    ];
    while let Some(cut) = cuts.pop() {
        let program_call = call("all-unrelated-program", &Kind::IntentFree);
        let mut all_calls = calls.as_ref().clone();
        all_calls.push((program_call.clone(), Kind::IntentFree));
        let mut probe = Probe::new(&all_calls);
        probe.body_barrier = Some(Arc::new(tokio::sync::Barrier::new(3)));
        probe.parallel_order = ids.clone();
        probe.gate = Some((ids[1].clone(), ids[0].clone()));
        probe.held.insert(ids[0].clone());
        let backend = lash_restate_test::backend(4975, ServerConfig::default())
            .await
            .unwrap();
        let crashes = lash_restate_test::CrashCount::new();
        assert!(backend.server().on_crash(crashes.listener()));
        if let Some(cut) = &cut {
            backend
                .server()
                .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
                    name: Some(cut.clone()),
                }));
        }
        probe.program_release = Some(ProgramRelease {
            call_id: program_call.call_id.clone(),
            ranked: [ids[0].clone(), ids[1].clone(), ids[2].clone()],
            server: backend.server().clone(),
        });
        let probe = Arc::new(probe);
        let finished = Arc::new(Mutex::new(Vec::new()));
        let attempt: lash_restate_test::HandlerAttempt = {
            let probe = Arc::clone(&probe);
            let calls = Arc::clone(&calls);
            let finished = Arc::clone(&finished);
            Arc::new(move |scoped| {
                let probe = Arc::clone(&probe);
                let calls = Arc::clone(&calls);
                let finished = Arc::clone(&finished);
                let program_call = program_call.clone();
                Box::pin(async move {
                    let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                    let plan = aggregate_plan("protected-all", &round, vec![0, 1, 2]);
                    let mut run =
                        RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                    run.start_aggregate(
                        &plan,
                        &round,
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    let independent = aggregate_plan(
                        "independent-program-effect",
                        std::slice::from_ref(&program_call),
                        vec![0],
                    );
                    run.start_aggregate(
                        &independent,
                        std::slice::from_ref(&program_call),
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    let answer = run
                        .consume_aggregate(&plan.key, AggregateConsumer::All)
                        .await;
                    assert!(
                        matches!(answer.unwrap(), RunAggregateOutcome::AllResults(results) if results.len() == 3 && results.iter().all(Option::is_some))
                    );
                    assert!(
                        matches!(run.consume_aggregate(&independent.key, AggregateConsumer::All).await.unwrap(), RunAggregateOutcome::AllResults(results) if results.len() == 1)
                    );
                    run.close().await.unwrap();
                    finished.lock().unwrap().push(run.into_records());
                })
            })
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
        )
        .await
        .expect("L18: rank 1 must drain before Promise.all waits for the held higher bodies")
        .unwrap_or_else(|error| panic!("cut={cut:?}: {error}"));
        assert_eq!(
            crashes.get(),
            u64::from(cut.is_some()),
            "cut={cut:?}: the named boundary must execute"
        );
        if cut.is_none() {
            // Cut each V boundary selected by the recorded schedule. V has
            // no separate callback command which could serialize later D.
            for view in backend.server().invocations() {
                for entry in backend.server().journal(&view.id).unwrap() {
                    let Some(Ok(bytes)) = entry.run_completion() else {
                        continue;
                    };
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let Some(record) = value.get("record") else {
                        continue;
                    };
                    let record: RunRecord = serde_json::from_value(record.clone()).unwrap();
                    if record
                        .events
                        .iter()
                        .any(|event| matches!(event, RunEvent::Presented { .. }))
                    {
                        cuts.push(Some(format!("lash:run:schedule:{}", record.first.0)));
                    }
                }
            }
        }
        let finished = finished.lock().unwrap();
        let records = finished.last().expect("the aggregate completed");
        assert!(
            drain_violations(records, &BTreeSet::new(), None).is_empty(),
            "cut={cut:?}: the protected drain remains transitive"
        );
        for id in &ids {
            assert_eq!(
                probe.executions_of(id),
                if cut.as_ref() == Some(&name(&ids[0], "declare")) && *id != ids[0] {
                    2
                } else {
                    1
                },
                "cut={cut:?}: only the still-unrecorded higher bodies redeliver"
            );
        }
        assert_eq!(probe.realized.lock().unwrap().len(), 2);
        let events: Vec<_> = records.iter().flat_map(|record| &record.events).collect();
        let declaration = events.iter().position(|event| matches!(event, RunEvent::DeclarationsIssued { call_id } if *call_id == ids[0])).unwrap();
        let second = events.iter().position(|event| matches!(event, RunEvent::Decided { call_id, rank: 2, .. } if *call_id == ids[1])).unwrap();
        let settled = events.iter().position(|event| matches!(event, RunEvent::DeclarationsSettled { call_id } if *call_id == ids[0])).unwrap();
        let unrelated = events.iter().position(|event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == probe.program_release.as_ref().unwrap().call_id)).unwrap();
        assert!(
            second < unrelated && unrelated < settled,
            "the independent program tool completes and publishes D while V1 remains blocked"
        );
        assert!(
            declaration < second && second < settled,
            "rank 2 commits and seats while rank 1's declaration is blocked"
        );
    }
}

#[tokio::test]
async fn l18_protected_io_stays_live_after_every_x_ack_and_recovers_mid_v() {
    for crash in [false, true] {
        let calls = Arc::new(vec![(
            call(
                "slow-protected-io",
                &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
        )]);
        let id = calls[0].0.call_id.clone();
        let mut probe = Probe::new(&calls);
        probe.held_after_realization.insert(id.clone());
        let probe = Arc::new(probe);
        let backend = lash_restate_test::backend(
            4975,
            ServerConfig {
                inactivity_timeout: Duration::from_millis(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let crashes = lash_restate_test::CrashCount::new();
        assert!(backend.server().on_crash(crashes.listener()));
        let finished = Arc::new(Mutex::new(Vec::new()));
        let attempt: lash_restate_test::HandlerAttempt = {
            let calls = Arc::clone(&calls);
            let probe = Arc::clone(&probe);
            let finished = Arc::clone(&finished);
            Arc::new(move |scoped| {
                let calls = Arc::clone(&calls);
                let probe = Arc::clone(&probe);
                let finished = Arc::clone(&finished);
                Box::pin(async move {
                    probe.handler_attempts.fetch_add(1, Ordering::SeqCst);
                    let round = vec![calls[0].0.clone()];
                    let plan = aggregate_plan("slow-protected-all", &round, vec![0]);
                    let mut run =
                        RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                    run.start_aggregate(
                        &plan,
                        &round,
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    assert!(
                        matches!(run.consume_aggregate(&plan.key, AggregateConsumer::All).await.unwrap(), RunAggregateOutcome::AllResults(results) if results.len() == 1)
                    );
                    run.close().await.unwrap();
                    finished.lock().unwrap().push(run.into_records());
                })
            })
        };
        let server = backend.server().clone();
        let external_ack = async {
            let invocation = loop {
                if !probe.realized.lock().unwrap().is_empty()
                    && let Some(view) = server.invocations().into_iter().find(|view| {
                        server.journal(&view.id).unwrap().into_iter().any(|entry| {
                            entry.run_completion().and_then(Result::ok).and_then(|bytes| {
                                let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                                serde_json::from_value::<RunRecord>(value.get("record")?.clone()).ok()
                            }).is_some_and(|record| record.events.iter().any(|event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == id)))
                        })
                    })
                {
                    break view;
                }
                tokio::task::yield_now().await;
            };
            // No owned X or unrelated SDK callback is left to keep V alive.
            // Its external outcome exists, but the I/O acknowledgment is slow.
            let attempts = probe.handler_attempts.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            let view = server
                .invocations()
                .into_iter()
                .find(|view| view.id == invocation.id)
                .unwrap();
            assert_eq!(
                view.status, "running",
                "V must not suspend while external I/O is in flight"
            );
            assert_eq!(
                probe.handler_attempts.load(Ordering::SeqCst),
                attempts,
                "slow protected I/O must retain its owning attempt"
            );
            assert!(probe.presentations.lock().unwrap().is_empty());
            if crash {
                assert!(server.crash(&invocation.id));
                loop {
                    if probe
                        .seen()
                        .iter()
                        .filter(
                            |event| matches!(event, Seen::RealizeBegin(call_id) if *call_id == id),
                        )
                        .count()
                        == 2
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    probe.realized.lock().unwrap().len(),
                    1,
                    "the recorded intent recovers through its external outcome fence"
                );
            }
            probe.run_unrelated();
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            let (answer, ()) = tokio::join!(
                backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
                external_ack
            );
            answer.unwrap();
        })
        .await
        .expect("L18: SDK-owned protected I/O survives a slow acknowledgment and a mid-V crash");
        assert_eq!(crashes.get(), u64::from(crash));
        assert_eq!(probe.executions_of(&id), 1);
        assert_eq!(probe.realized.lock().unwrap().len(), 1);
        assert_eq!(probe.presentations.lock().unwrap().len(), 1);
        let finished = finished.lock().unwrap();
        assert!(drain_violations(finished.last().unwrap(), &BTreeSet::new(), None).is_empty());
    }
}

#[tokio::test]
async fn l06_race_returns_before_inline_loser_and_keeps_it_unconsumed() {
    let calls = Arc::new(vec![
        (
            call("aggregate-winner", &Kind::IntentFree),
            Kind::IntentFree,
        ),
        (call("aggregate-loser", &Kind::IntentFree), Kind::IntentFree),
    ]);
    let mut probe = Probe::new(&calls);
    probe.body_barrier = Some(Arc::new(tokio::sync::Barrier::new(2)));
    probe.gate = Some((calls[1].0.call_id.clone(), calls[0].0.call_id.clone()));
    let probe = Arc::new(probe);
    let returned = Arc::new(AtomicBool::new(false));
    let backend = lash_restate_test::backend(4882, ServerConfig::default())
        .await
        .unwrap();
    let attempt: lash_restate_test::HandlerAttempt =
        {
            let returned = Arc::clone(&returned);
            let probe = Arc::clone(&probe);
            Arc::new(move |scoped| {
                let calls = Arc::clone(&calls);
                let probe = Arc::clone(&probe);
                let returned = Arc::clone(&returned);
                Box::pin(async move {
                    let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                    let mut run =
                        RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                    let plan = aggregate_plan("early-race", &round, vec![0, 1]);
                    run.start_aggregate(
                        &plan,
                        &round,
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    let selected = run
                        .consume_aggregate(&plan.key, AggregateConsumer::Race)
                        .await
                        .unwrap();
                    assert!(matches!(
                        selected,
                        RunAggregateOutcome::Selected {
                            operand: 0,
                            fulfilled: true,
                            reply: Some(_)
                        }
                    ));
                    assert_eq!(run.lifecycle(), RunLifecycle::Live);
                    assert!(!probe.gate_open.load(Ordering::SeqCst));
                    assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                    run.beside(scoped.controller().record_run_record(
                        UNRELATED.to_owned(),
                        unrelated_record(Arc::clone(&probe)),
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                    assert!(probe.unrelated.load(Ordering::SeqCst));
                    returned.store(true, Ordering::SeqCst);
                    probe.gate_open.store(true, Ordering::SeqCst);
                    probe.gate_wake.notify_waiters();
                    run.progress().await.unwrap();
                    run.drain_protected().await.unwrap();
                    let consumed: Vec<_> = run
                        .records()
                        .iter()
                        .flat_map(|record| &record.events)
                        .filter_map(|event| match event {
                            RunEvent::Consumed { call_id } => Some(call_id.clone()),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(consumed, vec![round[0].call_id.clone()]);
                    assert!(
                        probe
                            .presentations
                            .lock()
                            .unwrap()
                            .contains(&round[1].call_id)
                    );
                    run.close().await.unwrap();
                    assert_eq!(run.lifecycle(), RunLifecycle::Settled);
                })
            })
        };
    backend
        .run_in_handler(AdmittedScope::turn("session", "turn"), attempt)
        .await
        .unwrap();
    assert!(
        returned.load(Ordering::SeqCst),
        "L06: an early winner must return while its inline loser remains live"
    );
}

#[tokio::test]
async fn l05_check_cancel_is_an_operand_rejection_before_and_after() {
    for before in [true, false] {
        for consumer in [
            AggregateConsumer::Race,
            AggregateConsumer::Any,
            AggregateConsumer::All,
            AggregateConsumer::AllSettled,
            AggregateConsumer::ListBatch,
        ] {
            let calls = Arc::new(vec![
                (call("check-allowed", &Kind::IntentFree), Kind::IntentFree),
                (
                    {
                        let mut call = call("check-cancelled", &Kind::IntentFree);
                        call.cancel = ExternalCancelPolicy::CancelExternalWork;
                        call
                    },
                    Kind::IntentFree,
                ),
            ]);
            let mut probe = Probe::new(&calls);
            if before {
                probe.cancel_before = Some(calls[1].0.call_id.clone());
            } else {
                probe.cancel_after = Some(calls[1].0.call_id.clone());
                probe.parallel = Some(Arc::new(tokio::sync::Barrier::new(2)));
                probe.parallel_order = vec![calls[1].0.call_id.clone(), calls[0].0.call_id.clone()];
            }
            let probe = Arc::new(probe);
            let backend = lash_restate_test::backend(4925, ServerConfig::default())
                .await
                .unwrap();
            let finished = Arc::new(AtomicBool::new(false));
            let attempt: lash_restate_test::HandlerAttempt = {
                let probe = Arc::clone(&probe);
                let finished = Arc::clone(&finished);
                Arc::new(move |scoped| {
                    let calls = Arc::clone(&calls);
                    let probe = Arc::clone(&probe);
                    let finished = Arc::clone(&finished);
                    Box::pin(async move {
                        let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                        let plan = aggregate_plan("check-cancel", &round, vec![0, 1, 1]);
                        let mut run = RunCoordinator::open(
                            &scoped,
                            owner(),
                            SegmentOrdinal(0),
                            vec![revision()],
                        );
                        run.start_aggregate(
                            &plan,
                            &round,
                            lash_core::tool_run::CapacityScope::Held,
                            Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                            Default::default(),
                            &SystemClock,
                        )
                        .await
                        .unwrap();
                        let answer = run.consume_aggregate(&plan.key, consumer).await.unwrap();
                        let rejected = |reply: &Option<SingletonTerminal>| {
                            matches!(
                                reply,
                                Some(SingletonTerminal::Withheld {
                                    decision: CallDecision::CheckCancelled
                                })
                            )
                        };
                        match (consumer, &answer) {
                            (
                                AggregateConsumer::Any,
                                RunAggregateOutcome::Selected {
                                    operand: 0,
                                    fulfilled: true,
                                    reply: Some(SingletonTerminal::Final { .. }),
                                },
                            ) => {}
                            (
                                AggregateConsumer::Race
                                | AggregateConsumer::All
                                | AggregateConsumer::ListBatch,
                                RunAggregateOutcome::Selected {
                                    operand: 1,
                                    fulfilled: false,
                                    reply,
                                },
                            ) if rejected(reply) => {}
                            (
                                AggregateConsumer::AllSettled,
                                RunAggregateOutcome::AllResults(replies),
                            ) if replies.len() == 3
                                && rejected(&replies[1])
                                && replies[1] == replies[2] => {}
                            _ => panic!(
                                "L05: check cancellation with before={before} is a call rejection for {consumer:?}: {answer:?}"
                            ),
                        }
                        while run.progress().await.unwrap().is_some() {}
                        let decision = run
                            .records()
                            .iter()
                            .flat_map(|record| &record.events)
                            .find_map(|event| match event {
                                RunEvent::Decided {
                                    call_id,
                                    decision,
                                    after,
                                    ..
                                } if *call_id == round[1].call_id => Some((decision, after)),
                                _ => None,
                            })
                            .unwrap();
                        assert_eq!(*decision.0, CallDecision::CheckCancelled);
                        if before {
                            assert!(run.records().iter().flat_map(|record| &record.events).any(|event| match event {
                                RunEvent::Admitted { round } => round.members.iter().any(|member| matches!(member.checks.winner().map(|reply| &reply.verdict), Some(lash_core::tool_run::BeforeCheckVerdict::Cancel { cause }) if *cause == check_cancel_cause())),
                                _ => false,
                            }));
                        } else {
                            assert!(
                                matches!(decision.1.as_ref().unwrap().winner().map(|reply| &reply.verdict), Some(AfterCheckVerdict::Cancel { cause }) if *cause == check_cancel_cause())
                            );
                        }
                        assert_eq!(run.lifecycle(), RunLifecycle::Live);
                        assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                        run.close().await.unwrap();
                        finished.store(true, Ordering::SeqCst);
                    })
                })
            };
            tokio::time::timeout(
                Duration::from_secs(10),
                backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(finished.load(Ordering::SeqCst));
            assert!(probe.cancelled_calls.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn l05_check_cancel_never_replaces_an_earlier_winner() {
    for before in [true, false] {
        for consumer in [AggregateConsumer::Race, AggregateConsumer::Any] {
            let winner = if before {
                Kind::Cached
            } else {
                Kind::IntentFree
            };
            let calls = Arc::new(vec![
                (call("earlier-winner", &winner), winner),
                (
                    call("later-check-cancel", &Kind::IntentFree),
                    Kind::IntentFree,
                ),
            ]);
            let mut probe = Probe::new(&calls);
            if before {
                probe.cancel_before = Some(calls[1].0.call_id.clone());
            } else {
                probe.cancel_after = Some(calls[1].0.call_id.clone());
                probe.body_barrier = Some(Arc::new(tokio::sync::Barrier::new(2)));
                probe.gate = Some((calls[1].0.call_id.clone(), calls[0].0.call_id.clone()));
            }
            let probe = Arc::new(probe);
            let backend = lash_restate_test::backend(0x492505, ServerConfig::default())
                .await
                .unwrap();
            let finished = Arc::new(AtomicBool::new(false));
            let attempt: lash_restate_test::HandlerAttempt = {
                let probe = Arc::clone(&probe);
                let finished = Arc::clone(&finished);
                Arc::new(move |scoped| {
                    let calls = Arc::clone(&calls);
                    let probe = Arc::clone(&probe);
                    let finished = Arc::clone(&finished);
                    Box::pin(async move {
                        let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                        let plan = aggregate_plan("earlier-winner", &round, vec![0, 1]);
                        let mut run = RunCoordinator::open(
                            &scoped,
                            owner(),
                            SegmentOrdinal(0),
                            vec![revision()],
                        );
                        run.start_aggregate(
                            &plan,
                            &round,
                            lash_core::tool_run::CapacityScope::Held,
                            Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                            Default::default(),
                            &SystemClock,
                        )
                        .await
                        .unwrap();
                        let first = run.consume_aggregate(&plan.key, consumer).await.unwrap();
                        assert!(
                            matches!(
                                first,
                                RunAggregateOutcome::Selected {
                                    operand: 0,
                                    fulfilled: true,
                                    ..
                                }
                            ),
                            "L05: the earlier winner survives the check cancellation: {first:?}"
                        );
                        probe.gate_open.store(true, Ordering::SeqCst);
                        probe.gate_wake.notify_waiters();
                        while run.progress().await.unwrap().is_some() {}
                        run.drain_protected().await.unwrap();
                        let again = run.consume_aggregate(&plan.key, consumer).await.unwrap();
                        assert_eq!(
                            again, first,
                            "L05: a later check-cancel never discards a selected winner"
                        );
                        let consumed: Vec<_> = run
                            .records()
                            .iter()
                            .flat_map(|record| &record.events)
                            .filter_map(|event| match event {
                                RunEvent::Consumed { call_id } => Some(call_id.clone()),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(consumed, vec![round[0].call_id.clone()]);
                        assert_eq!(run.lifecycle(), RunLifecycle::Live);
                        run.close().await.unwrap();
                        finished.store(true, Ordering::SeqCst);
                    })
                })
            };
            tokio::time::timeout(
                Duration::from_secs(10),
                backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(finished.load(Ordering::SeqCst));
        }
    }
}

pub(super) fn aggregate_plan(
    key: &str,
    calls: &[SingletonToolCall],
    operands: Vec<u32>,
) -> AggregatePlan {
    AggregatePlan {
        key: key.to_owned(),
        leaves: calls
            .iter()
            .map(|call| AggregateLeaf::Call {
                call_id: call.call_id.clone(),
            })
            .collect(),
        operands,
    }
}

/// A consumer case owns only its returned values; the logical owner closes
/// the other calls after the case's assertions have observed them live.
async fn aggregate_case(
    consumer: AggregateConsumer,
    kinds: Vec<Kind>,
    order: Vec<usize>,
    operands: Vec<u32>,
) -> (RunAggregateOutcome, Vec<RunRecord>, Arc<Probe>) {
    let calls = Arc::new(
        kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| (call(&format!("case-{index}"), &kind), kind))
            .collect::<Vec<_>>(),
    );
    let mut probe = Probe::new(&calls);
    if !order.is_empty() {
        probe.parallel = Some(Arc::new(tokio::sync::Barrier::new(calls.len())));
        probe.parallel_order = order
            .iter()
            .map(|index| calls[*index].0.call_id.clone())
            .collect();
    }
    let probe = Arc::new(probe);
    let results = Arc::new(Mutex::new(Vec::new()));
    let backend = lash_restate_test::backend(4882, ServerConfig::default())
        .await
        .unwrap();
    let attempt: lash_restate_test::HandlerAttempt = {
        let probe = Arc::clone(&probe);
        let results = Arc::clone(&results);
        Arc::new(move |scoped| {
            let calls = Arc::clone(&calls);
            let probe = Arc::clone(&probe);
            let results = Arc::clone(&results);
            let operands = operands.clone();
            Box::pin(async move {
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let plan = aggregate_plan("case", &round, operands);
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_aggregate(
                    &plan,
                    &round,
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                let answer = run.consume_aggregate(&plan.key, consumer).await.unwrap();
                // Let any unconsumed sibling finish naturally before Closing.
                // The ordering probe waits for after_checks, so closing it
                // early would intentionally skip that callback and its gate.
                while run.progress().await.unwrap().is_some() {}
                run.close().await.unwrap();
                results.lock().unwrap().push((answer, run.into_records()));
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    let (answer, records) = results.lock().unwrap().pop().unwrap();
    (answer, records, probe)
}

#[tokio::test]
async fn l05_modes_preserve_aliases_source_order_and_distinct_rejections() {
    for (consumer, expected) in [
        (AggregateConsumer::Race, Some((2, false))),
        (AggregateConsumer::Any, Some((1, true))),
        (AggregateConsumer::All, Some((2, false))),
        (AggregateConsumer::AllSettled, None),
        (AggregateConsumer::ListBatch, Some((0, false))),
    ] {
        let (answer, records, probe) = aggregate_case(
            consumer,
            vec![Kind::Failed, Kind::IntentFree, Kind::Failed],
            vec![2, 1, 0],
            vec![0, 1, 2, 1],
        )
        .await;
        match (expected, answer) {
            (
                Some((operand, fulfilled)),
                RunAggregateOutcome::Selected {
                    operand: found,
                    fulfilled: success,
                    reply: Some(_),
                },
            ) => {
                assert_eq!((found, success), (operand, fulfilled));
            }
            (None, RunAggregateOutcome::AllResults(results)) => {
                assert_eq!(results.len(), 4);
                assert_eq!(
                    results[1], results[3],
                    "duplicate aliases return the same unique call"
                );
                for (index, result) in results.iter().enumerate() {
                    let Some(SingletonTerminal::Final { capture, .. }) = result else {
                        panic!("allSettled returns each real terminal");
                    };
                    assert_eq!(
                        matches!(capture, SingletonCapture::Done { .. }),
                        index == 1 || index == 3
                    );
                }
            }
            (_, answer) => panic!("wrong {consumer:?} answer: {answer:?}"),
        }
        assert_eq!(probe.executions.lock().unwrap().len(), 3);
        let consumed: BTreeSet<_> = records
            .iter()
            .flat_map(|record| &record.events)
            .filter_map(|event| match event {
                RunEvent::Consumed { call_id } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        let expected = if matches!(
            consumer,
            AggregateConsumer::AllSettled | AggregateConsumer::ListBatch
        ) {
            3
        } else {
            1
        };
        assert_eq!(consumed.len(), expected);
    }
    let (answer, _, _) = aggregate_case(
        AggregateConsumer::Any,
        vec![Kind::Failed, Kind::Failed],
        vec![1, 0],
        vec![0, 1, 0],
    )
    .await;
    let RunAggregateOutcome::ExhaustedRejections(rejections) = answer else {
        panic!("any exhausts every rejection");
    };
    assert_eq!(rejections.len(), 3);
    assert_eq!(rejections[0], rejections[2]);
    let (answer, _, _) = aggregate_case(
        AggregateConsumer::All,
        vec![Kind::IntentFree],
        vec![],
        vec![0, 0],
    )
    .await;
    assert!(
        matches!(answer, RunAggregateOutcome::AllResults(results) if results.len() == 2 && results[0] == results[1])
    );
}

#[tokio::test]
async fn l05_empty_immediate_prefix_and_timers_admit_pending_siblings() {
    let backend = lash_restate_test::backend(0x488205, ServerConfig::default())
        .await
        .unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let attempt: lash_restate_test::HandlerAttempt = {
        let finished = Arc::clone(&finished);
        Arc::new(move |scoped| {
            let finished = Arc::clone(&finished);
            Box::pin(async move {
                let calls = vec![(call("prefix-pending", &Kind::Deferred), Kind::Deferred)];
                let probe = Arc::new(Probe::new(&calls));
                let handlers = Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>;
                let round = vec![calls[0].0.clone()];
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                for (index, consumer) in [
                    AggregateConsumer::Race,
                    AggregateConsumer::Any,
                    AggregateConsumer::All,
                    AggregateConsumer::AllSettled,
                    AggregateConsumer::ListBatch,
                ]
                .into_iter()
                .enumerate()
                {
                    let empty = AggregatePlan {
                        key: format!("empty-{index}"),
                        leaves: vec![],
                        operands: vec![],
                    };
                    run.start_aggregate(
                        &empty,
                        &[],
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&handlers),
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    let answer = run.consume_aggregate(&empty.key, consumer).await.unwrap();
                    assert!(match (consumer, answer) {
                        (AggregateConsumer::Race, RunAggregateOutcome::Pending)
                        | (AggregateConsumer::Any, RunAggregateOutcome::ExhaustedRejections(_)) =>
                            true,
                        (_, RunAggregateOutcome::AllResults(values)) => values.is_empty(),
                        _ => false,
                    });
                }
                let immediate = AggregatePlan {
                    key: "immediate-only".to_owned(),
                    leaves: vec![
                        AggregateLeaf::Settled { fulfilled: false },
                        AggregateLeaf::Settled { fulfilled: true },
                    ],
                    operands: vec![0, 1, 1],
                };
                run.start_aggregate(
                    &immediate,
                    &[],
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::clone(&handlers),
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                for (consumer, expected) in [
                    (AggregateConsumer::Race, Some((0, false))),
                    (AggregateConsumer::Any, Some((1, true))),
                    (AggregateConsumer::All, Some((0, false))),
                    (AggregateConsumer::AllSettled, None),
                    (AggregateConsumer::ListBatch, Some((0, false))),
                ] {
                    let answer = run
                        .consume_aggregate(&immediate.key, consumer)
                        .await
                        .unwrap();
                    match (expected, answer) {
                        (
                            Some((operand, fulfilled)),
                            RunAggregateOutcome::Selected {
                                operand: selected,
                                fulfilled: success,
                                reply: None,
                            },
                        ) => assert_eq!((selected, success), (operand, fulfilled)),
                        (None, RunAggregateOutcome::AllResults(values)) => {
                            assert_eq!(values, vec![None, None, None]);
                        }
                        answer => panic!("immediate-only consumer outcome: {answer:?}"),
                    }
                }
                let prefix = AggregatePlan {
                    key: "prefix".to_owned(),
                    leaves: vec![
                        AggregateLeaf::Settled { fulfilled: false },
                        AggregateLeaf::Call {
                            call_id: round[0].call_id.clone(),
                        },
                        AggregateLeaf::Settled { fulfilled: true },
                        AggregateLeaf::Timer { duration_ms: 0 },
                    ],
                    operands: vec![0, 1, 2, 2, 3],
                };
                run.start_aggregate(
                    &prefix,
                    &round,
                    lash_core::tool_run::CapacityScope::Held,
                    handlers,
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                assert!(run.records().iter().flat_map(|record| &record.events).any(|event| matches!(event, RunEvent::Admitted { round } if round.members.iter().any(|member| member.call_id == calls[0].0.call_id))));
                assert!(matches!(
                    run.consume_aggregate("prefix", AggregateConsumer::Race)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Selected {
                        operand: 0,
                        fulfilled: false,
                        reply: None
                    }
                ));
                assert!(matches!(
                    run.consume_aggregate("prefix", AggregateConsumer::Any)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Selected {
                        operand: 2,
                        fulfilled: true,
                        reply: None
                    }
                ));
                while run.progress().await.unwrap().is_some() {}
                let timer = AggregatePlan {
                    key: "timer-only".to_owned(),
                    leaves: vec![AggregateLeaf::Timer { duration_ms: 0 }],
                    operands: vec![0, 0],
                };
                run.start_aggregate(
                    &timer,
                    &[],
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                assert!(
                    matches!(run.consume_aggregate("timer-only", AggregateConsumer::AllSettled).await.unwrap(), RunAggregateOutcome::AllResults(values) if values == vec![None, None])
                );
                assert!(
                    matches!(
                        run.consume_aggregate("prefix", AggregateConsumer::AllSettled)
                            .await
                            .unwrap(),
                        RunAggregateOutcome::Pending
                    ),
                    "a Deferred descriptor cannot synthesize a settled value"
                );
                run.close().await.unwrap();
                assert!(
                    probe.cancelled_calls.lock().unwrap().is_empty(),
                    "Ignore preserves external work at Closing"
                );
                finished.store(true, Ordering::SeqCst);
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn l03_l04_only_logical_closing_cancels_and_accepted_finals_still_drain() {
    for protected in [false, true] {
        let winner_kind = Kind::IntentFree;
        let loser_kind = if protected {
            Kind::Declares(vec![ToolIntentKind::EmitTrigger])
        } else {
            Kind::IntentFree
        };
        let mut loser = call("closing-loser", &loser_kind);
        loser.cancel = ExternalCancelPolicy::CancelExternalWork;
        let calls = Arc::new(vec![
            (call("closing-winner", &winner_kind), winner_kind),
            (loser, loser_kind),
        ]);
        let mut probe = Probe::new(&calls);
        probe.body_barrier = Some(Arc::new(tokio::sync::Barrier::new(2)));
        probe.gate = Some((calls[1].0.call_id.clone(), calls[0].0.call_id.clone()));
        let probe = Arc::new(probe);
        let records = Arc::new(Mutex::new(Vec::new()));
        let backend = lash_restate_test::backend(0x488203, ServerConfig::default())
            .await
            .unwrap();
        let attempt: lash_restate_test::HandlerAttempt = {
            let calls = Arc::clone(&calls);
            let records = Arc::clone(&records);
            let probe = Arc::clone(&probe);
            Arc::new(move |scoped| {
                let calls = Arc::clone(&calls);
                let records = Arc::clone(&records);
                let probe = Arc::clone(&probe);
                Box::pin(async move {
                    let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                    let plan = aggregate_plan("closing", &round, vec![0, 1]);
                    let mut run =
                        RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                    run.start_aggregate(
                        &plan,
                        &round,
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    assert!(matches!(
                        run.consume_aggregate(&plan.key, AggregateConsumer::Race)
                            .await
                            .unwrap(),
                        RunAggregateOutcome::Selected { operand: 0, .. }
                    ));
                    assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                    assert_eq!(run.lifecycle(), RunLifecycle::Live);
                    if protected {
                        probe.gate_open.store(true, Ordering::SeqCst);
                        probe.gate_wake.notify_waiters();
                        assert!(matches!(
                            run.progress().await.unwrap(),
                            Some((
                                _,
                                DecidedCall::Ranked {
                                    decision: CallDecision::Final { declares: true, .. },
                                    ..
                                }
                            ))
                        ));
                        probe.cancel.store(true, Ordering::SeqCst);
                    }
                    run.close().await.unwrap();
                    assert_eq!(run.lifecycle(), RunLifecycle::Settled);
                    records.lock().unwrap().extend(run.into_records());
                })
            })
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
        )
        .await
        .unwrap()
        .unwrap();
        let events: Vec<_> = records
            .lock()
            .unwrap()
            .iter()
            .flat_map(|record| record.events.clone())
            .collect();
        let closing = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RunEvent::Lifecycle {
                        state: RunLifecycle::Closing
                    }
                )
            })
            .unwrap();
        if protected {
            assert!(probe.cancelled_calls.lock().unwrap().is_empty());
            assert_eq!(
                probe.realized.lock().unwrap().as_slice(),
                &[(calls[1].0.call_id.clone(), ToolIntentKind::EmitTrigger)]
            );
            assert!(events.iter().skip(closing).any(|event| matches!(event, RunEvent::DeclarationsSettled { call_id } if *call_id == calls[1].0.call_id)));
        } else {
            assert_eq!(
                probe.cancelled_calls.lock().unwrap().as_slice(),
                &[calls[1].0.call_id.clone()]
            );
            assert!(
                events
                    .iter()
                    .skip(closing)
                    .any(|event| matches!(event, RunEvent::CancelDischarged { .. }))
            );
            assert!(events.iter().any(|event| matches!(event, RunEvent::Decided { call_id, decision: CallDecision::Cancelled, .. } if *call_id == calls[1].0.call_id)));
            assert!(probe.realized.lock().unwrap().is_empty());
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEvent::Consumed { .. }))
                .count(),
            1
        );
        let mut ledger = lash_core::tool_run::RunLedger::new(owner());
        for record in records.lock().unwrap().iter() {
            ledger.append(SegmentOrdinal(0), record).unwrap();
        }
        assert_eq!(ledger.lifecycle(), RunLifecycle::Settled);
    }
    for resolved in [false, true] {
        closing_deferred_seal_case(resolved).await;
    }
}

/// Closing must use the immutable source answer: it cannot replace a result
/// that won the source seal with a local cancellation decision.
async fn closing_deferred_seal_case(resolved: bool) {
    use lash_core::tool_run::{
        MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole, SealWriter,
        SourceSeal,
    };
    let calls = Arc::new(vec![(
        call("closing-source", &Kind::Deferred),
        Kind::Deferred,
    )]);
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let mut probe = Probe::new(&calls);
    probe.materials = Some(stores.process_env_store());
    let probe = Arc::new(probe);
    let backend = lash_restate_test::backend(0x488204, ServerConfig::default())
        .await
        .unwrap();
    let ingress = backend.ingress();
    let records = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let probe = Arc::clone(&probe);
        let records = Arc::clone(&records);
        Arc::new(move |scoped| {
            let calls = Arc::clone(&calls);
            let probe = Arc::clone(&probe);
            let records = Arc::clone(&records);
            let ingress = ingress.clone();
            Box::pin(async move {
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let plan = aggregate_plan("closing-source", &round, vec![0]);
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_aggregate(
                    &plan,
                    &round,
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                assert!(matches!(
                    run.consume_aggregate(&plan.key, AggregateConsumer::Race)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Pending
                ));
                if resolved {
                    let source = probe.sources.lock().unwrap()[&round[0].call_id].clone();
                    let capture = SingletonCapture::Done {
                        output: output_of(&round[0].call_id),
                        commands: Vec::new(),
                        intents: Vec::new(),
                        stream: Default::default(),
                        start: None,
                    };
                    let bundle = MaterialBundle::of([MaterialPayload::new(
                        MaterialOwner::Source {
                            source: source.clone(),
                        },
                        MaterialRole::AttemptOutput,
                        Some(revision()),
                        serde_json::to_string(&capture).unwrap(),
                    )])
                    .unwrap()
                    .unwrap();
                    let retained = probe
                        .materials
                        .as_ref()
                        .unwrap()
                        .retain_material(
                            &MaterialHolder::Source {
                                source: source.clone(),
                            },
                            &bundle,
                        )
                        .await
                        .unwrap();
                    let reply: crate::Reply<crate::durable_wait::RestateSourceSealReply> = ingress
                        .call_object_json(
                            "LashDurableWaitIndex",
                            "session",
                            "seal_source",
                            &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                                source,
                                writer: SealWriter::External,
                                seal: SourceSeal::Resolved {
                                    result: Box::new(retained.references[0].clone()),
                                },
                            }),
                        )
                        .await
                        .unwrap();
                    assert!(matches!(
                        reply.into_body(),
                        crate::durable_wait::RestateSourceSealReply::Outcome { .. }
                    ));
                }
                run.close().await.unwrap();
                assert_eq!(run.lifecycle(), RunLifecycle::Settled);
                records.lock().unwrap().extend(run.into_records());
            })
        })
    };
    backend
        .run_in_handler(AdmittedScope::turn("session", "turn"), attempt)
        .await
        .unwrap();
    let records = records.lock().unwrap();
    let events: Vec<_> = records.iter().flat_map(|record| &record.events).collect();
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::Decided {
            decision: CallDecision::Final {
                source: lash_core::tool_run::ResultSource::DeferredCompletion { .. },
                ..
            },
            ..
        }
    ) == resolved));
    assert!(
        events.iter().any(|event| matches!(
            event,
            RunEvent::Decided {
                decision: CallDecision::Cancelled,
                ..
            }
        )) == !resolved
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RunEvent::Consumed { .. }))
    );
    assert_eq!(
        probe.presentations.lock().unwrap().len(),
        usize::from(resolved)
    );
    assert!(probe.cancelled_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn l06_l16_worker_loss_recovers_the_loser_without_closing_or_consuming_it() {
    let calls = Arc::new(vec![
        (call("replay-winner", &Kind::IntentFree), Kind::IntentFree),
        (call("replay-loser", &Kind::IntentFree), Kind::IntentFree),
    ]);
    let mut probe = Probe::new(&calls);
    probe.body_barrier = Some(Arc::new(tokio::sync::Barrier::new(2)));
    probe.gate = Some((calls[1].0.call_id.clone(), calls[0].0.call_id.clone()));
    let probe = Arc::new(probe);
    let backend = lash_restate_test::backend(0x488206, ServerConfig::default())
        .await
        .unwrap();
    backend
        .server()
        .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some(UNRELATED.to_owned()),
        }));
    let crashes = lash_restate_test::CrashCount::new();
    assert!(backend.server().on_crash(crashes.listener()));
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt =
        {
            let probe = Arc::clone(&probe);
            let snapshots = Arc::clone(&snapshots);
            Arc::new(move |scoped| {
                let calls = Arc::clone(&calls);
                let probe = Arc::clone(&probe);
                let snapshots = Arc::clone(&snapshots);
                Box::pin(async move {
                    let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                    let plan = aggregate_plan("replay", &round, vec![0, 1]);
                    let mut run =
                        RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                    run.start_aggregate(
                        &plan,
                        &round,
                        lash_core::tool_run::CapacityScope::Held,
                        Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    assert!(matches!(
                        run.consume_aggregate(&plan.key, AggregateConsumer::Race)
                            .await
                            .unwrap(),
                        RunAggregateOutcome::Selected { operand: 0, .. }
                    ));
                    run.beside(scoped.controller().record_run_record(
                        UNRELATED.to_owned(),
                        unrelated_record(Arc::clone(&probe)),
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                    assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                    assert_eq!(
                        run.request_cut(lash_core::BoundaryReason::JournalBudget)
                            .phase,
                        lash_core::tool_run::CutPhase::Quiescing
                    );
                    assert!(matches!(
                        run.capture_cut(),
                        Err(lash_core::tool_dispatch::RunCutRefusal::NotQuiescent)
                    ));
                    probe.gate_open.store(true, Ordering::SeqCst);
                    probe.gate_wake.notify_waiters();
                    let snapshot = run.quiesce().await.unwrap();
                    assert_eq!(run.lifecycle(), RunLifecycle::Live);
                    snapshots.lock().unwrap().push(snapshot);
                })
            })
        };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(crashes.get(), 1);
    assert_eq!(
        probe.executions_of(&ToolCallId::fixture("replay-winner")),
        1
    );
    assert_eq!(
        probe.executions_of(&ToolCallId::fixture("replay-loser")),
        2,
        "only the unfinished X redelivers after worker loss"
    );
    let snapshots = snapshots.lock().unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].attempts.len(), 2);
    let events: Vec<_> = snapshots[0]
        .entries
        .iter()
        .flat_map(|entry| &entry.record.events)
        .collect();
    assert!(!events.iter().any(|event| matches!(
        event,
        RunEvent::Lifecycle { .. } | RunEvent::CancelDischarged { .. }
    )));
    let consumed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RunEvent::Consumed { call_id } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(consumed, vec![ToolCallId::fixture("replay-winner")]);
}

#[tokio::test]
async fn l05_timer_replay_keeps_the_recorded_admission_instant_and_wake() {
    let backend = lash_restate_test::backend(0x488215, ServerConfig::default())
        .await
        .unwrap();
    backend
        .server()
        .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some("lash:run:schedule:2".to_owned()),
        }));
    let crashes = lash_restate_test::CrashCount::new();
    assert!(backend.server().on_crash(crashes.listener()));
    let instants = Arc::new(Mutex::new(Vec::new()));
    let finished = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let instants = Arc::clone(&instants);
        let finished = Arc::clone(&finished);
        Arc::new(move |scoped| {
            let instants = Arc::clone(&instants);
            let finished = Arc::clone(&finished);
            Box::pin(async move {
                let plan = AggregatePlan {
                    key: "clock".to_owned(),
                    leaves: vec![AggregateLeaf::Timer { duration_ms: 1 }],
                    operands: vec![0],
                };
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_aggregate(
                    &plan,
                    &[],
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::new(Probe::new(&[])),
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                let instant = run
                    .records()
                    .iter()
                    .flat_map(|record| &record.events)
                    .find_map(|event| match event {
                        RunEvent::AggregateAdmitted { admitted_at_ms, .. } => Some(*admitted_at_ms),
                        _ => None,
                    })
                    .unwrap();
                instants.lock().unwrap().push(instant);
                assert!(matches!(
                    run.consume_aggregate(&plan.key, AggregateConsumer::Race)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Selected {
                        operand: 0,
                        fulfilled: true,
                        reply: None
                    }
                ));
                run.close().await.unwrap();
                finished.lock().unwrap().push(run.into_records());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(crashes.get(), 1);
    let instants = instants.lock().unwrap();
    assert_eq!(instants.len(), 2);
    assert_eq!(
        instants[0], instants[1],
        "recovery never starts a fresh timer duration"
    );
    let finished = finished.lock().unwrap();
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0]
            .iter()
            .flat_map(|record| &record.events)
            .filter(|event| matches!(event, RunEvent::TimerElapsed { .. }))
            .count(),
        1
    );
}

/// L05/L06/L09: an already admitted effect, timer and aliases use the same
/// recorded terminal order. An early timer leaves the effect owned, a program
/// effect progresses beside it, and a physical cut retains Live ownership.
#[tokio::test]
async fn l05_l06_l09_generic_timer_and_admitted_handles_share_the_run() {
    let calls = Arc::new(vec![(
        call("generic-loser", &Kind::IntentFree),
        Kind::IntentFree,
    )]);
    let mut probe = Probe::new(&calls);
    probe.gate = Some((calls[0].0.call_id.clone(), calls[0].0.call_id.clone()));
    let probe = Arc::new(probe);
    let finished = Arc::new(Mutex::new(Vec::new()));
    let backend = lash_restate_test::backend(4895, ServerConfig::default())
        .await
        .unwrap();
    let attempt: lash_restate_test::HandlerAttempt = {
        let probe = Arc::clone(&probe);
        let finished = Arc::clone(&finished);
        Arc::new(move |scoped| {
            let calls = Arc::clone(&calls);
            let probe = Arc::clone(&probe);
            let finished = Arc::clone(&finished);
            Box::pin(async move {
                let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                let mut run =
                    RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), vec![revision()]);
                run.start_round(
                    &round,
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::clone(&probe) as Arc<dyn SingletonToolHandlers>,
                    Default::default(),
                )
                .await
                .unwrap();
                let plan = AggregatePlan {
                    key: "generic-timer".to_owned(),
                    leaves: vec![
                        AggregateLeaf::Timer { duration_ms: 0 },
                        AggregateLeaf::Call {
                            call_id: round[0].call_id.clone(),
                        },
                    ],
                    operands: vec![0, 1, 1],
                };
                run.admit_aggregate(&plan, &SystemClock).await.unwrap();
                assert!(matches!(
                    run.consume_aggregate(&plan.key, AggregateConsumer::Race)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Selected {
                        operand: 0,
                        fulfilled: true,
                        reply: None
                    }
                ));
                assert_eq!(run.lifecycle(), RunLifecycle::Live);
                assert!(!probe.gate_open.load(Ordering::SeqCst));
                assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                run.beside(
                    scoped.controller().record_run_record(
                        UNRELATED.to_owned(),
                        unrelated_record(Arc::clone(&probe)),
                    ),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(probe.unrelated.load(Ordering::SeqCst));
                probe.gate_open.store(true, Ordering::SeqCst);
                probe.gate_wake.notify_waiters();
                run.progress().await.unwrap();
                let pending_timer = AggregatePlan {
                    key: "pending-timer".to_owned(),
                    leaves: vec![
                        AggregateLeaf::Settled { fulfilled: true },
                        AggregateLeaf::Timer {
                            duration_ms: 60_000,
                        },
                    ],
                    operands: vec![0, 1],
                };
                run.admit_aggregate(&pending_timer, &SystemClock)
                    .await
                    .unwrap();
                assert!(matches!(
                    run.consume_aggregate(&pending_timer.key, AggregateConsumer::Race)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Selected { operand: 0, .. }
                ));
                run.request_cut(lash_core::BoundaryReason::HandOver);
                let snapshot = run.quiesce().await.unwrap();
                assert!(snapshot.entries.iter().flat_map(|entry| &entry.record.events)
                    .any(|event| matches!(event, RunEvent::TimerElapsed { aggregate, leaf: 0 } if aggregate == &plan.key)));
                assert_eq!(run.lifecycle(), RunLifecycle::Live);
                assert!(
                    snapshot
                        .entries
                        .iter()
                        .flat_map(|entry| &entry.record.events)
                        .any(|event| matches!(event,
                    RunEvent::AggregateAdmitted { plan, .. } if plan == &pending_timer))
                );
                assert!(matches!(
                    run.admit_aggregate(
                        &AggregatePlan {
                            key: "after-cut".to_owned(),
                            leaves: vec![],
                            operands: vec![]
                        },
                        &SystemClock
                    )
                    .await,
                    Err(SingletonRunError::Cut(_))
                ));
                let RunAggregateOutcome::AllResults(results) = run
                    .consume_aggregate(&plan.key, AggregateConsumer::AllSettled)
                    .await
                    .unwrap()
                else {
                    panic!("allSettled must preserve timer and duplicate source positions");
                };
                assert_eq!(results.len(), 3);
                assert_eq!(results[0], None);
                assert_eq!(results[1], results[2]);
                assert!(results[1].is_some());
                assert_eq!(
                    run.records()
                        .iter()
                        .flat_map(|record| &record.events)
                        .filter(|event| matches!(event, RunEvent::Consumed { .. }))
                        .count(),
                    1
                );
                run.close().await.unwrap();
                finished.lock().unwrap().push(run.into_records());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(finished.lock().unwrap().len(), 1);
    assert_eq!(
        probe.executions.lock().unwrap().as_slice(),
        &[(
            call("generic-loser", &Kind::IntentFree).call_id,
            AttemptOrdinal::FIRST
        )]
    );
}

/// L03/L05/L21: tool-free timers replay their admitted clock and selected
/// terminal, then logical Closing discharges pending timers without any group
/// executor. Empty and immediate plans need no callback registry either.
#[tokio::test]
async fn l03_l05_tool_free_timers_replay_and_close_without_group_services() {
    let backend = lash_restate_test::backend(4896, ServerConfig::default())
        .await
        .unwrap();
    backend
        .server()
        .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some("lash:run:schedule:1".to_owned()),
        }));
    let crashes = lash_restate_test::CrashCount::new();
    assert!(backend.server().on_crash(crashes.listener()));
    let finished = Arc::new(Mutex::new(Vec::new()));
    let instants = Arc::new(Mutex::new(Vec::new()));
    let attempt: lash_restate_test::HandlerAttempt = {
        let finished = Arc::clone(&finished);
        let instants = Arc::clone(&instants);
        Arc::new(move |scoped| {
            let finished = Arc::clone(&finished);
            let instants = Arc::clone(&instants);
            Box::pin(async move {
                let mut run = RunCoordinator::open(&scoped, owner(), SegmentOrdinal(0), Vec::new());
                let unknown = AggregatePlan {
                    key: "unknown-effect".to_owned(),
                    leaves: vec![
                        AggregateLeaf::Call {
                            call_id: call("not-admitted", &Kind::IntentFree).call_id,
                        },
                        AggregateLeaf::Timer { duration_ms: 0 },
                    ],
                    operands: vec![0, 1],
                };
                assert!(matches!(
                    run.admit_aggregate(&unknown, &SystemClock).await,
                    Err(SingletonRunError::Ledger(_))
                ));
                assert!(run.records().is_empty());
                let timer = AggregatePlan {
                    key: "tool-free".to_owned(),
                    leaves: vec![
                        AggregateLeaf::Timer { duration_ms: 0 },
                        AggregateLeaf::Timer {
                            duration_ms: 60_000,
                        },
                    ],
                    operands: vec![0, 1],
                };
                run.admit_aggregate(&timer, &SystemClock).await.unwrap();
                let RunEvent::AggregateAdmitted { admitted_at_ms, .. } =
                    &run.records()[0].events[0]
                else {
                    panic!("aggregate admission")
                };
                instants.lock().unwrap().push(*admitted_at_ms);
                assert!(matches!(
                    run.consume_aggregate(&timer.key, AggregateConsumer::Race)
                        .await
                        .unwrap(),
                    RunAggregateOutcome::Selected {
                        operand: 0,
                        fulfilled: true,
                        reply: None
                    }
                ));
                for (key, leaves, operands, mode, expected) in [
                    (
                        "empty-race",
                        vec![],
                        vec![],
                        AggregateConsumer::Race,
                        RunAggregateOutcome::Pending,
                    ),
                    (
                        "empty-all",
                        vec![],
                        vec![],
                        AggregateConsumer::All,
                        RunAggregateOutcome::AllResults(vec![]),
                    ),
                    (
                        "immediate-any",
                        vec![
                            AggregateLeaf::Settled { fulfilled: false },
                            AggregateLeaf::Settled { fulfilled: true },
                        ],
                        vec![0, 1, 1],
                        AggregateConsumer::Any,
                        RunAggregateOutcome::Selected {
                            operand: 1,
                            fulfilled: true,
                            reply: None,
                        },
                    ),
                ] {
                    let plan = AggregatePlan {
                        key: key.to_owned(),
                        leaves,
                        operands,
                    };
                    run.admit_aggregate(&plan, &SystemClock).await.unwrap();
                    assert_eq!(run.consume_aggregate(key, mode).await.unwrap(), expected);
                }
                run.close().await.unwrap();
                assert_eq!(run.lifecycle(), RunLifecycle::Settled);
                let count = run.records().len();
                assert!(matches!(
                    run.admit_aggregate(
                        &AggregatePlan {
                            key: "closed".to_owned(),
                            leaves: vec![],
                            operands: vec![]
                        },
                        &SystemClock
                    )
                    .await,
                    Err(SingletonRunError::Ledger(
                        lash_core::tool_run::RunEventRefusal::AdmissionClosed
                    ))
                ));
                assert_eq!(run.records().len(), count);
                finished.lock().unwrap().push(run.into_records());
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        backend.run_in_handler(AdmittedScope::turn("session", "turn"), attempt),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(crashes.get(), 1);
    let instants = instants.lock().unwrap();
    assert_eq!(instants.len(), 2);
    assert_eq!(instants[0], instants[1]);
    let finished = finished.lock().unwrap();
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0]
            .iter()
            .flat_map(|record| &record.events)
            .filter(|event| matches!(event, RunEvent::TimerElapsed { .. }))
            .count(),
        1
    );
}
