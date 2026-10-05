//! L09/L16: actual turn publication and cold successor adoption share K6.
use super::*;
use lash_core::StoreSet as _;
use lash_core::facade_support::SystemClock;
use lash_core::session::OpenerState;
use lash_core::store::{PendingFollowOn, RunContinuation, RuntimeStore, ToolMaterialStore};

#[tokio::test]
async fn l09_l16_turn_commit_adopts_prior_and_current_cell_receipts_without_bodies() {
    turn_receipts(CrashSide::BeforePublication).await;
}

#[derive(Clone, Copy)]
enum CrashSide {
    BeforePublication,
    AfterPublication,
    SuccessorAdopted,
}

/// L09/L16: the SQL head can already owe the successor when the publishing
/// worker disappears; the successor can also disappear after cold adoption.
#[tokio::test]
async fn held_receipts_survive_publication_and_successor_worker_loss() {
    for side in [CrashSide::AfterPublication, CrashSide::SuccessorAdopted] {
        turn_receipts(side).await;
    }
}

async fn turn_receipts(side: CrashSide) {
    for reason in [
        lash_core::BoundaryReason::HandOver,
        lash_core::BoundaryReason::JournalBudget,
    ] {
        let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
        let store = stores.session_store_factory();
        let backend = lash_restate_test::backend_with_store_set(
            4739,
            ServerConfig {
                protocol: lash_restate_test::protocol::ProtocolVersion::V7,
                ..ServerConfig::default()
            },
            Default::default(),
            {
                let stores = stores.clone();
                move |_| {
                    let stores = stores.clone();
                    async { Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>) }
                }
            },
        )
        .await
        .unwrap();
        let calls = Arc::new(vec![
            (
                call(
                    "earlier-final",
                    &Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
                ),
                Kind::Declares(vec![ToolIntentKind::EmitTrigger]),
            ),
            (call("earlier-source", &Kind::Deferred), Kind::Deferred),
            (call("current-loser", &Kind::IntentFree), Kind::IntentFree),
        ]);
        let mut probe = Probe::new(&calls);
        probe.materials = Some(store.clone());
        probe.gate = Some((calls[2].0.call_id.clone(), calls[0].0.call_id.clone()));
        let probe = Arc::new(probe);
        let crashes = lash_restate_test::CrashCount::new();
        assert!(backend.server().on_crash(crashes.listener()));
        let armed = Arc::new(AtomicBool::new(false));
        let published = Arc::new(Mutex::new(None));
        let attempt: lash_restate_test::HandlerAttempt = {
            let store = store.clone();
            let probe = probe.clone();
            let calls = calls.clone();
            let source_engine = backend.clone();
            let server = backend.server().clone();
            let armed = armed.clone();
            let published = published.clone();
            Arc::new(move |scoped| {
                let store = store.clone();
                let probe = probe.clone();
                let calls = calls.clone();
                let source_engine = source_engine.clone();
                let server = server.clone();
                let armed = armed.clone();
                let published = published.clone();
                Box::pin(async move {
                    let opener = OpenerState::default();
                    let handlers = probe.clone() as Arc<dyn SingletonToolHandlers>;
                    let earlier: Vec<_> = calls[..2].iter().map(|(call, _)| call.clone()).collect();
                    let current = vec![calls[2].0.clone()];
                    let mut run = opener
                        .adopt_run(
                            &scoped,
                            owner(),
                            SegmentOrdinal(0),
                            vec![revision()],
                            handlers.clone(),
                            &SystemClock,
                        )
                        .await
                        .unwrap();
                    run.start_round(
                        &earlier,
                        lash_core::tool_run::CapacityScope::Held,
                        handlers.clone(),
                        Default::default(),
                    )
                    .await
                    .unwrap();
                    while run.progress().await.unwrap().is_some() {}
                    // A later cell uses this same logical opener, not a fresh registry.
                    let later_cell = opener.clone();
                    run.start_round(
                        &current,
                        lash_core::tool_run::CapacityScope::Held,
                        handlers,
                        Default::default(),
                    )
                    .await
                    .unwrap();
                    let committed = store
                        .load_session_head_meta(&lash_core::SessionId::from("session"))
                        .await
                        .unwrap()
                        .and_then(|head| head.pending_follow_on);
                    let snapshot = if let Some(owed) = committed {
                        // Replay the same issued handles, but publication has
                        // already ended the predecessor's material holder.
                        run.request_cut(reason);
                        run.quiesce().await.unwrap();
                        owed.continuation.unwrap().opener
                    } else {
                        let capturing = later_cell.capture_run(&mut run, reason, store.as_ref());
                        tokio::pin!(capturing);
                        if !probe.gate_open.load(Ordering::SeqCst) {
                            std::future::poll_fn(|cx| {
                                assert!(matches!(
                                    std::future::Future::poll(capturing.as_mut(), cx),
                                    Poll::Pending
                                ));
                                Poll::Ready(())
                            })
                            .await;
                            let refused = opener.boundary_snapshot(reason).unwrap_err();
                            assert!(refused.turn_failure_cause().aborts_invocation());
                            assert!(
                                matches!(refused.cause, Some(lash_core::RuntimeErrorCause::RunContinuationRefused { refusal }) if *refusal == lash_core::tool_run::ContinuationRefusal::NotQuiescent)
                            );
                            assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                            probe.gate_open.store(true, Ordering::SeqCst);
                            probe.gate_wake.notify_waiters();
                        }
                        capturing.await.unwrap();
                        opener.boundary_snapshot(reason).unwrap()
                    };
                    let transfer = snapshot.run.as_ref().unwrap();
                    assert_eq!(transfer.attempts.len(), 3);
                    assert_eq!(transfer.sources.len(), 1);
                    assert!(probe.realized.lock().unwrap().is_empty());
                    assert!(probe.presentations.lock().unwrap().is_empty());
                    // A real terminal already exists, but this Run has not ranked it.
                    let source = probe.sources.lock().unwrap()[&calls[1].0.call_id].clone();
                    let capture = SingletonCapture::Done {
                        output: output_of(&calls[1].0.call_id),
                        commands: Vec::new(),
                        intents: Vec::new(),
                        stream: Default::default(),
                        start: None,
                    };
                    let bundle = lash_core::tool_run::MaterialBundle::of([
                        lash_core::tool_run::MaterialPayload::new(
                            lash_core::tool_run::MaterialOwner::Source {
                                source: source.clone(),
                            },
                            lash_core::tool_run::MaterialRole::AttemptOutput,
                            Some(revision()),
                            serde_json::to_string(&capture).unwrap(),
                        ),
                    ])
                    .unwrap()
                    .unwrap();
                    let retained = store
                        .retain_material(
                            &lash_core::tool_run::MaterialHolder::Source {
                                source: source.clone(),
                            },
                            &bundle,
                        )
                        .await
                        .unwrap();
                    let _: crate::Reply<crate::durable_wait::RestateSourceSealReply> =
                        source_engine
                            .ingress()
                            .call_object_json(
                                "LashDurableWaitIndex",
                                "session",
                                "seal_source",
                                &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                                    source,
                                    writer: lash_core::tool_run::SealWriter::External,
                                    seal: lash_core::tool_run::SourceSeal::Resolved {
                                        result: Box::new(retained.references[0].clone()),
                                    },
                                }),
                            )
                            .await
                            .unwrap();
                    if !armed.swap(true, Ordering::SeqCst) {
                        let point = match side {
                            CrashSide::BeforePublication => Some(CrashPoint::BeforeRun {
                                name: "b01:publish".to_string(),
                            }),
                            CrashSide::AfterPublication => Some(CrashPoint::BeforeRunResult {
                                name: Some("b01:publish".to_string()),
                            }),
                            CrashSide::SuccessorAdopted => None,
                        };
                        if let Some(point) = point {
                            server.crash_on(CrashRule::new(point));
                        }
                    }
                    let publishing_store = store.clone();
                    let publishing = published.clone();
                    scoped
                        .controller()
                        .record_run_record(
                            "b01:publish".to_string(),
                            Box::pin(async move {
                                if let Some(owed) = publishing_store
                                    .load_session_head_meta(&lash_core::SessionId::from("session"))
                                    .await
                                    .unwrap()
                                    .and_then(|head| head.pending_follow_on)
                                {
                                    assert_eq!(
                                        owed.continuation.as_ref().unwrap().opener,
                                        snapshot
                                    );
                                    *publishing.lock().unwrap() = Some(owed);
                                    return Ok(RunJournalEntry {
                                        record: RunRecord {
                                            segment: SegmentOrdinal(0),
                                            first: RunEventOrdinal(0),
                                            events: Vec::new(),
                                            trace: None,
                                        },
                                        materials: Vec::new(),
                                        state: Vec::new(),
                                    });
                                }
                                let runtime: Arc<dyn RuntimeStore> = publishing_store.clone();
                                lash_core::testing::store_fixtures::admit_conformance_session(
                                    &runtime,
                                    &lash_core::SessionId::from("session"),
                                )
                                .await;
                                let mut state = lash_core::RuntimeSessionState {
                                    session_id: lash_core::SessionId::from("session"),
                                    ..lash_core::RuntimeSessionState::new(
                                        lash_core::SessionPolicy::new(
                                            lash_core::TurnBudget::Unbounded,
                                            lash_core::MaxToolCalls::new(1024),
                                        ),
                                    )
                                };
                                state.ensure_agent_frame_initialized();
                                let mut commit =
                                    lash_core::RuntimeCommit::persisted_state_for_test(&state)
                                        .with_operation(lash_core::OperationId::turn(
                                            "session", "turn", "final",
                                        ))
                                        .unwrap()
                                        .0;
                                let frame = lash_core::FrameNodeId::new(
                                    commit
                                        .graph
                                        .nodes()
                                        .iter()
                                        .find(|node| node.frame_open().is_some())
                                        .unwrap()
                                        .node_id
                                        .as_str()
                                        .to_string(),
                                )
                                .unwrap();
                                let owed = PendingFollowOn::after_boundary(
                                    &lash_core::TurnId::fixture("turn"),
                                    0,
                                    frame,
                                    RunContinuation {
                                        reason,
                                        protocol_iterations: 2,
                                        cell: None,
                                        tools: None,
                                        opener: snapshot,
                                    },
                                    0,
                                    lash_core::ResolvedRun::snapshot(
                                        lash_core::PersistedSessionConfig::new(
                                            lash_core::TurnBudget::Unbounded,
                                            lash_core::MaxToolCalls::new(1024),
                                        ),
                                        Default::default(),
                                        3,
                                    ),
                                )
                                .unwrap();
                                authorize_publication(&runtime, &mut commit).await;
                                commit.pending_follow_on = Some(owed.clone());
                                runtime.commit_runtime_state(commit).await.unwrap();
                                *publishing.lock().unwrap() = Some(owed);
                                Ok(RunJournalEntry {
                                    record: RunRecord {
                                        segment: SegmentOrdinal(0),
                                        first: RunEventOrdinal(0),
                                        events: Vec::new(),
                                        trace: None,
                                    },
                                    materials: Vec::new(),
                                    state: Vec::new(),
                                })
                            }),
                        )
                        .await
                        .unwrap();
                })
            })
        };
        backend
            .run_in_handler(AdmittedScope::turn("session", "turn"), attempt)
            .await
            .unwrap();
        assert_eq!(
            crashes.get(),
            u64::from(!matches!(side, CrashSide::SuccessorAdopted))
        );
        for (call, _) in calls.iter() {
            assert_eq!(probe.executions_of(&call.call_id), 1);
        }
        let head = store
            .load_session_head_meta(&lash_core::SessionId::from("session"))
            .await
            .unwrap()
            .unwrap();
        let owed = head.pending_follow_on.unwrap();
        assert_eq!(Some(owed.clone()), *published.lock().unwrap());
        let bytes = serde_json::to_vec(&owed.continuation.as_ref().unwrap().opener).unwrap();
        if matches!(side, CrashSide::SuccessorAdopted) {
            backend
                .server()
                .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
                    name: Some("fig4928:adopted".to_string()),
                }));
        }
        assert!(
            OpenerState::from_snapshot_for(
                serde_json::from_slice(&bytes).unwrap(),
                &EffectOpener::turn("session", "fresh")
            )
            .is_err()
        );
        let successor: lash_restate_test::HandlerAttempt = {
            let probe = probe.clone();
            Arc::new(move |scoped| {
                let opener = OpenerState::from_snapshot_for(
                    serde_json::from_slice(&bytes).unwrap(),
                    &owner(),
                )
                .unwrap();
                let probe = probe.clone();
                Box::pin(async move {
                    let mut run = opener
                        .adopt_run(
                            &scoped,
                            owner(),
                            SegmentOrdinal(1),
                            vec![revision()],
                            probe.clone(),
                            &SystemClock,
                        )
                        .await
                        .unwrap();
                    assert_eq!(run.lifecycle(), RunLifecycle::Live);
                    assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                    if matches!(side, CrashSide::SuccessorAdopted) {
                        scoped
                            .controller()
                            .record_run_record(
                                "fig4928:adopted".to_string(),
                                Box::pin(async {
                                    Ok(RunJournalEntry {
                                        record: RunRecord {
                                            segment: SegmentOrdinal(1),
                                            first: RunEventOrdinal(0),
                                            events: Vec::new(),
                                            trace: None,
                                        },
                                        materials: Vec::new(),
                                        state: Vec::new(),
                                    })
                                }),
                            )
                            .await
                            .unwrap();
                    }
                    run.await_deferred().await.unwrap();
                    let terminals = run.drain().await.unwrap();
                    assert_eq!(terminals.len(), 3);
                    run.close().await.unwrap();
                })
            })
        };
        backend
            .run_in_handler(
                AdmittedScope::turn("session", owed.follow_on_turn_id),
                successor,
            )
            .await
            .unwrap();
        for (call, _) in calls.iter() {
            assert_eq!(probe.executions_of(&call.call_id), 1);
        }
        assert_eq!(probe.realized.lock().unwrap().len(), 1);
        assert_eq!(
            crashes.get(),
            1,
            "the selected durable cut crashed exactly once"
        );
    }
}

/// L06/L09/L10: a physical capture leaves the loser's source open; logical
/// cancellation closes it both before publication and after cold adoption.
#[tokio::test]
async fn cancellation_closes_captured_and_adopted_losers() {
    for adopt in [false, true] {
        let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
        let materials = stores.tool_material_store();
        let backend = lash_restate_test::backend_with_store_set(
            4928,
            ServerConfig::default(),
            Default::default(),
            {
                let stores = stores.clone();
                move |_| {
                    let stores = stores.clone();
                    async { Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>) }
                }
            },
        )
        .await
        .unwrap();
        let calls = Arc::new(vec![
            (call("winner", &Kind::IntentFree), Kind::IntentFree),
            (call("held-loser", &Kind::Deferred), Kind::Deferred),
        ]);
        let mut probe = Probe::new(&calls);
        probe.materials = Some(stores.session_store_factory());
        let probe = Arc::new(probe);
        let captured = Arc::new(Mutex::new(None));
        let attempt: lash_restate_test::HandlerAttempt = {
            let probe = probe.clone();
            let captured = captured.clone();
            let materials = materials.clone();
            let calls = calls.clone();
            Arc::new(move |scoped| {
                let probe = probe.clone();
                let captured = captured.clone();
                let materials = materials.clone();
                let calls = calls.clone();
                Box::pin(async move {
                    let round: Vec<_> = calls.iter().map(|(call, _)| call.clone()).collect();
                    let opener = OpenerState::default();
                    let mut run = opener
                        .adopt_run(
                            &scoped,
                            owner(),
                            SegmentOrdinal(0),
                            vec![revision()],
                            probe.clone(),
                            &SystemClock,
                        )
                        .await
                        .unwrap();
                    let plan = super::aggregate::aggregate_plan("held-race", &round, vec![0, 1]);
                    run.start_aggregate(
                        &plan,
                        &round,
                        lash_core::tool_run::CapacityScope::Held,
                        probe.clone(),
                        Default::default(),
                        &SystemClock,
                    )
                    .await
                    .unwrap();
                    assert!(matches!(
                        run.consume_aggregate(
                            &plan.key,
                            lash_core::tool_run::AggregateConsumer::Race
                        )
                        .await
                        .unwrap(),
                        lash_core::tool_dispatch::RunAggregateOutcome::Selected {
                            operand: 0,
                            fulfilled: true,
                            ..
                        }
                    ));
                    opener
                        .capture_run(
                            &mut run,
                            lash_core::BoundaryReason::HandOver,
                            materials.as_ref(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(run.lifecycle(), RunLifecycle::Live);
                    assert!(probe.cancelled_calls.lock().unwrap().is_empty());
                    *captured.lock().unwrap() = Some(
                        opener
                            .boundary_snapshot(lash_core::BoundaryReason::HandOver)
                            .unwrap(),
                    );
                    if !adopt {
                        run.close().await.unwrap();
                        assert_eq!(run.lifecycle(), RunLifecycle::Settled);
                    }
                })
            })
        };
        backend
            .run_in_handler(AdmittedScope::turn("session", "turn"), attempt)
            .await
            .unwrap();
        if adopt {
            let snapshot = captured.lock().unwrap().clone().unwrap();
            let attempt: lash_restate_test::HandlerAttempt = {
                let probe = probe.clone();
                Arc::new(move |scoped| {
                    let opener =
                        OpenerState::from_snapshot_for(snapshot.clone(), &owner()).unwrap();
                    let probe = probe.clone();
                    Box::pin(async move {
                        let mut run = opener
                            .adopt_run(
                                &scoped,
                                owner(),
                                SegmentOrdinal(1),
                                vec![revision()],
                                probe,
                                &SystemClock,
                            )
                            .await
                            .unwrap();
                        run.close().await.unwrap();
                        assert_eq!(run.lifecycle(), RunLifecycle::Settled);
                    })
                })
            };
            backend
                .run_in_handler(AdmittedScope::turn("session", "successor"), attempt)
                .await
                .unwrap();
        }
        let source = probe.sources.lock().unwrap()[&calls[1].0.call_id].clone();
        let reply: crate::Reply<crate::durable_wait::RestateSourceSealReply> = backend
            .ingress()
            .call_object_json(
                "LashDurableWaitIndex",
                "session",
                "seal_source",
                &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                    source,
                    writer: lash_core::tool_run::SealWriter::Owner { opener: owner() },
                    seal: lash_core::tool_run::SourceSeal::Cancelled,
                }),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                reply.body,
                crate::durable_wait::RestateSourceSealReply::Outcome {
                    outcome: lash_core::tool_run::SealOutcome::AlreadySealed {
                        seal: lash_core::tool_run::SourceSeal::Cancelled
                    }
                }
            ),
            "Closing already sealed the loser: {reply:?}"
        );
        for (call, _) in calls.iter() {
            assert_eq!(probe.executions_of(&call.call_id), 1);
        }
    }
}

/// A native final head write carries the cancellation snapshot retained at
/// admission, even when the fixture itself issues no cancel request.
async fn authorize_publication(
    store: &Arc<dyn RuntimeStore>,
    commit: &mut lash_core::RuntimeCommit,
) {
    use lash_core::testing::store_fixtures::admit_run_request_for_test;
    let session = lash_core::SessionId::fixture("session");
    let turn = lash_core::TurnId::fixture("turn");
    let scope = lash_core::ExecutionScope::turn(session.clone(), turn.clone());
    let binding =
        lash_core::runtime::turn_control_binding_id_for_scope("publication-law", &scope).unwrap();
    let input = store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session.clone(),
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text("publication"),
            )
            .with_source_key("publication-law"),
        )
        .await
        .unwrap();
    let admission = lash_core::store::AdmissionId::new("publication#0");
    let executor = lash_core::store::RunExecutor::run(&admission);
    let preparation = store
        .prepare_shift_admission(&session, &admission, &executor)
        .await
        .unwrap();
    let selected = preparation.selection.as_ref().unwrap();
    let mut request = admit_run_request_for_test(
        &preparation.prospective_fence,
        &selected.run,
        lash_core::store::AdmittedHead::Input(input.input_id),
    );
    request.unsealed_epoch = Some(preparation.epoch.epoch);
    request.executor = executor.clone();
    request.turn_cancellation = Some(lash_core::store::TurnCancellationBinding {
        binding_id: binding.clone(),
        admitted_scope: scope.clone(),
    });
    let prepared = store
        .prepare_run_admission(&request)
        .await
        .unwrap()
        .unwrap();
    let receipt = store
        .commit_shift_admission(
            &lash_core::store::ShiftAdmissionWrite {
                session_id: session.clone(),
                admission,
                run_start: lash_core::store::RunStartNonce::new("publication-start"),
                executor,
                preparation,
                run: Some(prepared),
            },
            &lash_trace::TraceAnchor::Untraced,
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.cancel_intent,
        lash_core::TurnCancelIntentSnapshot::Absent
    );
    let lash_core::store::ShiftEpochSeal::Sealed(fence) = receipt.seal else {
        panic!("the publication's admitted fence")
    };
    let key = |wait| crate::tests::test_restate_await_event_key(&scope, wait).unwrap();
    let authorization = lash_core::TurnCancelClosureAuthorization::new(
        lash_core::facade_support::TurnAddress::new(session, turn.clone()),
        binding,
        scope.clone(),
        key(lash_core::AwaitEventWaitIdentity::TurnCancelGate),
        key(lash_core::AwaitEventWaitIdentity::TurnCancelEscalation),
        key(lash_core::AwaitEventWaitIdentity::TurnTerminal),
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &fence,
    )
    .unwrap();
    commit.interrupted_turn = Some(lash_core::store::InterruptedTurnClosure {
        settlement: lash_core::TurnCancelClosureSettlement::new(authorization, None, None),
        observed_intent: lash_core::TurnCancelIntentSnapshot::Absent,
        admitted_intent: Some(receipt.cancel_intent),
    });
    commit.shift_fence = Some(Box::new(fence));
    commit.park_run = Some(turn);
}
