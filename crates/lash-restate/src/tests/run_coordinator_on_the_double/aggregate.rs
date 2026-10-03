//! Early aggregate selection, loser ownership and logical Closing laws.

use super::*;
use lash_core::facade_support::SystemClock;
use lash_core::tool_dispatch::RunAggregateOutcome;
use lash_core::tool_run::{AggregateConsumer, AggregateLeaf, AggregatePlan};

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

fn aggregate_plan(key: &str, calls: &[SingletonToolCall], operands: Vec<u32>) -> AggregatePlan {
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
                run.start_aggregate(&prefix, &round, handlers, Default::default(), &SystemClock)
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
