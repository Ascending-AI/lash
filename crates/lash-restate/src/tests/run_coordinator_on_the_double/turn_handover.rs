//! L09/L16: actual turn publication and cold successor adoption share K6.
use super::*;
use lash_core::facade_support::SystemClock;
use lash_core::session::OpenerState;
use lash_core::store::{PendingFollowOn, RunContinuation, RuntimeStore, ToolMaterialStore};

#[tokio::test]
async fn l09_l16_turn_commit_adopts_prior_and_current_cell_receipts_without_bodies() {
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
                    run.start_round(&earlier, handlers.clone(), Default::default())
                        .await
                        .unwrap();
                    while run.progress().await.unwrap().is_some() {}
                    // A later cell uses this same logical opener, not a fresh registry.
                    let later_cell = opener.clone();
                    run.start_round(&current, handlers, Default::default())
                        .await
                        .unwrap();
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
                    let snapshot = opener.boundary_snapshot(reason).unwrap();
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
                        server.crash_on(CrashRule::new(CrashPoint::BeforeRun {
                            name: "b01:publish".to_string(),
                        }));
                    }
                    let publishing_store = store.clone();
                    let publishing = published.clone();
                    scoped
                        .controller()
                        .record_run_record(
                            "b01:publish".to_string(),
                            Box::pin(async move {
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
        assert_eq!(crashes.get(), 1);
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
        let restored =
            OpenerState::from_snapshot_for(serde_json::from_slice(&bytes).unwrap(), &owner())
                .unwrap();
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
                let opener = restored.clone();
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
    }
}
