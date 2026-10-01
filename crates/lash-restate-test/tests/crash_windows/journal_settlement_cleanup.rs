//! Every journal retains awaiting cleanups until the relay deferral elapses.

use super::process_root_recovery::{Harness, HeldModelCall, Storage, core, start_child};
use super::*;
use lash_core::engine::{
    AdmissionId, AdmitVerdict, Admitted, DriveAbort, DriveRequest, DriveRequestId, RootOutcome,
    RootRunEnd, admission_body,
};
use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupPorts, ArtifactCleanupRelay, StoreSetAuthorities,
};
use lash_core::runtime::drive::relay::{RelayVerdict, deliver_now, relay_due};
use lash_core::store::{ObligationId, RootCommittedOutcome, RootTerminalWrite, TurnCommitId};
use lash_core::{ArtifactCleanup, ArtifactReferrer, ExecutionScope, JournalReplay, SessionDriver};

/// Holds admission or a terminal root open while the law drives cleanup passes.
struct HeldDriver {
    store: lash_core::store::SessionStore,
    session: lash_core::SessionId,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    kind: JournalKind,
}

impl HeldDriver {
    async fn held(&self) {
        self.started.notify_one();
        self.release.acquire().await.unwrap().forget();
    }
}

#[async_trait::async_trait]
impl SessionDriver for HeldDriver {
    async fn admit(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        if matches!(self.kind, JournalKind::AwaitedRoot) {
            return Ok(if ordinal == 0 {
                AdmitVerdict::Admit(admission_body::admitted(
                    self.session.clone(),
                    "driveless-root".into(),
                    request.request.clone(),
                    AdmissionId::new("awaited#0"),
                    u64::from(ordinal),
                    request.build_generation.clone(),
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

    async fn run_root(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> RootRunEnd {
        self.held().await;
        let root = admitted.root().clone();
        let mut state =
            lash_core::RuntimeSessionState::new(lash_core::testing::mock_session_policy());
        state.session_id = self.session.clone();
        state.policy.session_id = Some(self.session.clone());
        let operation = lash_core::OperationId::turn(
            self.session.as_str(),
            root.as_str(),
            "settlement-cleanup",
        );
        let mut graph = state.pending_graph_commit();
        graph.derive_node_ids(&self.session, &operation).unwrap();
        let mut commit = lash_core::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
            &state, graph, operation,
        )
        .unwrap();
        let finish = lash_core::facade_support::TurnFinish::AssistantMessage {
            text: "settled".into(),
        };
        commit.root_terminal = Some(Box::new(RootTerminalWrite {
            root: root.clone(),
            commit: TurnCommitId::new(root.clone(), 0),
            turn: root.clone(),
            outcome: RootCommittedOutcome::Finished(finish.clone()),
        }));
        self.store.commit_runtime_state(commit).await.unwrap();
        // Terminal evidence is durable, but Restate still owns this open
        // attempt. Terminal evidence alone does not settle its journal.
        self.held().await;
        RootRunEnd::owing_nothing(Ok(RootOutcome::Committed {
            root,
            outcome: lash_core::facade_support::TurnOutcome::Finished(finish),
        }))
    }

    async fn close_root(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &lash_core::SessionId,
        _root: &lash_core::TurnId,
    ) -> Result<(), DriveAbort> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum JournalKind {
    AwaitedRoot,
    DrivelessRoot,
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
    let session = lash_core::SessionId::from(run_tag("settlement-cleanup"));
    let root = lash_core::TurnId::from("driveless-root");
    let mut process = None;
    let mut held_driver = None;
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
        JournalKind::AwaitedRoot | JournalKind::DrivelessRoot | JournalKind::SessionOperation => {
            let store = lash_core::runtime::admit_session_view(
                &backend.session_store_factory(),
                &lash_core::SessionStoreCreateRequest {
                    session_id: session.clone(),
                    relation: lash_core::SessionRelation::Root,
                    pending_observer_intents: Vec::new(),
                    config: lash_core::testing::mock_session_policy().into(),
                    head: lash_core::SessionCreationHead::CommittedByCreator,
                    owning_process_id: None,
                },
            )
            .await
            .unwrap();
            let driver = Arc::new(HeldDriver {
                store,
                session: session.clone(),
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
                kind,
            });
            installation = Some(
                backend
                    .session_work()
                    .install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>),
            );
            let generation = backend.build_generation().clone();
            let scope = match kind {
                JournalKind::DrivelessRoot => {
                    ingress(&harness)
                        .send_workflow_json(
                            "LashTurn",
                            &lash_restate::turn_workflow_key(&session, &root),
                            "run",
                            &lash_restate::Call::new(lash_restate::RestateTurnDriveRequest {
                                sender_generation: Some(generation.clone()),
                                admitted: admission_body::admitted(
                                    session.clone(),
                                    root.clone(),
                                    DriveRequestId::new("unawaited"),
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
                    ExecutionScope::turn(session.clone(), root)
                }
                JournalKind::SessionOperation | JournalKind::AwaitedRoot => {
                    ingress(&harness)
                        .send_object_json(
                            "LashSession",
                            session.as_str(),
                            "drive",
                            &lash_restate::Call::new(lash_restate::RestateSessionDriveRequest {
                                request: DriveRequest {
                                    session: session.clone(),
                                    request: DriveRequestId::new("drain-drive"),
                                    build_generation: generation,
                                },
                                handed_off: None,
                            }),
                        )
                        .await
                        .unwrap();
                    if matches!(kind, JournalKind::AwaitedRoot) {
                        ExecutionScope::turn(session.clone(), root)
                    } else {
                        ExecutionScope::session_operation(session.clone(), "drain")
                    }
                }
                JournalKind::Process => unreachable!(),
            };
            tokio::time::timeout(BOUND, driver.started.notified())
                .await
                .unwrap();
            held_driver = Some(driver);
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
        lash_core::ReferrerClaim::guarded(
            ArtifactReferrer::Execution(journal.clone()),
            lash_core::ArtifactCleanupPlan::AwaitJournal,
        )
        .unwrap()
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
    if matches!(kind, JournalKind::DrivelessRoot | JournalKind::AwaitedRoot) {
        let driver = held_driver.as_ref().unwrap();
        driver.release.add_permits(1);
        tokio::time::timeout(BOUND, driver.started.notified())
            .await
            .unwrap();
        assert!(
            backend
                .session_store_factory()
                .root_terminal(&driver.session, &lash_core::TurnId::from("driveless-root"))
                .await
                .unwrap()
                .is_some(),
            "the root terminal is durable while its invocation is open"
        );
        assert_eq!(
            host.journal_replay(&journal).await.unwrap(),
            JournalReplay::MayReplay,
            "terminal evidence does not settle the still-open root invocation"
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
    if let Some(driver) = &held_driver {
        driver.release.add_permits(1);
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
    if matches!(kind, JournalKind::AwaitedRoot) {
        let drive = ExecutionScope::session_operation(
            held_driver.as_ref().unwrap().session.clone(),
            "drain",
        )
        .journal_identity()
        .unwrap();
        tokio::time::timeout(BOUND, async {
            while host.journal_replay(&drive).await.unwrap() != JournalReplay::Settled {
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
    drop(held_driver);
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
            async fn awaited_root_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, false, JournalKind::AwaitedRoot).await;
            }
            #[ignore = "live Restate; crash-windows suite"]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn live_restate_awaited_root_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, true, JournalKind::AwaitedRoot).await;
            }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn driveless_root_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, false, JournalKind::DrivelessRoot).await;
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
            async fn live_restate_driveless_root_deferral_retains_then_releases_awaiting_cleanups() {
                law($storage, true, JournalKind::DrivelessRoot).await;
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
