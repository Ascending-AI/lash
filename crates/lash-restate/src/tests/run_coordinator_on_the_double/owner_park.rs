//! L02/L03/L07/L20 through the real Run-owning turn service.

use super::super::run_owner_park::NoGroupCatalog;
use super::*;
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::{
    DeploymentStore as _, SessionId, SessionShifts, SessionWorkEngine, StoreSet as _, TurnId,
};
use lash_restate_test::{DeploymentId, RestateTestServer};

struct OwnerDriver {
    session: SessionId,
    run: TurnId,
    operation: Option<lash_core::BatchId>,
    input: lash_core::InputId,
    store: lash_core::store::SessionStore,
    calls: Vec<SingletonToolCall>,
    probe: Arc<Probe>,
    records: Arc<Mutex<Vec<RunRecord>>>,
}

#[async_trait::async_trait]
impl SessionShifts for OwnerDriver {
    async fn admit(
        &self,
        scoped: ScopedEffectController<'_>,
        request: &ShiftRequest,
        generation: &BuildGeneration,
        ordinal: u32,
        _: Option<&BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        super::super::run_control_witnesses::recorded_admission(
            &scoped,
            request,
            generation,
            ordinal,
            || async {
                if self.store.run_terminal(&self.run).await.unwrap().is_some() {
                    return Ok(AdmitVerdict::Idle);
                }
                let work = self.operation.as_ref().map_or_else(
                    || AdmittedWork::Input {
                        head: self.input.clone(),
                    },
                    |operation| AdmittedWork::Operation {
                        operation: operation.clone(),
                    },
                );
                Ok(AdmitVerdict::Admit({
                    let receipt_session: lash_core::SessionId = self.session.clone();
                    let receipt_admission =
                        AdmissionId::new(format!("{}#{ordinal}", request.request.as_str()));
                    admission_body::admitted(
                        receipt_session.clone(),
                        request.request.clone(),
                        receipt_admission.clone(),
                        generation.clone(),
                        lash_core::store::ShiftAdmissionReceipt {
                            selection: lash_core::store::ShiftAdmissionSelection {
                                run: self.run.clone(),
                                work,
                                observed_epoch: 0,
                            },
                            run_start: lash_core::store::RunStartNonce::new(
                                receipt_admission.as_str(),
                            ),
                            seal: lash_core::store::ShiftEpochSeal::Sealed(
                                lash_core::store_backend_support::sealed_shift_fence(
                                    receipt_session.clone(),
                                    1,
                                    receipt_admission.clone(),
                                ),
                            ),
                            cancel_intent: lash_core::TurnCancelIntentSnapshot::Absent,
                            run_admission: None,
                        },
                    )
                }))
            },
        )
        .await
    }

    async fn execute_run(&self, scoped: ScopedEffectController<'_>, _: Admitted) -> RunEnd {
        RunEnd::owing_nothing(
            Box::pin(async {
                let operation_scope;
                let scoped = if let Some(operation) = &self.operation {
                    // As the shift's step controller: the engine's scope-bound
                    // controller rebuilt for the operation, so the Run's own
                    // effects journal under the operation's scope.
                    assert!(scoped.is_scope_bound());
                    operation_scope = scoped
                        .rescope(AdmittedScope::session_operation(
                            self.session.clone(),
                            operation.as_str(),
                        ))
                        .unwrap();
                    &operation_scope
                } else {
                    &scoped
                };
                let mut coordinator = RunCoordinator::open(
                    scoped,
                    self.calls[0].owner.clone(),
                    SegmentOrdinal(0),
                    vec![revision()],
                );
                crate::tests::decide_round(
                    &mut coordinator,
                    &self.calls,
                    self.probe.clone(),
                    Default::default(),
                )
                .await
                .map_err(|error| {
                    ShiftAbort::Retry(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::RuntimeStore,
                        error.to_string(),
                    ))
                })?;
                coordinator.await_deferred().await.map_err(|error| {
                    ShiftAbort::Retry(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::RuntimeStore,
                        error.to_string(),
                    ))
                })?;
                coordinator.close().await.map_err(|error| {
                    ShiftAbort::Retry(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::RuntimeStore,
                        error.to_string(),
                    ))
                })?;
                *self.records.lock().unwrap() = coordinator.into_records();
                let mut state =
                    lash_core::RuntimeSessionState::new(lash_core::testing::mock_session_policy());
                state.session_id = self.session.clone();
                let operation = lash_core::OperationId::turn(
                    self.session.clone(),
                    self.run.clone(),
                    "owner-law",
                );
                let mut graph = state.pending_graph_commit();
                graph.derive_node_ids(&self.session, &operation).unwrap();
                let mut commit =
                    lash_core::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
                        &state, graph, operation,
                    )
                    .unwrap();
                commit.run_terminal = Some(Box::new(RunTerminalWrite {
                    run: self.run.clone(),
                    commit: TurnCommitId::new(self.run.clone(), 0),
                    turn: self.run.clone(),
                    outcome: RunCommittedOutcome::Finished(
                        lash_core::facade_support::TurnFinish::AssistantMessage {
                            text: "all owned results drained".into(),
                        },
                    ),
                }));
                self.store.commit_runtime_state(commit).await.unwrap();
                Ok(RunOutcome::Committed {
                    run: self.run.clone(),
                    kind: RunTerminalKind::Answered,
                    work_remaining: false,
                })
            })
            .await,
        )
    }

    async fn close_run(
        &self,
        _: ScopedEffectController<'_>,
        _: &SessionId,
        _: &TurnId,
    ) -> Result<(), ShiftAbort> {
        Ok(())
    }
}

struct World {
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    work: crate::RestateSessionWork,
    control: Arc<crate::session_control::RestateSessionControl>,
    endpoint: restate_sdk::endpoint::Endpoint,
    host: Arc<crate::RestateEffectHost>,
    _installation: Arc<dyn SessionShifts>,
    driver: Arc<OwnerDriver>,
}

impl World {
    async fn open(
        server: &RestateTestServer,
        stores: Arc<lash_sqlite_store::SqliteStoreSet>,
        operation: bool,
        probe: Arc<Probe>,
        records: Arc<Mutex<Vec<RunRecord>>>,
    ) -> Self {
        let session = SessionId::fixture("session");
        let operation_id = operation.then(|| lash_core::BatchId::from("operation"));
        let owner = operation_id.as_ref().map_or_else(
            || EffectOpener::turn(session.clone(), "turn"),
            |id| {
                lash_core::tool_run::OperationRun {
                    session_id: session.clone(),
                    operation_id: id.to_string(),
                }
                .opener()
            },
        );
        let run = operation_id.as_ref().map_or_else(
            || TurnId::from("turn"),
            |id| {
                lash_core::tool_run::OperationRun {
                    session_id: session.clone(),
                    operation_id: id.to_string(),
                }
                .run_id()
            },
        );
        let factory: Arc<dyn lash_core::DeploymentStore> = stores.session_store_factory();
        let store = lash_core::runtime::admit_session_view(
            &factory,
            &lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                config: lash_core::testing::mock_session_policy().into(),
                head: lash_core::SessionCreationHead::Config,
                owning_process_id: None,
            },
        )
        .await
        .unwrap();
        let input = match store.load_turn_park().await.unwrap() {
            Some(_) => lash_core::InputId::from("already-bound"),
            None => {
                let input = store
                    .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                        session.clone(),
                        lash_core::TurnInputIngress::next_turn(),
                        lash_core::TurnInput::text("owner law"),
                    ))
                    .await
                    .unwrap()
                    .input_id;
                store
                    .bind_run_inputs(&run, std::slice::from_ref(&input))
                    .await
                    .unwrap();
                input
            }
        };
        let calls = [Kind::IntentFree, Kind::Deferred, Kind::IntentFree]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let mut call = call(&format!("owner-{index}"), &kind);
                call.owner = owner.clone();
                call
            })
            .collect();
        let driver = Arc::new(OwnerDriver {
            session,
            run,
            operation: operation_id,
            input,
            store,
            calls,
            probe,
            records,
        });
        let connection = NoGroupCatalog::connection(server);
        let admin = crate::RestateAdminClient::new(connection.clone());
        let ingress = crate::RestateIngressClient::new(connection.clone());
        let slot = crate::RestateSessionShiftsSlot::new();
        let generation = BuildGeneration::for_test("owner-law");
        let control = Arc::new(crate::session_control::RestateSessionControl {
            admin: admin.clone(),
            ingress: ingress.clone(),
            namespace: Default::default(),
            processes: stores.process_registry(),
            continuations: stores.process_continuations(),
            generation: EngineGeneration::fixed(generation.clone()),
            sessions: factory.clone(),
            lost_processes: Default::default(),
            lost_runs: Default::default(),
        });
        let work = crate::RestateSessionWork::new(
            ingress.clone(),
            slot.clone(),
            EngineGeneration::fixed(generation.clone()),
            Default::default(),
            control.clone(),
        );
        let installation = work.install_session_shifts(driver.clone());
        let host = Arc::new(crate::RestateEffectHost::new_for_test(connection));
        // Group services remain bound for the staged deletion closure. The
        // guarded transport proves that neither recovery nor these Runs call them.
        let endpoint = crate::services::bind_lash_services(
            restate_sdk::endpoint::Endpoint::builder(),
            crate::services::LashServiceParts {
                tool_realizer: Arc::new(crate::tests::NoIntentsRealizer),
                effect_host: &host,
                materials: stores.process_env_store(),

                admin,
                attachments: factory.clone() as Arc<dyn lash_core::AttachmentReferrers>,

                process_workflow: crate::process::LashProcessWorkflowImpl::new_for_test(
                    Arc::new(super::super::conformance_harness::LawProcessRunner::default()),
                    stores.process_registry(),
                    stores.process_continuations(),
                ),
                session_shifts: slot,
                build_generation: generation,
                namespace: Default::default(),
                fleet: Default::default(),
            },
        )
        .build();
        Self {
            stores,
            work,
            control,
            endpoint,
            host,
            driver,
            _installation: installation,
        }
    }

    async fn reconcile(&self) -> ParkReconcileReport {
        let clock = lash_core::facade_support::SystemClock;
        let factory: Arc<dyn lash_core::DeploymentStore> = self.stores.session_store_factory();
        let writer = lash_core::shift::StoreParkRecovery::new(factory.as_ref(), &clock);
        self.control
            .reconcile_parks(
                &writer,
                EnginePage {
                    after: None,
                    limit: std::num::NonZeroUsize::new(32).unwrap(),
                    budget: Duration::from_secs(5),
                },
            )
            .await
            .unwrap()
    }
}

async fn wait_paused(server: &RestateTestServer) -> lash_restate_test::InvocationView {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(view) = server
                .invocations()
                .into_iter()
                .find(|view| view.target.starts_with("LashTurn") && view.status == "paused")
            {
                return view;
            }
            server.fire_next_timer();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the owner exhausts its bounded attempts")
}

async fn resolve_parked_source(world: &World, probe: &Probe) {
    use lash_core::tool_run::{
        MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRole, SealWriter,
        SourceSeal,
    };
    let (call_id, source) = probe
        .sources
        .lock()
        .unwrap()
        .iter()
        .next()
        .map(|(id, source)| (id.clone(), source.clone()))
        .unwrap();
    let capture = SingletonCapture::Done {
        output: output_of(&call_id),
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
    let retained = world
        .stores
        .process_env_store()
        .retain_material(
            &MaterialHolder::Source {
                source: source.clone(),
            },
            &bundle,
        )
        .await
        .unwrap();
    let reply: crate::durable_wait::RestateSourceSealReply = world
        .control
        .ingress
        .call_lash_object(
            "LashDurableWaitIndex",
            "session",
            "seal_source",
            &crate::durable_wait::RestateSourceSealRequest {
                source,
                writer: SealWriter::External,
                seal: SourceSeal::Resolved {
                    result: Box::new(retained.references[0].clone()),
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        reply,
        crate::durable_wait::RestateSourceSealReply::Outcome { .. }
    ));
}

struct StoppedOwner;
#[async_trait::async_trait]
impl StalledExecution for StoppedOwner {
    async fn still_stopped(&self) -> Result<bool, EngineRefusal> {
        Ok(true)
    }
}

async fn park_other_owner(world: &World) -> (lash_core::store::SessionStore, TurnPark) {
    let factory: Arc<dyn lash_core::DeploymentStore> = world.stores.session_store_factory();
    let session = SessionId::fixture("other-owner");
    let run = TurnId::from("other-run");
    let store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: vec![],
            config: lash_core::testing::mock_session_policy().into(),
            head: lash_core::SessionCreationHead::Config,
            owning_process_id: None,
        },
    )
    .await
    .unwrap();
    let input = store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session.clone(),
            lash_core::TurnInputIngress::next_turn(),
            lash_core::TurnInput::text("other owner"),
        ))
        .await
        .unwrap()
        .input_id;
    store.bind_run_inputs(&run, &[input]).await.unwrap();
    let clock = lash_core::facade_support::SystemClock;
    lash_core::shift::StoreParkRecovery::new(factory.as_ref(), &clock)
        .record_engine_park(
            &ParkTarget::Run { session, run },
            ParkReason::engine_retry_exhausted(8, None, "other owner stopped".into()),
            EnginePark::new("another-owner-invocation"),
            &StoppedOwner,
        )
        .await
        .unwrap();
    let park = store.load_turn_park().await.unwrap().unwrap();
    (store, park)
}

async fn owner_law(file: bool) {
    for operation in [false, true] {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let server = RestateTestServer::new(ServerConfig::default().with_seed(4892)).unwrap();
            let stores: Arc<lash_sqlite_store::SqliteStoreSet> = if file {
                Arc::new(
                    lash_sqlite_store::SqliteStoreSet::open(directory.path())
                        .await
                        .unwrap(),
                )
            } else {
                Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap())
            };
            let kinds = [Kind::IntentFree, Kind::Deferred, Kind::IntentFree];
            let calls: Vec<_> = kinds
                .iter()
                .enumerate()
                .map(|(i, kind)| (call(&format!("owner-{i}"), kind), kind.clone()))
                .collect();
            let restored = Arc::new(AtomicBool::new(false));
            let mut probe = Probe::new(&calls);
            probe.unavailable = Some((calls[2].0.call_id.clone(), restored.clone()));
            probe.materials = Some(stores.process_env_store());
            probe.gate = Some((calls[2].0.call_id.clone(), calls[0].0.call_id.clone()));
            let mut probe = Arc::new(probe);
            let records = Arc::new(Mutex::new(Vec::new()));
            let mut world =
                World::open(&server, stores, operation, probe.clone(), records.clone()).await;
            let deployment: DeploymentId = server.register(world.endpoint.clone()).await.unwrap();
            world
                .work
                .request_shift(&world.driver.session, ShiftRequestId::new("original"))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let durable = server.invocations().iter().any(|view| server.journal(&view.id).unwrap().iter().any(|entry| {
                        let Some(Ok(bytes)) = entry.run_completion() else { return false; };
                        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false; };
                        value.get("record").and_then(|record| serde_json::from_value::<RunRecord>(record.clone()).ok()).is_some_and(|record| record.events.iter().any(|event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == calls[0].0.call_id)))
                    }));
                    if durable { break; }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.unwrap_or_else(|_| panic!("owner never made a durable decision: {:?}", server.invocations()));
            probe.gate_open.store(true, Ordering::SeqCst);
            probe.gate_wake.notify_waiters();
            let paused = wait_paused(&server).await;
            let report = world.reconcile().await;
            assert!(report.failed.is_empty(), "{report:?}");
            assert_eq!(
                report.parked,
                [ParkTarget::Run {
                    session: world.driver.session.clone(),
                    run: world.driver.run.clone()
                }]
            );
            let park = world.driver.store.load_turn_park().await.unwrap().unwrap();
            assert_eq!(park.engine.as_ref().unwrap().as_str(), paused.id);
            assert_eq!(probe.executions_of(&calls[0].0.call_id), 1);
            assert_eq!(probe.executions_of(&calls[1].0.call_id), 1);
            assert!(
                world
                    .driver
                    .store
                    .run_terminal(&world.driver.run)
                    .await
                    .unwrap()
                    .is_none()
            );
            let listed = world
                .stores
                .session_store_factory()
                .list_turn_parks(&TurnParkQuery {
                    reasons: None,
                    session: Some(world.driver.session.clone()),
                    parked_at_or_before_ms: None,
                    after: None,
                    limit: std::num::NonZeroUsize::MIN,
                })
                .await
                .unwrap();
            assert_eq!(listed.as_slice(), std::slice::from_ref(&park));
            world.reconcile().await;
            assert_eq!(
                world.driver.store.load_turn_park().await.unwrap(),
                Some(park.clone())
            );

            if !cancel {
                restored.store(true, Ordering::SeqCst);
                resolve_parked_source(&world, &probe).await;
            }
            let (other_store, other_park) = park_other_owner(&world).await;
            drop(other_store);
            if file {
                // Replace the old endpoint before dropping its stores, so no
                // handler or parked future retains a SQLite connection.
                server
                    .restart_deployment(
                        &deployment,
                        restate_sdk::endpoint::Endpoint::builder().build(),
                    )
                    .await
                    .unwrap();
                drop(world);
                let executions = probe.executions.lock().unwrap().clone();
                let sources = probe.sources.lock().unwrap().clone();
                drop(probe);
                let reopened: Arc<lash_sqlite_store::SqliteStoreSet> = Arc::new(
                    lash_sqlite_store::SqliteStoreSet::open(directory.path())
                        .await
                        .unwrap(),
                );
                let mut fresh_probe = Probe::new(&calls);
                fresh_probe.unavailable = Some((calls[2].0.call_id.clone(), restored.clone()));
                fresh_probe.materials = Some(reopened.process_env_store());
                *fresh_probe.executions.get_mut().unwrap() = executions;
                *fresh_probe.sources.get_mut().unwrap() = sources;
                probe = Arc::new(fresh_probe);
                world =
                    World::open(&server, reopened, operation, probe.clone(), records.clone()).await;
                server
                    .restart_deployment(&deployment, world.endpoint.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    world.driver.store.load_turn_park().await.unwrap(),
                    Some(park.clone())
                );
            }
            let factory: Arc<dyn lash_core::DeploymentStore> = world.stores.session_store_factory();
            let stale_redrive = if cancel {
                Some(
                    factory
                        .open_run_intent(
                            &RunIntentRequest {
                                session_id: world.driver.session.clone(),
                                run: world.driver.run.clone(),
                                park: park.park_id,
                                verb: RunVerb::Redrive,
                            },
                            9,
                        )
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let intent = factory
                .open_run_intent(
                    &RunIntentRequest {
                        session_id: world.driver.session.clone(),
                        run: world.driver.run.clone(),
                        park: park.park_id,
                        verb: if cancel {
                            RunVerb::Cancel
                        } else {
                            RunVerb::Redrive
                        },
                    },
                    10,
                )
                .await
                .unwrap();
            let scopes: Arc<dyn ScopeCloseSink> = Arc::new(
                lash_core::RegistryScopeClose::new(
                    world.stores.process_registry(),
                    Arc::new(lash_core::facade_support::SystemClock),
                )
                .with_session_store_factory(factory.clone())
                .with_effect_host(world.host.clone()),
            );
            let scope_close = Arc::new(lash_core::shift::ScopeCloseRelay::new(
                world.stores.obligation_ledger(ObligationKind::ScopeClose),
                factory.clone(),
                scopes.clone(),
            ));
            let relay = lash_core::shift::ControlIntentRelay::new(
                world
                    .stores
                    .obligation_ledger(ObligationKind::ControlIntent),
                factory.clone(),
                Arc::new(world.work.clone()),
                scopes,
                scope_close,
                Arc::new(lash_core::facade_support::SystemClock),
            );
            if let Some(stale) = stale_redrive {
                assert!(
                    matches!(relay.deliver_intent(&stale).await.unwrap(), ControlIntentState::Superseded { by } if by == intent.id)
                );
            }
            assert!(matches!(
                relay.deliver_intent(&intent).await.unwrap(),
                ControlIntentState::Acknowledged { .. }
            ));
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if world
                        .driver
                        .store
                        .run_terminal(&world.driver.run)
                        .await
                        .unwrap()
                        .is_some()
                    {
                        break;
                    }
                    server.fire_next_timer();
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert!(world.driver.store.load_turn_park().await.unwrap().is_none());
            assert!(
                server
                    .invocations()
                    .iter()
                    .all(|view| !view.target.contains("EffectGroup")),
                "L20 never calls a group service"
            );
            let other = lash_core::store::SessionStore::new(
                factory.clone(),
                SessionId::fixture("other-owner"),
            )
            .unwrap();
            assert_eq!(
                other.load_turn_park().await.unwrap(),
                Some(other_park),
                "L20 leaves other owners' parks unchanged"
            );
            assert_eq!(
                probe.executions_of(&calls[0].0.call_id),
                1,
                "L02 reuses durable sibling X"
            );
            assert_eq!(
                probe.executions_of(&calls[1].0.call_id),
                1,
                "L07 never redispatches Deferred"
            );
            assert!(
                probe
                    .executions
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, ordinal)| *ordinal == AttemptOrdinal::FIRST),
                "retry redelivery retains attempt identity"
            );
            if cancel {
                assert!(
                    records.lock().unwrap().is_empty(),
                    "cancel does not revive a failed invocation"
                );
            } else {
                {
                    let records = records.lock().unwrap();
                    assert_eq!(
                        records
                            .iter()
                            .flat_map(|r| &r.events)
                            .filter(|event| matches!(event, RunEvent::AttemptRecorded { .. }))
                            .count(),
                        3
                    );
                    assert!(drain_violations(&records, &BTreeSet::new(), None).is_empty());
                }
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if server
                            .invocations()
                            .iter()
                            .any(|view| view.id == paused.id && view.status == "completed")
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .unwrap();
                let original = server
                    .invocations()
                    .into_iter()
                    .find(|view| view.id == paused.id)
                    .unwrap();
                assert_eq!(
                    original.status, "completed",
                    "redrive keeps the original invocation journal"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l02_l03_l07_l20_turn_and_operation_recover_the_owner_sqlite_memory() {
    owner_law(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l02_l03_l07_l20_turn_and_operation_recover_after_file_reopen() {
    owner_law(true).await;
}

/// Concurrent local X in a process's own segment, with the second body
/// held until the sibling's final is durable.
pub(in crate::tests) struct ProcessAttempts {
    calls: Vec<SingletonToolCall>,
    probe: Arc<Probe>,
    runs: std::sync::atomic::AtomicUsize,
    restored: Arc<AtomicBool>,
    registry: Mutex<Option<std::sync::Weak<dyn lash_core::ProcessRegistry>>>,
}

impl ProcessAttempts {
    pub(in crate::tests) fn new() -> Self {
        let calls = ["process-sibling", "process-loser"]
            .map(|label| (call(label, &Kind::IntentFree), Kind::IntentFree));
        let restored = Arc::new(AtomicBool::new(false));
        let mut probe = Probe::new(&calls);
        probe.unavailable = Some((calls[1].0.call_id.clone(), restored.clone()));
        probe.gate = Some((calls[1].0.call_id.clone(), calls[0].0.call_id.clone()));
        Self {
            calls: calls.into_iter().map(|(call, _)| call).collect(),
            probe: Arc::new(probe),
            runs: Default::default(),
            restored,
            registry: Default::default(),
        }
    }

    pub(in crate::tests) fn observe_registry(
        &self,
        registry: &Arc<dyn lash_core::ProcessRegistry>,
    ) {
        *self.registry.lock().unwrap() = Some(Arc::downgrade(registry));
    }

    pub(in crate::tests) fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    pub(in crate::tests) fn sibling_bodies(&self) -> usize {
        self.probe.executions_of(&self.calls[0].call_id)
    }

    pub(in crate::tests) fn restore(&self) {
        self.restored.store(true, Ordering::SeqCst);
    }

    pub(in crate::tests) async fn release_after_sibling(&self, server: &RestateTestServer) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let durable = server.invocations().iter().any(|view| server.journal(&view.id).unwrap().iter().any(|entry| {
                    let Some(Ok(bytes)) = entry.run_completion() else { return false; };
                    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false; };
                    value.get("record").and_then(|record| serde_json::from_value::<RunRecord>(record.clone()).ok()).is_some_and(|record| record.events.iter().any(|event| matches!(event, RunEvent::Decided { call_id, .. } if *call_id == self.calls[0].call_id)))
                }));
                if durable { break; }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.unwrap();
        self.probe.gate_open.store(true, Ordering::SeqCst);
        self.probe.gate_wake.notify_waiters();
    }
}

#[async_trait::async_trait]
impl crate::RestateProcessRunner for ProcessAttempts {
    fn executable_generation(
        &self,
        _: &lash_core::ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _: &crate::SegmentStarted,
        process: lash_core::ProcessId,
        _: lash_core::ProcessRegistration,
        _: lash_core::ProcessExecutionContext,
        scoped: ScopedEffectController<'_>,
        _: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let registry = self
            .registry
            .lock()
            .unwrap()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let cancel_requested = match registry {
            Some(registry) => registry
                .get_process(&process)
                .await?
                .is_some_and(|record| record.cancel_request.is_some()),
            None => false,
        };
        if cancel_requested {
            self.probe.cancel.store(true, Ordering::SeqCst);
        }
        let owner = EffectOpener::process(process);
        let calls: Vec<_> = self
            .calls
            .iter()
            .cloned()
            .map(|mut call| {
                call.owner = owner.clone();
                call
            })
            .collect();
        let mut run = RunCoordinator::open(&scoped, owner, SegmentOrdinal(0), vec![revision()]);
        let attempt = async {
            crate::tests::decide_round(&mut run, &calls, self.probe.clone(), Default::default())
                .await?;
            run.close().await
        };
        tokio::pin!(attempt);
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.probe.cancel.store(true, Ordering::SeqCst);
                attempt.await
            }
            result = &mut attempt => result,
        };
        result.map_err(|error| {
            lash_core::PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                error.to_string(),
            ))
        })?;
        Ok(if cancel_requested || cancellation.is_cancelled() {
            super::super::process_cancellation("operator cancelled the parked owner", None)
        } else {
            super::super::process_success(serde_json::json!({ "resumed": "completed" }))
        }
        .into())
    }
}
