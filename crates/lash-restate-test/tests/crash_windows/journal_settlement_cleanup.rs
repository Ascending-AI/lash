//! Every journal retains awaiting cleanups until the relay deferral elapses.

use super::process_run_recovery::{Harness, HeldModelCall, Storage, core, start_child};
use super::*;
use lash_core::engine::{
    AdmissionId, AdmitVerdict, Admitted, RunEnd, RunOutcome, ShiftAbort, ShiftRequest,
    ShiftRequestId, admission_body,
};
use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupPorts, ArtifactCleanupRelay, StoreSetAuthorities,
};
use lash_core::runtime::shift::relay::{RelayVerdict, deliver_now, relay_due};
use lash_core::store::{ObligationId, RunCommittedOutcome, RunTerminalWrite, TurnCommitId};
use lash_core::{ArtifactCleanup, ArtifactReferrer, ExecutionScope, JournalReplay, SessionShifts};

/// Holds admission or a terminal run open while the law executes cleanup passes.
struct HeldShifts {
    store: lash_core::store::SessionStore,
    session: lash_core::SessionId,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    kind: JournalKind,
}

impl HeldShifts {
    async fn held(&self) {
        self.started.notify_one();
        self.release.acquire().await.unwrap().forget();
    }
}

#[async_trait::async_trait]
impl SessionShifts for HeldShifts {
    async fn admit(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        if matches!(self.kind, JournalKind::AwaitedRun) {
            return Ok(if ordinal == 0 {
                AdmitVerdict::Admit(admission_body::admitted(
                    self.session.clone(),
                    "shiftless-run".into(),
                    request.request.clone(),
                    AdmissionId::new("awaited#0"),
                    u64::from(ordinal),
                    admitting_generation.clone(),
                    lash_core::engine::AdmittedWork::Input {
                        head: "awaited-input".into(),
                    },
                ))
            } else {
                AdmitVerdict::Idle
            });
        }
        self.held().await;
        Ok(AdmitVerdict::Idle)
    }

    async fn execute_run(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> RunEnd {
        self.held().await;
        let run = admitted.run().clone();
        let mut state =
            lash_core::RuntimeSessionState::new(lash_core::testing::mock_session_policy());
        state.session_id = self.session.clone();
        let operation =
            lash_core::OperationId::turn(self.session.clone(), run.clone(), "settlement-cleanup");
        let mut graph = state.pending_graph_commit();
        graph.derive_node_ids(&self.session, &operation).unwrap();
        let mut commit = lash_core::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
            &state, graph, operation,
        )
        .unwrap();
        let finish = lash_core::facade_support::TurnFinish::AssistantMessage {
            text: "settled".into(),
        };
        commit.run_terminal = Some(Box::new(RunTerminalWrite {
            run: run.clone(),
            commit: TurnCommitId::new(run.clone(), 0),
            turn: run.clone(),
            outcome: RunCommittedOutcome::Finished(finish.clone()),
        }));
        self.store.commit_runtime_state(commit).await.unwrap();
        // Terminal evidence is durable, but Restate still owns this open
        // attempt. Terminal evidence alone does not settle its journal.
        self.held().await;
        RunEnd::owing_nothing(Ok(RunOutcome::Committed {
            run,
            kind: lash_core::store::RunTerminalKind::Answered,
        }))
    }

    async fn close_run(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &lash_core::SessionId,
        _run: &lash_core::TurnId,
    ) -> Result<(), ShiftAbort> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum JournalKind {
    AwaitedRun,
    ShiftlessRun,
    Process,
    SessionOperation,
}

fn ingress(harness: &Harness) -> lash_restate::RestateIngressClient {
    match harness {
        Harness::Double(backend) => backend.ingress(),
        Harness::Live(backend) => backend.ingress(),
    }
}

fn cleanup_relay(backend: &lash_core::Backend) -> ArtifactCleanupRelay {
    ArtifactCleanupRelay::new(ArtifactCleanupPorts {
        ledger: backend.artifact_cleanup(),
        authorities: Arc::new(StoreSetAuthorities {
            effect_host: backend.effect_host(),
            sessions: backend.session_store_factory(),
            processes: backend.process_registry(),
            triggers: backend.trigger_store(),
        }),
        process_env: backend.process_env_store(),
        modules: backend.module_artifacts(),
        definitions: backend.definition_store(),
        engines: lash_core::ProcessEngineRegistry::new(),
        attachments: backend.attachment_referrers(),
        clock: backend.clock(),
    })
}

async fn law(storage: Storage, live: bool, kind: JournalKind) {
    let (harness, _stores) = Harness::new(storage, live).await;
    let backend = harness.backend();
    let session = lash_core::SessionId::fixture(run_tag("settlement-cleanup"));
    let run = lash_core::TurnId::from("shiftless-run");
    let mut process = None;
    let mut held_shifts = None;
    let mut installation = None;
    let scope = match kind {
        JournalKind::Process => {
            let hold = Arc::new(HeldModelCall::new());
            let core = core(&harness, Arc::clone(&hold));
            let id = start_child(&harness, &core, None, "settlement-cleanup-process").await;
            tokio::time::timeout(BOUND, hold.started.notified())
                .await
                .unwrap();
            process = Some((core, hold));
            ExecutionScope::process(id)
        }
        JournalKind::AwaitedRun | JournalKind::ShiftlessRun | JournalKind::SessionOperation => {
            let store = lash_core::runtime::admit_session_view(
                &backend.session_store_factory(),
                &lash_core::SessionStoreCreateRequest {
                    session_id: session.clone(),
                    relation: lash_core::SessionRelation::Root,
                    pending_observer_intents: Vec::new(),
                    config: lash_core::testing::mock_session_policy().into(),
                    head: lash_core::SessionCreationHead::Config,
                    owning_process_id: None,
                },
            )
            .await
            .unwrap();
            let shifts = Arc::new(HeldShifts {
                store,
                session: session.clone(),
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
                kind,
            });
            installation = Some(
                backend
                    .session_work()
                    .install_session_shifts(Arc::clone(&shifts) as Arc<dyn SessionShifts>),
            );
            let generation = backend
                .build_generation()
                .expect("the engine's generation is bound")
                .clone();
            let scope = match kind {
                JournalKind::ShiftlessRun => {
                    ingress(&harness)
                        .send_workflow_json(
                            "LashTurn",
                            &lash_restate::turn_workflow_key(&session, &run),
                            "run",
                            &lash_restate::Call::new(lash_restate::RestateRunRequest {
                                sender_generation: Some(generation.clone()),
                                admitted: admission_body::admitted(
                                    session.clone(),
                                    run.clone(),
                                    ShiftRequestId::new("unawaited"),
                                    AdmissionId::new("unawaited#0"),
                                    0,
                                    generation,
                                    lash_core::engine::AdmittedWork::Input {
                                        head: lash_core::InputId::from("unawaited-input"),
                                    },
                                ),
                            }),
                        )
                        .await
                        .unwrap();
                    ExecutionScope::turn(session.clone(), run)
                }
                JournalKind::SessionOperation | JournalKind::AwaitedRun => {
                    ingress(&harness)
                        .send_object_json(
                            "LashSession",
                            session.as_str(),
                            "shift",
                            &lash_restate::Call::new(lash_restate::RestateSessionShiftRequest {
                                request: ShiftRequest {
                                    session: session.clone(),
                                    request: ShiftRequestId::new("drain-shift"),
                                    intended_lane: None,
                                },
                                handed_off: None,
                            }),
                        )
                        .await
                        .unwrap();
                    if matches!(kind, JournalKind::AwaitedRun) {
                        ExecutionScope::turn(session.clone(), run)
                    } else {
                        ExecutionScope::session_operation(session.clone(), "drain")
                    }
                }
                JournalKind::Process => unreachable!(),
            };
            tokio::time::timeout(BOUND, shifts.started.notified())
                .await
                .unwrap();
            held_shifts = Some(shifts);
            scope
        }
    };
    let journal = scope.journal_identity().unwrap();
    let host = backend.effect_host();
    assert_eq!(
        host.journal_replay(&journal).await.unwrap(),
        JournalReplay::MayReplay
    );
    let ledger = backend.artifact_cleanup();
    let now = backend.clock().timestamp_ms();
    let cleanups = [
        ArtifactCleanup::ended(
            ArtifactReferrer::FrameEnvironment(lash_core::FrameEnvironmentId::new(
                session,
                lash_core::FrameNodeId::new("settlement-frame").unwrap(),
            )),
            Vec::new(),
            Some(journal.clone()),
        ),
        lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(journal.clone()))
            .guard_cleanup()
            .unwrap(),
    ];
    let relay = cleanup_relay(&backend);
    let mut ids: Vec<ObligationId> = Vec::new();
    for cleanup in &cleanups {
        let id = ledger.arm_cleanup(cleanup, now).await.unwrap();
        assert_eq!(
            deliver_now(&relay, &id, &lash_core::testing::TestClock::new(now))
                .await
                .unwrap(),
            RelayVerdict::Deferred {
                due_at_ms: now + 900_000
            },
        );
        ids.push(id);
    }
    if matches!(kind, JournalKind::ShiftlessRun | JournalKind::AwaitedRun) {
        let shifts = held_shifts.as_ref().unwrap();
        shifts.release.add_permits(1);
        tokio::time::timeout(BOUND, shifts.started.notified())
            .await
            .unwrap();
        assert!(
            backend
                .session_store_factory()
                .run_terminal(&shifts.session, &lash_core::TurnId::from("shiftless-run"))
                .await
                .unwrap()
                .is_some(),
            "the run terminal is durable while its invocation is open"
        );
        assert_eq!(
            host.journal_replay(&journal).await.unwrap(),
            JournalReplay::MayReplay,
            "terminal evidence does not settle the still-open run invocation"
        );
    }
    // Force a due pass while the invocation is still open. It must retain
    // both obligations and start another full deferral, regardless of kind.
    let deferred_at = now + 900_000;
    let deferred_until = deferred_at + 900_000;
    let open_pass = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(deferred_at),
        std::num::NonZeroUsize::new(64).unwrap(),
    )
    .await
    .unwrap();
    assert!(open_pass.claimed >= 2);
    assert_eq!(
        count_outstanding(&ledger, &ids).await,
        2,
        "an open journal retains both awaiting cleanups even after the first deferral"
    );
    if let Some(shifts) = &held_shifts {
        shifts.release.add_permits(1);
    }
    if let Some((_, hold)) = &process {
        hold.release.add_permits(1);
    }
    tokio::time::timeout(BOUND, async {
        loop {
            if host.journal_replay(&journal).await.unwrap() == JournalReplay::Settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the engine observes settlement after the handler ends");
    if matches!(kind, JournalKind::AwaitedRun) {
        let shift = ExecutionScope::session_operation(
            held_shifts.as_ref().unwrap().session.clone(),
            "drain",
        )
        .journal_identity()
        .unwrap();
        tokio::time::timeout(BOUND, async {
            while host.journal_replay(&shift).await.unwrap() != JournalReplay::Settled {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    relay_due(
        &relay,
        &lash_core::testing::TestClock::new(deferred_until - 1),
        std::num::NonZeroUsize::new(64).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        count_outstanding(&ledger, &ids).await,
        2,
        "settlement retains awaiting cleanups until the relay deferral expires"
    );
    let pass = relay_due(
        &relay,
        &lash_core::testing::TestClock::new(deferred_until),
        std::num::NonZeroUsize::new(64).unwrap(),
    )
    .await
    .unwrap();
    let outstanding = count_outstanding(&ledger, &ids).await;
    println!(
        "{kind:?} on {storage:?}, live={live}: settled journal, next pass {pass:?}, outstanding={outstanding}"
    );
    drop(installation);
    drop(held_shifts);
    drop(process);
    harness.finish().await;
    assert_eq!(
        outstanding, 0,
        "the relay releases both awaiting cleanups once settlement and the deferral permit it"
    );
}

async fn count_outstanding(
    ledger: &Arc<dyn lash_core::store::ArtifactCleanupLedger>,
    ids: &[ObligationId],
) -> usize {
    let mut outstanding = 0;
    for id in ids {
        outstanding += usize::from(ledger.load_cleanup(id).await.unwrap().is_some());
    }
    outstanding
}

macro_rules! laws {
    ($module:ident, $storage:expr $(, $service:literal)?) => {
        mod $module {
            use super::*;
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn awaited_run_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, false, JournalKind::AwaitedRun).await;
            }
            #[ignore = "live Restate; crash-windows suite"]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn live_restate_awaited_run_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, true, JournalKind::AwaitedRun).await;
            }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn driveless_run_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, false, JournalKind::ShiftlessRun).await;
            }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn process_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, false, JournalKind::Process).await;
            }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn session_operation_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, false, JournalKind::SessionOperation).await;
            }
            #[ignore = "live Restate; crash-windows suite"]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn live_restate_driveless_run_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, true, JournalKind::ShiftlessRun).await;
            }
            #[ignore = "live Restate; crash-windows suite"]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn live_restate_process_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, true, JournalKind::Process).await;
            }
            #[ignore = "live Restate; crash-windows suite"]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn live_restate_session_operation_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, true, JournalKind::SessionOperation).await;
            }
        }
    };
}

laws!(sqlite_memory, Storage::Memory);
laws!(sqlite_file, Storage::File);
laws!(
    postgres,
    Storage::Postgres,
    "requires PostgreSQL; pg16 service gate"
);
