use super::*;

/// A process-backed session turn opens its tool group under the process
/// scope (ADR 0099 §1). Its paused child parks that process, and the host's
/// park redrive retries the child's retained journal. A second exhaustion
/// re-parks the same park once; restoring the model completes the process.
pub(super) async fn a_process_opened_group_child_parks_resumes_and_reparks_idempotently(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let shown = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = || {
        let calls = Arc::clone(&calls);
        let shown = Arc::clone(&shown);
        lash::testing::TestProvider::builder()
            .kind(KIND)
            .complete(move |request| {
                let response = match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "process-ask-1".to_string(),
                            tool_name: ASK_MODEL.to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    },
                    1 => text("the process's direct answer"),
                    _ => {
                        shown
                            .lock()
                            .expect("shown requests")
                            .push(format!("{:?}", request.messages));
                        text("the process answers")
                    }
                };
                async move { Ok(response) }
            })
            .build()
            .into_handle()
    };
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", provider()));
    let settled = Arc::new(Mutex::new(Vec::new()));
    let tools: Arc<dyn lash::plugins::PluginFactory> =
        Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("keys-process-ask-model"),
            lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(AskModel {
                catalog: Arc::clone(&catalog),
                retired: AtomicBool::new(false),
                settled: Arc::clone(&settled),
            })),
        ));
    let core = core_over(&double, &catalog, vec![tools]);
    double.double.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("process worker configuration"),
        )
        .expect("the process worker builds"),
    );
    let started = start_on(
        &double,
        &core,
        "keys-process-group-start",
        session_turn_start(
            "keys-process-group",
            "ask the model through the tool",
            &lash::SessionSpec::new(
                KIMI,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            ),
        ),
    )
    .await
    .expect("the process starts its child session turn");
    let process = started.process_id;
    let registry = double.double.lash_backend().process_registry();
    let work = lash::ParkedWorkRef::Process {
        process_id: process.clone(),
    };
    let parked = await_parked_on(&double, KIMI).await;
    let dispatch = double.double.service_name("EffectGroupDispatch");
    assert!(
        parked.target.starts_with(&dispatch) && parked.target.ends_with("/child"),
        "the engine stopped the process's tool child: {parked:?}"
    );
    assert_no_recorded_bind_fault(&double, &parked);
    reconcile_pass(&double).await;
    let record = registry
        .get_process(&process)
        .await
        .expect("read the process")
        .expect("the process is retained");
    let park = record
        .park()
        .cloned()
        .expect("the paused group child parks the process that opened its group");
    assert!(!record.is_terminal());
    assert_eq!(
        record.outcome(),
        None,
        "exhaustion writes no terminal evidence"
    );
    assert_eq!(park.attempts, 1);
    assert!(park.refusing);
    assert_eq!(
        park.engine, None,
        "a child park discovers all children by opener"
    );
    assert_eq!(
        park.reason.code(),
        lash::persistence::ParkReasonCode::EngineRetryExhausted
    );
    assert_eq!(park.reason.model_key(), Some(&ModelKey::new(KIMI)));
    assert_eq!(
        park.build_generation,
        record
            .first_started
            .as_deref()
            .and_then(|start| start.build_generation.clone()),
        "the park retains its process checkpoint's generation"
    );
    let listed = listed_parks(&core).await;
    assert_eq!(listed.len(), 1, "only the process is parked: {listed:?}");
    assert_eq!(listed[0].0, work);
    if !replay {
        replay_waiting_process(&double, &process).await;
    }
    reconcile_pass(&double).await;
    assert_eq!(
        listed_parks(&core).await,
        listed,
        "the same pause writes nothing twice"
    );
    assert_eq!(
        registry
            .get_process(&process)
            .await
            .expect("read")
            .expect("retained")
            .park(),
        record.park(),
        "reconciliation retains every park field"
    );

    // The key remains unavailable: the operator resumes the child, which
    // exhausts a new retry loop over the same invocation and journal.
    // The attempts are the engine's count: a bind fault ends an attempt
    // before its tool settles on anything (FIG-4632).
    let failed_attempts = parked.attempts;
    core.parked_work()
        .redrive(&work, park.park_id)
        .await
        .expect("the park surface resumes the process's paused child");
    let paused_again = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            if let Some(view) = double
                .double
                .server()
                .invocations()
                .into_iter()
                .find(|view| {
                    view.id == parked.id
                        && view.status == "paused"
                        && view.attempts > failed_attempts
                })
            {
                break view;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the same child exhausts its second retry loop");
    assert_no_recorded_bind_fault(&double, &paused_again);
    reconcile_pass(&double).await;
    let reparked = registry
        .get_process(&process)
        .await
        .expect("read")
        .expect("retained");
    let second = reparked.park().expect("the second exhaustion re-parks");
    assert_eq!(second.park_id, park.park_id);
    assert_eq!(second.since_ms, park.since_ms);
    assert_eq!(second.attempts, 2, "one refusal per exhausted retry loop");
    assert!(second.refusing);
    assert_eq!(reparked.outcome(), None);
    let listed_again = listed_parks(&core).await;
    reconcile_pass(&double).await;
    assert_eq!(
        listed_parks(&core).await,
        listed_again,
        "the second pause is idempotent too"
    );
    assert_eq!(
        registry
            .get_process(&process)
            .await
            .expect("read")
            .expect("retained")
            .park(),
        reparked.park()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "retries replay the opener's model call"
    );

    catalog.serve(registry_of(KIMI, "kimi-k3", provider()));
    core.parked_work()
        .redrive(&work, park.park_id)
        .await
        .expect("the same park resumes the restored child");
    let completed = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let record = registry
                .get_process(&process)
                .await
                .expect("read")
                .expect("retained");
            if record.is_terminal() {
                break record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the resumed process completes");
    assert_eq!(completed.status(), lash_core::ProcessStatus::Completed);
    assert_eq!(completed.park(), None);
    assert!(listed_parks(&core).await.is_empty());
    assert_eq!(
        double
            .double
            .server()
            .invocations()
            .iter()
            .find(|view| view.id == parked.id)
            .map(|view| view.status),
        Some("completed")
    );
    assert_eq!(
        settled.lock().expect("settled attempts").last(),
        Some(&Ok("the process's direct answer".to_string()))
    );
    let shown = shown.lock().expect("shown requests").join("\n");
    assert!(
        shown.contains("the process's direct answer")
            && !shown.contains("the direct completion failed"),
        "the process sees the completion, never the bind fault: {shown}"
    );
    let events = registry
        .process_park_feed(
            lash::persistence::ParkFeedCursor::initial(),
            std::num::NonZeroUsize::new(64).expect("non-zero"),
        )
        .await
        .expect("read park feed")
        .events;
    let events: Vec<_> = events
        .into_iter()
        .filter(|event| event.target == process)
        .collect();
    assert_eq!(
        events.len(),
        2,
        "one park opens and one completion ends it: {events:?}"
    );
    assert_eq!(events[0].park_id, park.park_id);
    assert_eq!(events[1].park_id, park.park_id);
}

/// Replay the still-running opener while its dispatcher is paused. This
/// replay reaches the same wait; it is not an operator's park redrive.
async fn replay_waiting_process(double: &Double, process: &lash::ProcessId) {
    let server = double.double.server();
    let segment = server
        .invocations()
        .into_iter()
        .find(|view| {
            view.target.starts_with("LashProcessWorkflow")
                && view.target.contains(process.as_str())
                && view.target.ends_with("/run")
                && view.status == "running"
        })
        .expect("the opener's segment waits for its dispatcher");
    assert!(server.crash(&segment.id), "replay the waiting segment");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if server.invocations().iter().any(|view| {
                view.id == segment.id
                    && view.attempts > segment.attempts
                    && view.blocked_on_server == Some(true)
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the replay reaches the same dispatcher wait");
}

/// Dispatcher preparation and retirement are work of the same retained
/// opener as its children. Both scopes use the park surface, including a
/// retirement that already replaced the live group with pending cleanup.
async fn paused_group_dispatch_work_parks_its_opener(
    tier: Tier,
    replay: bool,
    seed: u64,
    process_opener: bool,
    handler: &'static str,
) {
    for retirement_window in 0..if handler == "retire" { 3 } else { 1 } {
        let refusing = Arc::new(AtomicBool::new(true));
        let refused = Arc::clone(&refusing);
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let hooks = lash_restate_test::DeploymentHooks {
            refuse: Some(Arc::new(move |dispatch| {
                if dispatch.service.contains("EffectGroupDispatch") && dispatch.handler == handler {
                    let attempt = observed.fetch_add(1, Ordering::SeqCst);
                    // Crash once into the cleanup window, then fail the
                    // retrying endpoint until the engine's budget runs out.
                    // A simulated deployment crash itself has no budget.
                    if (retirement_window == 0 || attempt > 0) && refused.load(Ordering::SeqCst) {
                        return Some(lash_restate_test::Refusal::Retryable);
                    }
                }
                None
            })),
            ..lash_restate_test::DeploymentHooks::default()
        };
        let Some(double) = double_with_hooks(tier, replay, seed + retirement_window, hooks).await
        else {
            return;
        };
        let dispatch_prefix = double.double.service_name("EffectGroupDispatch");
        let dispatch_service = double
            .double
            .server()
            .service_names()
            .into_iter()
            .find(|name| name.starts_with(&dispatch_prefix))
            .expect("the recorded dispatcher lane");
        if retirement_window > 0 {
            let point = if retirement_window == 1 {
                lash_restate_test::CrashPoint::BeforeCommand { index: 3 }
            } else {
                lash_restate_test::CrashPoint::BeforeFrame {
                    ty: lash_restate_test::protocol::MessageType::OutputCommand,
                }
            };
            double.double.server().crash_on(
                lash_restate_test::CrashRule::new(point)
                    .service(&dispatch_service)
                    .handler("retire"),
            );
        }
        let (finish, wait_for_finish) = tokio::sync::watch::channel(false);
        let final_model_started = Arc::new(AtomicBool::new(false));
        let model_started = Arc::clone(&final_model_started);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let provider = lash::testing::TestProvider::builder()
            .kind(KIND)
            .complete(move |_| {
                let index = counted.fetch_add(1, Ordering::SeqCst);
                let mut wait = wait_for_finish.clone();
                let started = Arc::clone(&model_started);
                async move {
                    if index == 0 {
                        return Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "dispatch-ask-1".to_string(),
                                tool_name: ASK_MODEL.to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        });
                    }
                    if handler == "retire" && index > 1 {
                        started.store(true, Ordering::SeqCst);
                        while !*wait.borrow_and_update() {
                            if wait.changed().await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(text("dispatch completed"))
                }
            })
            .build()
            .into_handle();
        let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", provider));
        let tools: Arc<dyn lash::plugins::PluginFactory> =
            Arc::new(lash::plugins::StaticPluginFactory::new(
                lash::plugins::PluginDeclaration::initial("keys-dispatch-model"),
                lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(AskModel {
                    catalog: Arc::clone(&catalog),
                    retired: AtomicBool::new(true),
                    settled: Arc::default(),
                })),
            ));
        let core = core_over(&double, &catalog, vec![tools]);
        let session_id = "keys-dispatch-work";
        let mut session = None;
        let work = if process_opener {
            double.double.install_process_worker(
                lash::durability::DurableProcessWorker::new(
                    core.durable_process_worker_config()
                        .expect("worker configuration"),
                )
                .expect("the process worker builds"),
            );
            let started = start_on(
                &double,
                &core,
                "keys-dispatch-start",
                session_turn_start(
                    session_id,
                    "run the tool",
                    &lash::SessionSpec::new(
                        KIMI,
                        lash::TurnBudget::Unbounded,
                        lash::MaxToolCalls::new(1024),
                    ),
                ),
            )
            .await
            .expect("start the process-backed turn");
            lash::ParkedWorkRef::Process {
                process_id: started.process_id,
            }
        } else {
            let created = created_on(&core, session_id, KIMI).await;
            created
                .send(TurnInput::text("run the tool"))
                .id("dispatch-work-root")
                .await
                .expect("the root is accepted");
            session = Some(created);
            lash::ParkedWorkRef::Turn {
                session_id: lash::SessionId::from(session_id),
                turn_id: lash::TurnId::from("dispatch-work-root"),
            }
        };
        if handler == "retire" {
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                while !final_model_started.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the opener waits after its tool group closed");
            let target = double
                .double
                .server()
                .invocations()
                .into_iter()
                .find(|view| {
                    view.target.starts_with(&dispatch_service) && view.target.ends_with("/run")
                })
                .expect("the group dispatcher ran")
                .target;
            let group_key = target
                .strip_prefix(&format!("{dispatch_service}/"))
                .and_then(|target| target.strip_suffix("/run"))
                .expect("group key");
            double
                .double
                .ingress()
                .send_workflow_json(
                    &dispatch_service,
                    group_key,
                    "retire",
                    &lash_restate::Call::new(group_key.to_string()),
                )
                .await
                .expect("retire the closed group while its opener stays live");
        }
        let parked = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if let Some(view) = double
                    .double
                    .server()
                    .invocations()
                    .into_iter()
                    .find(|view| {
                        view.target.starts_with(&dispatch_service)
                            && view.target.ends_with(&format!("/{handler}"))
                            && view.status == "paused"
                    })
                {
                    break view;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the dispatcher invocation exhausts its retry loop in retirement window {retirement_window}: {error:?}"
            )
        });
        if retirement_window > 0 {
            let group_key = parked
                .target
                .strip_prefix(&format!("{dispatch_service}/"))
                .and_then(|target| target.strip_suffix("/retire"))
                .expect("group key");
            let state = double
                .double
                .server()
                .object_state(&double.double.service_name("EffectGroupIndex"), group_key);
            let state = state
                .values()
                .map(|value| String::from_utf8_lossy(value))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                state.contains("retired")
                    && state.contains(if retirement_window == 1 {
                        "pending"
                    } else {
                        "complete"
                    }),
                "the stopped retirement reached its cleanup window: {state}"
            );
        }
        reconcile_pass(&double).await;
        let listed = core
            .parked_work()
            .list(&lash::ParkedWorkQuery::all(
                std::num::NonZeroUsize::new(8).expect("non-zero"),
            ))
            .await
            .expect("list parked work")
            .records;
        let [park] = listed.as_slice() else {
            panic!("paused {handler} parks its opener: {listed:?}");
        };
        assert_eq!(park.target, work);
        assert_eq!(park.attempts, 1);
        assert_eq!(
            park.reason.code(),
            lash::persistence::ParkReasonCode::EngineRetryExhausted
        );
        let before = listed_parks(&core).await;
        if !replay
            && handler == "run"
            && let lash::ParkedWorkRef::Process { process_id } = &work
        {
            replay_waiting_process(&double, process_id).await;
        }
        reconcile_pass(&double).await;
        assert_eq!(
            listed_parks(&core).await,
            before,
            "dispatcher reconciliation is idempotent"
        );
        refusing.store(false, Ordering::SeqCst);
        double.double.server().clear_crashes();
        core.parked_work()
            .redrive(&work, park.park_id)
            .await
            .expect("the park surface resumes dispatcher work");
        if handler == "retire" {
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                loop {
                    if double
                        .double
                        .server()
                        .invocations()
                        .iter()
                        .find(|view| view.id == parked.id)
                        .is_some_and(|view| view.status == "completed")
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the redrive finishes the same retirement invocation");
            finish
                .send(true)
                .expect("release the opener after retirement");
        }
        if let Some(session) = session {
            assert_eq!(
                answer_after_redrive(&session, "dispatch-work-root").await,
                "dispatch completed"
            );
        } else if let lash::ParkedWorkRef::Process { process_id } = &work {
            let registry = double.double.lash_backend().process_registry();
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                loop {
                    let record = registry
                        .get_process(process_id)
                        .await
                        .expect("read")
                        .expect("retained");
                    if record.is_terminal() {
                        assert_eq!(record.status(), lash_core::ProcessStatus::Completed);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the process completes after the dispatcher resumes");
        }
        assert!(listed_parks(&core).await.is_empty());
        assert_eq!(
            double
                .double
                .server()
                .invocations()
                .iter()
                .find(|view| view.id == parked.id)
                .map(|view| view.status),
            Some("completed")
        );
        assert!(
            attempts.load(Ordering::SeqCst) > 1,
            "the fault exercised retry and replay"
        );
    }
}

macro_rules! dispatch_law {
    ($law:ident, $process:literal, $handler:literal) => {
        pub(super) async fn $law(tier: Tier, replay: bool, seed: u64) {
            paused_group_dispatch_work_parks_its_opener(tier, replay, seed, $process, $handler)
                .await;
        }
    };
}

dispatch_law!(a_paused_group_run_parks_its_root_opener, false, "run");
dispatch_law!(a_paused_group_run_parks_its_process_opener, true, "run");
dispatch_law!(a_paused_group_retire_parks_its_root_opener, false, "retire");
dispatch_law!(
    a_paused_group_retire_parks_its_process_opener,
    true,
    "retire"
);
