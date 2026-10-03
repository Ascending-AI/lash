//! Durable sleeps retain their deadline through the existing cell boundary.
use super::*;

async fn sleep_hands_over(storage: Storage, crash: Option<Crash>, cancel: bool) -> Result<()> {
    sleep_with_clock_crash(storage, crash, cancel, None).await
}

async fn sleep_with_clock_crash(
    storage: Storage,
    crash: Option<Crash>,
    cancel: bool,
    clock_crash: Option<lash_restate_test::CrashPoint>,
) -> Result<()> {
    let session = "cell-sleep-handover";
    let code = cell(&format!("go-{session}")).replace(
        "const answer = await handle;",
        "await sleep(60000); const answer = await handle;",
    );
    let crashes = lash_restate_test::CrashCount::new();
    let mut roll = CellRoll::start_with_cell_setup(storage, session, code, |server| {
        assert!(server.on_crash(crashes.listener()));
        if let Some(point) = clock_crash.clone() {
            server.crash_on(
                lash_restate_test::CrashRule::new(point)
                    .service(lash_restate_test::TURN_DRIVER_SERVICE)
                    .handler("run"),
            );
        }
    })
    .await?;
    let parked = roll.parked_run("run-run", 0).await;
    let timer = roll
        .server()
        .timers()
        .into_iter()
        .find(|timer| timer.invocation == parked.id)
        .expect("the cell's sleep is armed");
    match crash {
        Some(Crash::OldRunWhileParked) => {
            assert!(roll.server().crash(&parked.id));
            roll.parked_run("run-run", 0).await;
        }
        Some(crash) => {
            let commands = roll
                .server()
                .journal(&parked.id)
                .expect("parked journal")
                .iter()
                .filter(|entry| entry.ty.is_command())
                .count();
            let old_key = lash_restate::recorded_turn_invocation_key(
                roll.core.store_factory.as_ref(),
                &roll.session,
                &lash_core::TurnId::fixture("run-run"),
            )
            .await?
            .expect("the parked run records its invocation");
            if let Some(rule) = crash.rule(commands, Some(&old_key)) {
                roll.server().crash_on(rule);
            }
        }
        None => {}
    }
    roll.server().advance(std::time::Duration::from_secs(10));
    roll.hand_over().await?;
    let resumed = roll.parked_run(CONTINUATION, 0).await;
    let successor_timer = roll
        .server()
        .timers()
        .into_iter()
        .find(|timer| timer.invocation == resumed.id)
        .expect("the resumed sleep is armed");
    assert!(
        timer.fire_at_ms.abs_diff(successor_timer.fire_at_ms) < 1000,
        "handover must preserve the absolute sleep deadline: {timer:?} vs {successor_timer:?}"
    );
    if matches!(crash, Some(Crash::ContinuationWhileParked)) {
        assert!(roll.server().crash(&resumed.id));
        roll.parked_run(CONTINUATION, 0).await;
    }
    if cancel {
        roll.handle
            .as_ref()
            .expect("send handle")
            .cancel()
            .origin("sleep-handover")
            .await?;
        assert_eq!(
            tokio::time::timeout(WEDGE, roll.sent().outcome())
                .await
                .expect("cancel answers")?
                .status(),
            crate::TurnStatus::Cancelled
        );
        return Ok(());
    }
    roll.release_process().await?;
    roll.server().advance_to(timer.fire_at_ms + 1000);
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::json!({"answer":"done", "before":42}),
        })
    );
    assert_eq!(roll.requests.lock_recover().len(), 1);
    assert_eq!(roll.started().await, 1);
    if crash.is_some() || clock_crash.is_some() {
        assert_eq!(crashes.get(), 1, "the selected crash executed once");
    }
    roll.assert_ended().await
}

macro_rules! sleep_clock_laws {
    ($($name:ident: $storage:expr, $point:ident;)*) => { $(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() -> Result<()> {
            sleep_with_clock_crash($storage, None, false,
                Some(lash_restate_test::CrashPoint::$point { suffix: "sleep-clock".into() })).await
        }
    )* };
}
sleep_clock_laws! {
    sleep_crash_before_clock_sqlite_memory: Storage::SqliteMemory, BeforeRunEnding;
    sleep_crash_before_clock_result_sqlite_memory: Storage::SqliteMemory, BeforeRunResultEnding;
    sleep_crash_before_clock_sqlite_file: Storage::SqliteFile, BeforeRunEnding;
    sleep_crash_before_clock_result_sqlite_file: Storage::SqliteFile, BeforeRunResultEnding;
}

macro_rules! sleep_laws {
    ($($name:ident: $storage:expr, $crash:expr, $cancel:expr;)*) => { $(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() -> Result<()> { sleep_hands_over($storage, $crash, $cancel).await }
    )* };
}

sleep_laws! {
    sleep_hands_over_sqlite_memory: Storage::SqliteMemory, None, false;
    sleep_crash_old_run_while_parked_sqlite_memory: Storage::SqliteMemory, Some(Crash::OldRunWhileParked), false;
    sleep_crash_old_run_after_the_wake_sqlite_memory: Storage::SqliteMemory, Some(Crash::OldRunAfterTheWake), false;
    sleep_crash_old_run_after_its_boundary_commit_sqlite_memory: Storage::SqliteMemory, Some(Crash::OldRunAfterItsBoundaryCommit), false;
    sleep_crash_continuation_before_its_first_step_sqlite_memory: Storage::SqliteMemory, Some(Crash::ContinuationBeforeItsFirstStep), false;
    sleep_crash_continuation_while_parked_sqlite_memory: Storage::SqliteMemory, Some(Crash::ContinuationWhileParked), false;
    sleep_crash_continuation_after_its_commit_sqlite_memory: Storage::SqliteMemory, Some(Crash::ContinuationAfterItsCommit), false;
    sleep_cancel_after_sqlite_memory: Storage::SqliteMemory, None, true;
    sleep_hands_over_sqlite_file: Storage::SqliteFile, None, false;
    sleep_crash_old_run_while_parked_sqlite_file: Storage::SqliteFile, Some(Crash::OldRunWhileParked), false;
    sleep_crash_old_run_after_the_wake_sqlite_file: Storage::SqliteFile, Some(Crash::OldRunAfterTheWake), false;
    sleep_crash_old_run_after_its_boundary_commit_sqlite_file: Storage::SqliteFile, Some(Crash::OldRunAfterItsBoundaryCommit), false;
    sleep_crash_continuation_before_its_first_step_sqlite_file: Storage::SqliteFile, Some(Crash::ContinuationBeforeItsFirstStep), false;
    sleep_crash_continuation_while_parked_sqlite_file: Storage::SqliteFile, Some(Crash::ContinuationWhileParked), false;
    sleep_crash_continuation_after_its_commit_sqlite_file: Storage::SqliteFile, Some(Crash::ContinuationAfterItsCommit), false;
    sleep_cancel_after_sqlite_file: Storage::SqliteFile, None, true;
}

#[derive(Clone, Copy)]
enum EventCrash {
    OldWhileParked,
    OldAfterWake,
    NextBeforeWait,
    NextAfterWait,
}

async fn event_crosses_handover(storage: Storage, resolve_before_resume: bool) -> Result<()> {
    event_with_crash(storage, resolve_before_resume, None).await
}

async fn event_with_crash(
    storage: Storage,
    resolve_before_resume: bool,
    crash: Option<EventCrash>,
) -> Result<()> {
    event_with_retirement_check(storage, resolve_before_resume, crash, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_handover_retires_predecessor_subscription_sqlite_memory() -> Result<()> {
    event_with_retirement_check(Storage::SqliteMemory, false, None, true).await
}

async fn event_with_retirement_check(
    storage: Storage,
    resolve_before_resume: bool,
    crash: Option<EventCrash>,
    remove_predecessor_deployment: bool,
) -> Result<()> {
    let World { engine, _keep, .. } = double_world(storage).await;
    let Engine::Double(double) = &engine else {
        unreachable!("double law")
    };
    let core = cell_core(
        engine.old_backend(),
        engine.old_work(),
        typescript_block("finish(null);"),
        &Arc::default(),
    );
    let session = lash_core::SessionId::fixture("event-handover");
    let _session = core.session(session.clone()).created().await.open().await?;
    let scope =
        lash_core::AdmittedScope::turn(session.clone(), lash_core::TurnId::fixture("event-first"));
    let resolver = engine.old_backend().effect_host();
    let key = resolver
        .await_event_key(
            scope.scope(),
            lash_core::AwaitEventWaitIdentity::Custom {
                key: "same-event".into(),
            },
        )
        .await?;
    let crashes = lash_restate_test::CrashCount::new();
    assert!(double.server().on_crash(crashes.listener()));
    let first_key = key.clone();
    let first = double.run_in_handler(
        scope,
        Arc::new(move |controller| {
            let key = first_key.clone();
            Box::pin(async move {
                assert_eq!(
                    event_wait(&controller, key)
                        .await
                        .expect_err("event hands over")
                        .code,
                    lash_core::RuntimeErrorCode::TurnWaitHandedOver
                );
            })
        }),
    );
    let next = BuildGeneration::for_test("event-next");
    let transfer = async {
        let parked = parked_event_handler(double.server()).await;
        if matches!(crash, Some(EventCrash::OldWhileParked)) {
            assert!(double.server().crash(&parked.id));
            parked_event_handler(double.server()).await;
        }
        if matches!(crash, Some(EventCrash::OldAfterWake)) {
            double.server().crash_on(event_output_crash(double));
        }
        double
            .add_build(
                next,
                "event-next",
                lash_restate_test::DeploymentHooks::default(),
            )
            .await
            .expect("next build");
        hand_over_event(double, &session).await;
    };
    let (ended, ()) = tokio::join!(first, transfer);
    ended.expect("old event handler returns");
    if remove_predecessor_deployment {
        let old_handler = double
            .server()
            .invocations()
            .into_iter()
            .find(|view| view.target.starts_with("LashTestHandlerHost/"))
            .expect("the predecessor handler");
        let old_deployment = double
            .server()
            .pinned_deployment(&old_handler.id)
            .expect("the predecessor deployment");
        double.server().remove_deployment(&old_deployment, false)
            .unwrap_or_else(|error| panic!(
                "handover must retire the predecessor subscription before the event resolves: {error:?}; {:?}",
                double.server().invocations()));
    }
    let resolution = lash_core::Resolution::Ok(serde_json::json!({"event":42}));
    if resolve_before_resume {
        assert_eq!(
            resolver
                .resolve_await_event(&key, resolution.clone())
                .await?,
            lash_core::ResolveOutcome::Accepted
        );
    }
    match crash {
        Some(EventCrash::NextBeforeWait) => double.server().crash_on(
            lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
                ty: lash_restate_test::protocol::MessageType::CallCommand,
            })
            .service(double.service_name("LashTestHandlerHost"))
            .handler("run"),
        ),
        Some(EventCrash::NextAfterWait) => double.server().crash_on(event_output_crash(double)),
        _ => {}
    }
    let successor_key = key.clone();
    let expected = resolution.clone();
    let successor = double.run_in_handler(lash_core::AdmittedScope::turn(session, lash_core::TurnId::fixture("event-next")),
        Arc::new(move |controller| {
            let key = successor_key.clone();
            let expected = expected.clone();
            Box::pin(async move {
                assert!(matches!(event_wait(&controller, key).await.expect("event resumes"),
                    lash_core::RuntimeEffectOutcome::AwaitEvent { resolution } if resolution == expected));
            })
        }));
    let resolve = async {
        if !resolve_before_resume {
            parked_event_handler(double.server()).await;
            assert_eq!(
                resolver
                    .resolve_await_event(&key, resolution.clone())
                    .await
                    .expect("resolve"),
                lash_core::ResolveOutcome::Accepted
            );
        }
    };
    let (result, ()) = tokio::join!(successor, resolve);
    result.expect("successor returns");
    assert_ne!(
        resolver.resolve_await_event(&key, resolution).await?,
        lash_core::ResolveOutcome::Accepted,
        "the event resolves once across the boundary"
    );
    if crash.is_some() {
        assert_eq!(crashes.get(), 1, "the selected event crash executed once");
    }
    let handlers = double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with("LashTestHandlerHost/"))
        .collect::<Vec<_>>();
    assert_eq!(handlers.len(), 2);
    assert_ne!(
        handlers[0].pinned_deployment_id, handlers[1].pinned_deployment_id,
        "the successor runs on the next build"
    );
    Ok(())
}

async fn parked_event_handler(
    server: &lash_restate_test::RestateTestServer,
) -> lash_restate_test::InvocationView {
    let until = tokio::time::Instant::now() + WEDGE;
    let mut seen = None;
    loop {
        if let Some(view) = server.invocations().into_iter().find(|view| {
            view.target.starts_with("LashTestHandlerHost/")
                && view.status == "running"
                && view.blocked_on_server == Some(true)
        }) {
            if seen == Some(view.journal_len) {
                return view;
            }
            seen = Some(view.journal_len);
        }
        assert!(
            tokio::time::Instant::now() < until,
            "event handler never parked: {:?}",
            server.invocations()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn event_output_crash(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
) -> lash_restate_test::CrashRule {
    lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
        ty: lash_restate_test::protocol::MessageType::OutputCommand,
    })
    .service(double.service_name("LashTestHandlerHost"))
    .handler("run")
}

async fn hand_over_event(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    session: &lash_core::SessionId,
) {
    let until = tokio::time::Instant::now() + WEDGE;
    loop {
        let reply: lash_restate::Reply<u64> = double
            .ingress()
            .call_object_json(
                &double.service_name("LashDurableWaitIndex"),
                session.as_str(),
                "hand_over_turns",
                &lash_restate::Call::new(lash_restate::RestateDurableWaitHandOverRequest {
                    generation: double
                        .lash_backend()
                        .build_generation()
                        .expect("bound generation")
                        .clone(),
                }),
            )
            .await
            .expect("wake event handover");
        if reply.body == 1 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "event wait never registered handover"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn event_wait(
    controller: &lash_core::ScopedEffectController<'_>,
    key: lash_core::AwaitEventKey,
) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
    let scope = controller.execution_scope().clone();
    controller
        .execute_effect(
            lash_core::RuntimeEffectEnvelope::new(
                lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(scope.clone(), "same-event")
                        .expect("event address"),
                    lash_core::RuntimeAttribution::for_turn_admission(
                        scope.session_id().expect("session").clone(),
                        scope.turn_id().expect("turn").clone(),
                    ),
                    "same-event",
                ),
                lash_core::RuntimeEffectCommand::AwaitEvent { key },
            ),
            lash_core::RuntimeEffectLocalExecutor::await_event_under(
                &lash_core::runtime::TurnCancelWait::observing(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )
                .transferable(true),
                Arc::new(lash_core::facade_support::SystemClock),
            ),
        )
        .await
}

macro_rules! event_laws {
    ($($name:ident: $storage:expr, $early:expr;)*) => { $(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() -> Result<()> { event_crosses_handover($storage, $early).await }
    )* };
}
event_laws! {
    event_resolved_during_handover_sqlite_memory: Storage::SqliteMemory, true;
    event_resolved_after_handover_sqlite_memory: Storage::SqliteMemory, false;
    event_resolved_during_handover_sqlite_file: Storage::SqliteFile, true;
    event_resolved_after_handover_sqlite_file: Storage::SqliteFile, false;
}

macro_rules! event_crash_laws {
    ($($name:ident: $storage:expr, $crash:ident;)*) => { $(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() -> Result<()> { event_with_crash($storage, false, Some(EventCrash::$crash)).await }
    )* };
}
event_crash_laws! {
    event_crash_old_parked_sqlite_memory: Storage::SqliteMemory, OldWhileParked;
    event_crash_old_after_wake_sqlite_memory: Storage::SqliteMemory, OldAfterWake;
    event_crash_next_before_wait_sqlite_memory: Storage::SqliteMemory, NextBeforeWait;
    event_crash_next_after_wait_sqlite_memory: Storage::SqliteMemory, NextAfterWait;
    event_crash_old_parked_sqlite_file: Storage::SqliteFile, OldWhileParked;
    event_crash_old_after_wake_sqlite_file: Storage::SqliteFile, OldAfterWake;
    event_crash_next_before_wait_sqlite_file: Storage::SqliteFile, NextBeforeWait;
    event_crash_next_after_wait_sqlite_file: Storage::SqliteFile, NextAfterWait;
}

async fn sleep_cancel_races(storage: Storage) -> Result<()> {
    let session = "sleep-cancel-race";
    let code = cell(&format!("go-{session}")).replace(
        "const answer = await handle;",
        "await sleep(60000); const answer = await handle;",
    );
    let mut roll = CellRoll::start_with_cell(storage, session, code).await?;
    roll.parked_run("run-run", 0).await;
    let handle = roll.handle.as_ref().expect("send handle");
    let page = std::num::NonZeroUsize::new(16).expect("page");
    let cursor = lash_core::engine::ReconcileCursor::default();
    let (cancelled, reconciled) = tokio::join!(
        handle.cancel().origin("sleep-cancel-race"),
        roll.core._session_shifts.reconcile(&cursor, page)
    );
    cancelled?;
    reconciled?;
    assert_eq!(
        tokio::time::timeout(WEDGE, roll.sent().outcome())
            .await
            .expect("cancel answers")?
            .status(),
        crate::TurnStatus::Cancelled
    );
    // The process has session lifetime, so cancelling its starter's Run
    // leaves it waiting for its signal. End that independent work as well.
    roll.release_process().await?;
    let until = tokio::time::Instant::now() + WEDGE;
    while !roll
        .core
        .generation_drain_status(&roll.old)
        .await?
        .drained()
    {
        assert!(
            tokio::time::Instant::now() < until,
            "the signalled process must end"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    roll.assert_ended().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sleep_cancel_races_sqlite_memory() -> Result<()> {
    sleep_cancel_races(Storage::SqliteMemory).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sleep_cancel_races_sqlite_file() -> Result<()> {
    sleep_cancel_races(Storage::SqliteFile).await
}

async fn event_cancel_races(storage: Storage) -> Result<()> {
    let World { engine, _keep, .. } = double_world(storage).await;
    let Engine::Double(double) = &engine else {
        unreachable!("double law")
    };
    let core = cell_core(
        engine.old_backend(),
        engine.old_work(),
        typescript_block("finish(null);"),
        &Arc::default(),
    );
    let session = lash_core::SessionId::fixture("event-cancel-race");
    let _session = core.session(session.clone()).created().await.open().await?;
    let scope =
        lash_core::AdmittedScope::turn(session.clone(), lash_core::TurnId::fixture("event-cancel"));
    let resolver = engine.old_backend().effect_host();
    let key = resolver
        .await_event_key(
            scope.scope(),
            lash_core::AwaitEventWaitIdentity::Custom {
                key: "same-event".into(),
            },
        )
        .await?;
    let cancel_key = resolver
        .await_event_key(
            scope.scope(),
            lash_core::AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await?;
    let handed_over = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = handed_over.clone();
    let first_key = key.clone();
    let first = double.run_in_handler(
        scope.clone(),
        Arc::new(move |controller| {
            let key = first_key.clone();
            let observed = observed.clone();
            Box::pin(async move {
                match event_wait(&controller, key).await {
                    Err(error) if error.code == lash_core::RuntimeErrorCode::TurnWaitHandedOver => {
                        observed.store(true, Ordering::SeqCst);
                    }
                    Ok(lash_core::RuntimeEffectOutcome::AwaitEvent {
                        resolution: lash_core::Resolution::Cancelled,
                    }) => {}
                    outcome => panic!("cancel or handover must win: {outcome:?}"),
                }
            })
        }),
    );
    let race = async {
        parked_event_handler(double.server()).await;
        double
            .add_build(
                BuildGeneration::for_test("event-cancel-next"),
                "event-cancel-next",
                lash_restate_test::DeploymentHooks::default(),
            )
            .await
            .expect("next build");
        let cancel = resolver.resolve_await_event(&cancel_key, lash_core::Resolution::Cancelled);
        let ingress = double.ingress();
        let service = double.service_name("LashDurableWaitIndex");
        let request = lash_restate::Call::new(lash_restate::RestateDurableWaitHandOverRequest {
            generation: double
                .lash_backend()
                .build_generation()
                .expect("bound generation")
                .clone(),
        });
        let transfer = ingress.call_object_json::<_, lash_restate::Reply<u64>>(
            &service,
            session.as_str(),
            "hand_over_turns",
            &request,
        );
        let (cancelled, transferred) = tokio::join!(cancel, transfer);
        cancelled.expect("cancel resolves");
        transferred.expect("handover races cancellation");
    };
    let (result, ()) = tokio::join!(first, race);
    result.expect("old handler ends");
    if handed_over.load(Ordering::SeqCst) {
        let key = key.clone();
        double
            .run_in_handler(
                scope,
                Arc::new(move |controller| {
                    let key = key.clone();
                    Box::pin(async move {
                        assert!(matches!(
                            event_wait(&controller, key)
                                .await
                                .expect("cancelled successor"),
                            lash_core::RuntimeEffectOutcome::AwaitEvent {
                                resolution: lash_core::Resolution::Cancelled
                            }
                        ));
                    })
                }),
            )
            .await
            .expect("successor ends");
    }
    assert_eq!(
        resolver.peek_await_event(&key).await?,
        Some(lash_core::Resolution::Cancelled)
    );
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_cancel_races_sqlite_memory() -> Result<()> {
    event_cancel_races(Storage::SqliteMemory).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_cancel_races_sqlite_file() -> Result<()> {
    event_cancel_races(Storage::SqliteFile).await
}
