//! Durable events preserve their resolution and cancellation through handover.
use super::*;

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
