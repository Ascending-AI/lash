//! Run-owned foreground cells retain locals and issued waits across physical
//! turns. Ingress owns cancellation; publication owns the successor intent.
use super::*;
use lash_core::testing::{Script, StoreOp};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashCount, CrashPoint, CrashRule, RestateTestServer};

const RUN: &str = "native-cell-run";
const SLEEP: &str = "let local = 20; await sleep(60000); local += 22; finish(local);";
const PROCESS: &str = r#"
const worker = async () => { return await waitSignal("go"); };
let local = 20;
const job = await processes.start({ definition: worker });
const answer = await job;
local += 22;
finish({ answer, local });
"#;

type Requests = Arc<std::sync::Mutex<Vec<LlmRequest>>>;
type Double = lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>;

async fn parked(
    server: &RestateTestServer,
    session: &lash_core::SessionId,
) -> lash_restate_test::InvocationView {
    tokio::time::timeout(WEDGE, async {
        let mut seen = None;
        loop {
            let has_source_reader = server
                .object_state("LashDurableWaitIndex", session.as_str())
                .values()
                .any(|bytes| {
                    serde_json::from_slice::<serde_json::Value>(bytes)
                        .ok()
                        .is_some_and(|row| {
                            row["body"]["subscribers"]
                                .as_array()
                                .is_some_and(|readers| !readers.is_empty())
                                || row["body"]["awakeables"].as_array().is_some_and(|readers| {
                                    readers.iter().any(|reader| !reader["hand_over"].is_null())
                                })
                        })
                });
            let view = server.invocations().into_iter().find(|view| {
                if view.status != "running" || view.blocked_on_server != Some(true) {
                    return false;
                }
                let Some((service, rest)) = view.target.split_once('/') else {
                    return false;
                };
                let Some(key) = rest.strip_suffix("/run") else {
                    return false;
                };
                service.contains("LashTurn")
                    && server
                        .object_state(service, key)
                        .get("admission")
                        .and_then(|bytes| {
                            serde_json::from_slice::<lash_core::engine::Admitted>(bytes).ok()
                        })
                        .is_some_and(|admitted| admitted.session() == session)
                    && (has_source_reader
                        || server
                            .timers()
                            .iter()
                            .any(|timer| timer.invocation == view.id && timer.kind == "sleep"))
            });
            if let Some(view) = view {
                if seen == Some((view.id.clone(), view.journal_len)) {
                    return view;
                }
                seen = Some((view.id.clone(), view.journal_len));
            } else {
                seen = None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("cell never awaited: {:?}", server.invocations()))
}

fn turn_rule(point: CrashPoint) -> CrashRule {
    CrashRule::new(point)
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .handler("run")
}

/// One crash family, rather than copies for every physical turn and store.
#[derive(Clone, Copy, Debug)]
enum Cut {
    None,
    ClockCommand,
    ClockResult,
    PredecessorParked,
    PredecessorWake,
    Published,
    SuccessorFirst,
    SuccessorParked,
    SuccessorCommitted,
}

struct CellRun {
    world: World,
    core: LashCore,
    session: lash_core::SessionId,
    requests: Requests,
    handle: Option<crate::SendHandle>,
    first: lash_restate_test::InvocationView,
    crashes: CrashCount,
    script: Arc<Script>,
}

impl CellRun {
    async fn start(storage: Storage, code: &str, cut: Cut) -> Result<Self> {
        let mut config =
            lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual);
        config.retry.initial_interval = std::time::Duration::ZERO;
        config.retry.max_interval = std::time::Duration::ZERO;
        let script = Arc::new(Script::new());
        let world = double_world_with_script(storage, config, Some(script.clone())).await;
        let Engine::Double(double) = &world.engine else {
            unreachable!("double law")
        };
        let crashes = CrashCount::new();
        assert!(double.server().on_crash(crashes.listener()));
        match cut {
            Cut::ClockCommand => double
                .server()
                .crash_on(turn_rule(CrashPoint::BeforeRunEnding {
                    suffix: ":sleep-clock".into(),
                })),
            Cut::ClockResult => {
                double
                    .server()
                    .crash_on(turn_rule(CrashPoint::BeforeRunResultEnding {
                        suffix: ":sleep-clock".into(),
                    }))
            }
            _ => {}
        }
        let requests = Arc::default();
        let core = cell_core(
            world.engine.old_backend(),
            world.engine.old_work(),
            typescript_block(code),
            &requests,
        );
        double.install_process_worker(
            lash_core_worker::DurableProcessWorker::new(
                core.durable_process_worker_config()
                    .expect("process worker configuration"),
            )
            .expect("the cell law's process worker"),
        );
        let session = lash_core::SessionId::fixture(format!("native-cell-{cut:?}"));
        let handle = core
            .session(session.clone())
            .created()
            .await
            .open()
            .await?
            .send(TurnInput::text("retain locals and issued work"))
            .id(RUN)
            .await?;
        let first = parked(double.server(), &session).await;
        if matches!(cut, Cut::PredecessorParked) {
            assert!(double.server().crash(&first.id));
            parked(double.server(), &session).await;
        }
        Ok(Self {
            world,
            core,
            session,
            requests,
            handle: Some(handle),
            first,
            crashes,
            script,
        })
    }

    fn double(&self) -> &Double {
        let Engine::Double(double) = &self.world.engine else {
            unreachable!("double law")
        };
        double
    }

    async fn hand_over(&self, cut: Cut) -> Result<lash_restate_test::InvocationView> {
        let double = self.double();
        let predecessor_key = lash_restate::recorded_turn_invocation_key(
            self.core.store_factory.as_ref(),
            &self.session,
            &lash_core::TurnId::fixture(RUN),
        )
        .await?
        .expect("the predecessor invocation");
        match cut {
            Cut::PredecessorWake => double.server().crash_on(
                turn_rule(CrashPoint::BeforeFrame {
                    ty: MessageType::CallCommand,
                })
                .key(predecessor_key),
            ),
            Cut::Published => double.server().crash_on(
                turn_rule(CrashPoint::BeforeStateWrite {
                    key: "outcome".into(),
                    value_contains: None,
                })
                .key(predecessor_key),
            ),
            Cut::SuccessorFirst => {
                double
                    .server()
                    .crash_on(turn_rule(CrashPoint::BeforeRunResultStarting {
                        prefix: "lash:shift-run-start:".into(),
                    }))
            }
            _ => {}
        }
        let next = BuildGeneration::for_test("native-cell-next");
        double
            .add_build(next, "native-cell-next", Default::default())
            .await
            .expect("next build");
        super::waits::hand_over_event(double, &self.session).await;
        let successor = tokio::time::timeout(WEDGE, async {
            loop {
                let view = parked(double.server(), &self.session).await;
                if view.id != self.first.id {
                    break view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the successor owns the wait");
        assert_ne!(
            self.first.pinned_deployment_id,
            successor.pinned_deployment_id
        );
        assert_eq!(
            self.requests.lock_recover().len(),
            1,
            "resume never asks the model again"
        );
        let store = lash_core::runtime::live_session_view(&self.core.store_factory, &self.session)
            .await?
            .unwrap();
        let owed = store
            .load_pending_follow_on()
            .await?
            .expect("the immutable successor intent");
        assert!(owed.continuation.as_ref().unwrap().cell.is_some());
        if matches!(cut, Cut::SuccessorParked) {
            assert!(double.server().crash(&successor.id));
            parked(double.server(), &self.session).await;
            assert_eq!(
                Some(owed.clone()),
                store.load_pending_follow_on().await?,
                "redrive cannot rewrite its own intent"
            );
        }
        if matches!(cut, Cut::SuccessorCommitted) {
            double
                .server()
                .crash_on(turn_rule(CrashPoint::BeforeStateWrite {
                    key: "outcome".into(),
                    value_contains: None,
                }));
        }
        Ok(successor)
    }

    async fn process(&self) -> Result<lash_core::facade_support::ObservedProcess> {
        let all = self
            .core
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await?;
        assert_eq!(all.len(), 1, "handover never repeats process start");
        Ok(self.core.processes().get(&all[0].id).await?.unwrap())
    }

    async fn hand_over_signal_reader(&self) -> Result<()> {
        let double = self.double();
        let process = self.process().await?;
        let (predecessor, wait) = tokio::time::timeout(WEDGE, async {
            loop {
                let record = self
                    .core
                    .processes()
                    .get(&process.process_id)
                    .await
                    .unwrap()
                    .unwrap();
                if let Some(wait) = record.wait
                    && let Some(view) = double.server().invocations().into_iter().find(|view| {
                        view.target.starts_with("LashProcessWorkflow/")
                            && view.target.contains(process.process_id.as_str())
                            && view.target.ends_with("/run")
                            && view.blocked_on_server == Some(true)
                    })
                {
                    break (view, wait.kind);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the worker awaits its signal");
        let index_key = double
            .server()
            .invocations()
            .into_iter()
            .find_map(|view| {
                let rest = view.target.strip_prefix("LashDurableWaitIndex/")?;
                let key = rest.strip_suffix("/register_awakeable")?;
                key.contains(process.process_id.as_str())
                    .then(|| key.to_owned())
            })
            .expect("the signal's physical subscription");
        let readers = || {
            double
                .server()
                .object_state("LashDurableWaitIndex", &index_key)
                .values()
                .find_map(|bytes| {
                    serde_json::from_slice::<serde_json::Value>(bytes)
                        .ok()
                        .and_then(|row| row["body"]["awakeables"].as_array().cloned())
                })
                .unwrap_or_default()
        };
        let previous = readers();
        assert_eq!(previous.len(), 1);
        let (service, rest) = predecessor.target.split_once('/').unwrap();
        let _: lash_restate::Reply<()> = double.ingress().call_workflow_json(
            service, rest.strip_suffix("/run").unwrap(), "deliver_hand_over",
            &lash_restate::Call::new(serde_json::json!({
                "process_id": process.process_id,
                "generation": double.lash_backend().build_generation().expect("bound generation"),
            })),
        ).await.expect("wake the signal reader");
        tokio::time::timeout(WEDGE, async {
            loop {
                let ended = double
                    .server()
                    .invocations()
                    .iter()
                    .any(|view| view.id == predecessor.id && view.status == "completed");
                let current = readers();
                if ended && current.len() == 1 && current != previous {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the successor replaces only the signal read");
        let record = self
            .core
            .processes()
            .get(&process.process_id)
            .await?
            .unwrap();
        assert_eq!(record.wait.as_ref().map(|wait| &wait.kind), Some(&wait));
        assert!(record.cancel_request.is_none());
        assert!(!record.terminal());
        Ok(())
    }

    async fn release_process(&self) -> Result<()> {
        let process = self.process().await?;
        let handler = self
            .double()
            .open_handler(lash_core::AdmittedScope::runtime_operation(
                "native-cell-signal",
            ))
            .await
            .expect("signal handler");
        self.core
            .processes()
            .signal(
                lash_core::ProcessSignal::new(
                    lash_core::ProcessSignalIdentity::new(
                        process.process_id,
                        "go",
                        "native-cell-go",
                    )
                    .expect("signal identity"),
                    serde_json::json!("done"),
                ),
                handler.scoped(),
            )
            .await?;
        Ok(())
    }

    async fn finish(mut self, expected: serde_json::Value, cut: Cut) -> Result<()> {
        let output = tokio::time::timeout(WEDGE, self.handle.take().unwrap().output())
            .await
            .expect("the Run answers")?;
        assert_eq!(output.final_value(), Some(&expected));
        assert_eq!(self.requests.lock_recover().len(), 1);
        assert_eq!(
            self.crashes.get(),
            u64::from(!matches!(cut, Cut::None)),
            "the selected crash executed once: {cut:?}"
        );
        let store = lash_core::runtime::live_session_view(&self.core.store_factory, &self.session)
            .await?
            .unwrap();
        assert!(store.load_pending_follow_on().await?.is_none());
        Ok(())
    }
}

async fn sleep_resume(storage: Storage, (): ()) -> Result<()> {
    for cut in [
        Cut::None,
        Cut::ClockCommand,
        Cut::ClockResult,
        Cut::PredecessorParked,
        Cut::PredecessorWake,
        Cut::Published,
        Cut::SuccessorFirst,
        Cut::SuccessorParked,
        Cut::SuccessorCommitted,
    ] {
        let run = CellRun::start(storage, SLEEP, cut).await?;
        let original = run.double().server().now_ms() + 60_000;
        run.double()
            .server()
            .advance(std::time::Duration::from_secs(10));
        let successor = run.hand_over(cut).await?;
        let resumed = run
            .double()
            .server()
            .timers()
            .into_iter()
            .find(|timer| timer.invocation == successor.id && timer.kind == "sleep")
            .expect("the successor timer")
            .fire_at_ms;
        let session = run.core.session(run.session.clone()).open().await?;
        let snapshot = session.admin().state().snapshot_execution().await?.unwrap();
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Body {
            Inline {
                #[serde(with = "serde_bytes")]
                body: Vec<u8>,
            },
            Leaf {
                component: String,
            },
        }
        #[derive(serde::Deserialize)]
        struct Root {
            suspended_cell: Body,
        }
        #[derive(serde::Deserialize)]
        struct Host {
            sleep_deadlines: std::collections::BTreeMap<u64, u64>,
        }
        #[derive(serde::Deserialize)]
        struct Suspension {
            host: Host,
        }
        let root: Root = rmp_serde::from_slice(&snapshot.root).expect("the committed suspension");
        let bytes = match root.suspended_cell {
            Body::Inline { body } => body,
            Body::Leaf { component } => snapshot
                .components
                .iter()
                .find(|(key, _)| key.as_str() == component)
                .expect("suspended VM bytes")
                .1
                .to_vec(),
        };
        let suspension: Suspension =
            rmp_serde::from_slice(&bytes).expect("the sleep's captured host ledger");
        assert_eq!(
            suspension
                .host
                .sleep_deadlines
                .values()
                .copied()
                .collect::<Vec<_>>(),
            [original],
            "the absolute deadline survives {cut:?}"
        );
        assert!(
            resumed - run.double().server().now_ms() < 51_000,
            "resume uses the remaining duration"
        );
        run.double()
            .server()
            .advance(std::time::Duration::from_secs(51));
        run.finish(serde_json::json!(42), cut).await?;
    }
    Ok(())
}

async fn process_resume(storage: Storage, (): ()) -> Result<()> {
    for cut in [
        Cut::None,
        Cut::PredecessorParked,
        Cut::PredecessorWake,
        Cut::Published,
        Cut::SuccessorFirst,
        Cut::SuccessorParked,
        Cut::SuccessorCommitted,
    ] {
        let run = CellRun::start(storage, PROCESS, cut).await?;
        run.process().await?;
        run.hand_over(cut).await?;
        let process = run.process().await?;
        assert!(
            !process.terminal(),
            "retiring the predecessor read cannot cancel the process"
        );
        if matches!(cut, Cut::None) {
            run.hand_over_signal_reader().await?;
        }
        run.release_process().await?;
        run.finish(serde_json::json!({"answer":"done","local":42}), cut)
            .await?;
    }
    Ok(())
}

drain_hand_over_laws! {
    sleep_deadline_and_locals_resume_sqlite_memory: sleep_resume, Storage::SqliteMemory, ();
    sleep_deadline_and_locals_resume_sqlite_file: sleep_resume, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run inside a pg16 gate"]
    sleep_deadline_and_locals_resume_postgres: sleep_resume, Storage::Postgres, ();
    cancelled_cell_starts_identical_source_fresh_sqlite_memory: cancel_cell, Storage::SqliteMemory, false;
    cancelled_cell_starts_identical_source_fresh_sqlite_file: cancel_cell, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run inside a pg16 gate"]
    cancelled_cell_starts_identical_source_fresh_postgres: cancel_cell, Storage::Postgres, false;
    sleep_cancellation_stays_terminal_sqlite_memory: cancel_cell, Storage::SqliteMemory, true;
    sleep_cancellation_stays_terminal_sqlite_file: cancel_cell, Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run inside a pg16 gate"]
    sleep_cancellation_stays_terminal_postgres: cancel_cell, Storage::Postgres, true;
    process_await_and_locals_resume_sqlite_memory: process_resume, Storage::SqliteMemory, ();
    process_await_and_locals_resume_sqlite_file: process_resume, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run inside a pg16 gate"]
    process_await_and_locals_resume_postgres: process_resume, Storage::Postgres, ();
}

/// The cancellation windows straddle the authoritative intent read and the head transaction,
/// on both the publishing segment and its successor. Store gates count arrivals.
async fn cancel_cell(storage: Storage, sleep: bool) -> Result<()> {
    for successor in [false, true] {
        for (operation, after) in [
            (StoreOp::turn_cancel_request_intent, false),
            (StoreOp::turn_cancel_request_intent, true),
            (StoreOp::commit_runtime_state, false),
            (StoreOp::commit_runtime_state, true),
        ] {
            let mut run =
                CellRun::start(storage, if sleep { SLEEP } else { PROCESS }, Cut::None).await?;
            let original_process = if sleep {
                None
            } else {
                Some(run.process().await?.process_id)
            };
            if successor {
                run.hand_over(Cut::None).await?;
            }
            let next = run.script.calls(operation) + 1;
            let on = run.script.on(operation).nth(next);
            let gate = if after {
                on.after().pause()
            } else {
                on.before().pause()
            };
            let double = run.double().clone();
            if successor {
                double
                    .add_build(
                        BuildGeneration::for_test("native-cell-third"),
                        "native-cell-third",
                        Default::default(),
                    )
                    .await
                    .expect("third build");
                let _: lash_restate::Reply<u64> = double
                    .ingress()
                    .call_object_json(
                        &double.service_name("LashDurableWaitIndex"),
                        run.session.as_str(),
                        "hand_over_turns",
                        &lash_restate::Call::new(lash_restate::RestateDurableWaitHandOverRequest {
                            generation: BuildGeneration::for_test("native-cell-next"),
                        }),
                    )
                    .await
                    .expect("wake the successor handover");
            } else {
                double
                    .add_build(
                        BuildGeneration::for_test("native-cell-next"),
                        "native-cell-next",
                        Default::default(),
                    )
                    .await
                    .expect("next build");
                super::waits::hand_over_event(&double, &run.session).await;
            }
            gate.reached(1).await;
            run.handle
                .as_ref()
                .unwrap()
                .cancel()
                .origin("native-cell-cancellation-law")
                .await?;
            gate.open_all();
            let output = tokio::time::timeout(WEDGE, run.handle.take().unwrap().output()).await.expect("cancellation settles")
                .unwrap_or_else(|error| panic!("cancellation must settle at successor={successor} {operation:?} after={after}: {error}"));
            assert!(matches!(
                output.result.outcome,
                TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
            ));
            let store =
                lash_core::runtime::live_session_view(&run.core.store_factory, &run.session)
                    .await?
                    .unwrap();
            assert!(
                store.load_pending_follow_on().await?.is_none(),
                "terminal Run owes no VM continuation"
            );
            assert_eq!(run.requests.lock_recover().len(), 1);
            let session = run.core.session(run.session.clone()).open().await?;
            // A newly assembled runtime reads the committed state, rather than
            // using the cancelled invocation's resident VM.
            #[derive(serde::Deserialize)]
            struct CellOwner {
                suspended_cell: Option<serde::de::IgnoredAny>,
            }
            let terminal = session
                .admin()
                .state()
                .snapshot_execution()
                .await?
                .expect("RLM terminal state");
            let terminal: CellOwner =
                rmp_serde::from_slice(&terminal.root).expect("RLM snapshot root");
            assert!(
                terminal.suspended_cell.is_none(),
                "terminal cancellation discards the old VM continuation"
            );
            let fresh = session
                .send(TurnInput::text("the same source in a fresh Run"))
                .id("native-fresh-run")
                .await?;
            tokio::time::timeout(WEDGE, async {
                loop {
                    if run.requests.lock_recover().len() == 2 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the fresh Run asks for identical source");
            if sleep {
                let deadline = tokio::time::timeout(WEDGE, async {
                    loop {
                        if let Some(key) = lash_restate::recorded_turn_invocation_key(
                            run.core.store_factory.as_ref(),
                            &run.session,
                            &lash_core::TurnId::fixture("native-fresh-run"),
                        )
                        .await
                        .unwrap()
                        {
                            let views = double.server().invocations();
                            if let Some(timer) =
                                double.server().timers().into_iter().find(|timer| {
                                    timer.kind == "sleep"
                                        && views.iter().any(|view| {
                                            view.id == timer.invocation
                                                && view.target.ends_with(&format!("/{key}/run"))
                                        })
                                })
                            {
                                break timer.fire_at_ms;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the fresh Run issues its own timer");
                double.server().advance(std::time::Duration::from_millis(
                    deadline.saturating_sub(double.server().now_ms()) + 1,
                ));
            } else {
                tokio::time::timeout(WEDGE, async {
                    loop {
                        let all = run
                            .core
                            .process_registry()
                            .list_processes(&lash_core::ProcessListFilter {
                                status: lash_core::ProcessStatusFilter::Any,
                                ..Default::default()
                            })
                            .await
                            .unwrap();
                        if all.len() == 2 {
                            let fresh_process = all
                                .into_iter()
                                .find(|process| Some(&process.id) != original_process.as_ref());
                            if let Some(process) = fresh_process {
                                let handler = double
                                    .open_handler(lash_core::AdmittedScope::runtime_operation(
                                        "fresh-signal",
                                    ))
                                    .await
                                    .expect("fresh signal handler");
                                run.core
                                    .processes()
                                    .signal(
                                        lash_core::ProcessSignal::new(
                                            lash_core::ProcessSignalIdentity::new(
                                                process.id, "go", "fresh-go",
                                            )
                                            .expect("fresh signal"),
                                            serde_json::json!("fresh"),
                                        ),
                                        handler.scoped(),
                                    )
                                    .await
                                    .unwrap();
                                break;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("identical source starts a fresh process");
            }
            let output = tokio::time::timeout(WEDGE, fresh.output()).await.unwrap_or_else(|_| panic!(
                "fresh Run never settled at successor={successor} {operation:?} after={after}",
            ))?;
            assert_eq!(
                output.final_value(),
                Some(&if sleep {
                    serde_json::json!(42)
                } else {
                    serde_json::json!({"answer":"fresh","local":42})
                })
            );
        }
    }
    Ok(())
}

struct NativeCutBodies {
    entered: tokio::sync::mpsc::UnboundedSender<String>,
    release: [Arc<tokio::sync::Notify>; 2],
}

fn native_cut_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:native_cut", "native_cut", "held native body",
        serde_json::json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    ).unwrap().with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["cut"], "body"))
}

#[async_trait]
impl ToolProvider for NativeCutBodies {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![native_cut_definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "native_cut").then(|| Arc::new(native_cut_definition().contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let id = call.args["id"].as_str().unwrap();
        self.entered.send(id.into()).unwrap();
        self.release[usize::from(id == "b")].notified().await;
        lash_core::ToolOutcome::ok(serde_json::json!(id)).into()
    }
}

/// K6/L09/L16: an accepted drain cuts native X without a pending turn wait,
/// including acceptance immediately before the last X returns. Native X is
/// acknowledged before the retained Run and VM continuation move together.
#[tokio::test]
async fn l09_generation_drain_cuts_native_aggregate_without_a_turn_wait() {
    for (last_only, crash) in [
        (false, None),
        (true, None),
        (false, Some(false)),
        (false, Some(true)),
    ] {
        native_cut_without_wait(last_only, false, crash).await;
    }
}

#[tokio::test]
async fn l09_generation_drain_cuts_native_process_aggregate_without_a_source_wait() {
    for (last_only, crash) in [
        (false, None),
        (true, None),
        (false, Some(false)),
        (false, Some(true)),
    ] {
        native_cut_without_wait(last_only, true, crash).await;
    }
}

async fn native_cut_without_wait(
    last_only: bool,
    process: bool,
    crash_after_cut_ack: Option<bool>,
) {
    let world = double_world(Storage::SqliteMemory).await;
    let Engine::Double(double) = &world.engine else {
        unreachable!()
    };
    let crashes = CrashCount::new();
    assert!(double.server().on_crash(crashes.listener()));
    let (entered, mut bodies) = tokio::sync::mpsc::unbounded_channel();
    let release = [
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    ];
    let requests = Arc::default();
    let code = typescript_block(if process {
        r#"const worker = async () => { let local = 40; const results = await Promise.all([cut.body({id:"a"}), cut.body({id:"b"})]); return {results, local:local + 2}; }; const job = await processes.start({definition:worker}); finish(await job);"#
    } else {
        r#"let local = 40; const results = await Promise.all([cut.body({id:"a"}), cut.body({id:"b"})]); finish({results, local:local + 2});"#
    });
    let tools = Arc::new(NativeCutBodies {
        entered,
        release: release.clone(),
    });
    let provider = cell_provider(code.clone(), &requests);
    let deployed = |backend: lash_core::Backend, work: Arc<dyn lash_core::SessionWorkEngine>| {
        let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
            .with_session_work(work)
            .into_backend();
        rlm_core_builder_over(backend)
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(
                crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
            )
            .serve_test_llm_profile(provider.clone(), mock_llm_profile_spec())
            .tools(tools.clone())
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                ),
            ))
            .plugin(lash_core::testing::process_engine_plugin_fixture())
            .build(crate::testing::runtime_lease_owner())
            .unwrap()
    };
    let core = deployed(world.engine.old_backend(), world.engine.old_work());
    double.install_process_worker(
        lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap(),
    );
    let session = lash_core::SessionId::fixture(format!("native-cut-{process}-{last_only}"));
    let handle = core
        .session(session.clone())
        .created()
        .await
        .open()
        .await
        .unwrap()
        .send(TurnInput::text("cut active native work"))
        .id(RUN)
        .await
        .unwrap();
    for _ in 0..2 {
        tokio::time::timeout(WEDGE, bodies.recv())
            .await
            .unwrap()
            .unwrap();
    }
    if last_only {
        release[0].notify_one();
        // Pin the last-body window to a durable first-X receipt. The second
        // body remains held when the operator accepts the drain.
        tokio::time::timeout(WEDGE, async {
            loop {
                let acknowledged = double
                    .server()
                    .invocations()
                    .into_iter()
                    .filter(|view| {
                        view.target.contains(if process {
                            "LashProcessWorkflow"
                        } else {
                            "LashTurn"
                        }) && view.target.ends_with("/run")
                    })
                    .flat_map(|view| double.server().journal(&view.id).unwrap_or_default())
                    .filter_map(|entry| entry.run_completion().and_then(std::result::Result::ok))
                    .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .any(|value| {
                        value.get("call_id").is_some()
                            && value.get("attempt").is_some()
                            && value.get("result").is_some()
                    });
                if acknowledged {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first native X is acknowledged before the last-body drain");
    }
    let old = core.build_generation().clone();
    let next = BuildGeneration::for_test("native-cut-next");
    let successor = double
        .add_separate_build(next.clone(), "native-cut-next", Default::default())
        .await
        .unwrap();
    let admin = deployed(
        successor.lash_backend(),
        successor.explicit_reconcile_session_work(),
    );
    assert_eq!(admin.build_generation(), &next);
    successor.processes().install(
        lash_core_worker::DurableProcessWorker::new(admin.durable_process_worker_config().unwrap())
            .unwrap(),
    );
    assert!(admin.drain_generation(&old).await.unwrap());
    let turn_views = || {
        double
            .server()
            .invocations()
            .into_iter()
            .filter(|view| {
                view.target.contains(if process {
                    "LashProcessWorkflow"
                } else {
                    "LashTurn"
                }) && view.target.ends_with("/run")
                    // A refused admission probe owns no native Run. Count
                    // the physical owners whose journals contain K6 work.
                    && double.server().journal(&view.id).unwrap_or_default().iter()
                        .any(|entry| entry.name.as_deref().is_some_and(|name| name.starts_with("lash:run:")))
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(turn_views().len(), 1, "no successor before native ACK");
    if let Some(after_ack) = crash_after_cut_ack {
        let owner = turn_views().pop().unwrap();
        let (service, rest) = owner.target.split_once('/').unwrap();
        let point = if after_ack && process {
            CrashPoint::BeforeRunResult {
                name: Some("lash.segment.handover".into()),
            }
        } else if after_ack {
            // Publication follows durable acceptance of the cut frame.
            CrashPoint::BeforeStateWrite {
                key: "outcome".into(),
                value_contains: None,
            }
        } else {
            CrashPoint::BeforeRunResultStarting {
                prefix: "lash:run:aggregate:".into(),
            }
        };
        double.server().crash_on(
            CrashRule::new(point)
                .service(service)
                .handler("run")
                .key(rest.strip_suffix("/run").unwrap()),
        );
    }
    if !last_only {
        release[0].notify_one();
    }
    release[1].notify_one();
    let result = tokio::time::timeout(WEDGE, handle.output()).await;
    let output = match result {
        Ok(output) => output,
        Err(_) => {
            let journals = double
                .server()
                .invocations()
                .into_iter()
                .filter(|view| view.target.ends_with("/run"))
                .map(|view| {
                    let (service, key) = view.target.split_once('/').unwrap();
                    let outcome = double.server().object_state(service, key.strip_suffix("/run").unwrap())
                        .get("outcome").and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok()).map(|value| value["body"]["outcome"].clone());
                    let tail = double.server().journal(&view.id).unwrap_or_default().into_iter()
                        .rev().take(12).map(|entry| (entry.ty, entry.name.clone(), entry.run_completion().and_then(std::result::Result::err)))
                        .collect::<Vec<_>>();
                    let commands = double.server().journal(&view.id).unwrap_or_default().into_iter()
                        .filter(|entry| format!("{:?}", entry.ty).ends_with("Command"))
                        .enumerate().map(|(index, entry)| (index, entry.ty, entry.name.clone(), entry.call_command())).collect::<Vec<_>>();
                    (view, outcome, commands, tail)
                })
                .collect::<Vec<_>>();
            let store = lash_core::runtime::live_session_view(&core.store_factory, &session).await.unwrap().unwrap();
            let pending = store.load_pending_follow_on().await.unwrap().and_then(|pending| pending.continuation)
                .map(|continuation| (continuation.reason, continuation.cell.is_some(), continuation.opener.run.map(|run| run.vm_continuation)));
            panic!(
                "native cut did not resume: process={process}, last_only={last_only}, crash_after_cut_ack={crash_after_cut_ack:?}; pending={pending:?}; model_calls={}; {journals:#?}", requests.lock_recover().len()
            )
        }
    }.unwrap();
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!({"results":["a","b"],"local":42}))
    );
    assert_eq!(
        requests.lock_recover().len(),
        1,
        "VM resumes without another model call"
    );
    assert!(
        bodies.try_recv().is_err(),
        "ACKed native bodies never run again"
    );
    assert_eq!(
        crashes.get(),
        u64::from(crash_after_cut_ack.is_some()),
        "the cut crash witness executed once: process={process}, last_only={last_only}, crash_after_cut_ack={crash_after_cut_ack:?}"
    );
    let views = turn_views();
    assert_eq!(
        views.len(),
        2,
        "accepted drain published no physical Run transfer"
    );
    assert_ne!(views[0].pinned_deployment_id, views[1].pinned_deployment_id);
    let retained = views
        .iter()
        .flat_map(|view| double.server().journal(&view.id).unwrap_or_default())
        .filter_map(|entry| entry.run_completion().and_then(std::result::Result::ok))
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .any(|value| retained_native_cut(&value));
    assert!(
        retained,
        "the Capturable K6 Run and VM continuation were not retained together"
    );
}

// Process VM state is opaque bytes inside its journaled handover. Decode that
// envelope before checking the same typed K6 transfer a foreground cell owns.
fn retained_native_cut(value: &serde_json::Value) -> bool {
    if value.get("vm_continuation").is_some() {
        let transfer: lash_core::tool_run::RunTransfer =
            serde_json::from_value(value.clone()).unwrap();
        return transfer.vm_continuation
            && transfer.reason == lash_core::BoundaryReason::HandOver
            && transfer.ledger().is_ok()
            && transfer
                .entries
                .iter()
                .flat_map(|entry| &entry.record.events)
                .any(|event| {
                    matches!(
                        event,
                        lash_core::tool_run::RunEvent::CutChecked {
                            reason: lash_core::BoundaryReason::HandOver
                        }
                    )
                });
    }
    if let Some(state) = value.get("engine_state") {
        let bytes: Vec<u8> = serde_json::from_value(state.clone()).unwrap();
        let state = serde_json::from_slice(&bytes).unwrap();
        return retained_native_cut(&state);
    }
    match value {
        serde_json::Value::Object(fields) => fields.values().any(retained_native_cut),
        serde_json::Value::Array(values) => values.iter().any(retained_native_cut),
        _ => false,
    }
}
