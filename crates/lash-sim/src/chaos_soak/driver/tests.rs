use super::*;

/// A rolling deploy's drain charges its ticks to time, never to work
/// (FIG-4624). One drive of the first build is calling a backlog of roots
/// when the build is rolled, and each root takes wall time: more of it,
/// in all, than thirty quiesce budgets. The drive needs no tick to end,
/// so the generation drains within [`DRAIN_TICKS`] however long the
/// roots take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_charges_no_tick_to_roots_the_old_build_is_still_driving() {
    const ROOTS: usize = 32;
    let seed = 0x4624;
    let mut driver = Driver::new(seed).await.expect("world");
    driver
        .step(
            seed,
            &Step::Open {
                session: 0,
                lane: Lane::Plain,
                parent: None,
            },
        )
        .await
        .expect("open the session");
    for root in 0..ROOTS {
        driver
            .step(
                seed,
                &Step::Send {
                    session: 0,
                    root: format!("slow-{root}"),
                },
            )
            .await
            .expect("send a slow root");
    }
    let old = driver.deployment.clone();
    let drive = format!("LashSession/{}/drive running", driver.ledger.sessions[0].id);
    assert!(
        pinned_open(&driver.world, &old).contains(&drive),
        "the first build's drive is still calling the backlog"
    );

    let rolled = driver
        .step(seed, &Step::Roll)
        .await
        .expect("the old generation drains");

    assert!(pinned_open(&driver.world, &old).is_empty(), "{rolled}");
    assert_eq!(driver.ledger.retired.len(), 1, "{rolled}");
    let backend = driver.world.backend();
    for root in 0..ROOTS {
        let root = lash_core::TurnId::from(format!("slow-{root}"));
        assert!(
            backend
                .session_store_factory()
                .root_terminal(&driver.ledger.sessions[0].id, &root)
                .await
                .expect("read the root's terminal")
                .is_some(),
            "`{root}` ran to its terminal before the build was removed"
        );
    }
}

/// A drive attempt that replays while the root it called is still
/// running waits for that root on the engine, as its first attempt did
/// (FIG-4729). The drive's attempt is dropped under a held root, whose
/// run goes on in the same process on the runtime the drive held. The
/// replay re-serves its recorded admission and waits on its recorded
/// call; it does not wait in the process for the runtime the root runs
/// on, which a root that never answers never gives back. So a rolling
/// deploy's drain, which ticks once the engine's work has settled, ends
/// the deleted session's root and retires the old build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_replayed_under_its_running_root_waits_on_the_engine() {
    let seed = 0x4729;
    let mut driver = Driver::new(seed).await.expect("world");
    driver
        .step(
            seed,
            &Step::Open {
                session: 0,
                lane: Lane::Held,
                parent: None,
            },
        )
        .await
        .expect("open the session");
    let id = driver.ledger.sessions[0].id.clone();
    driver
        .send_held(&id, "held-0", false)
        .await
        .expect("send the held root");
    assert!(driver.reached("held-0"), "the root is in its model call");
    let target = format!("LashSession/{id}/drive");
    let drive = |driver: &Driver| {
        driver
            .world
            .double()
            .expect("double")
            .server()
            .invocations()
            .into_iter()
            .find(|view| view.target == target)
            .expect("the session's drive")
    };
    let first = drive(&driver);
    assert_eq!((first.status, first.attempts), ("running", 1), "{first:?}");
    assert!(
        driver.world.drop_attempt(&first.id).expect("double"),
        "the drive's first attempt was running"
    );

    let settled = tokio::time::Instant::now() + Duration::from_secs(10);
    while driver.works().expect("double") && tokio::time::Instant::now() < settled {
        driver.world.quiesce().await;
    }
    let replayed = drive(&driver);
    assert_eq!(
        (
            replayed.status,
            replayed.attempts,
            replayed.blocked_on_server
        ),
        ("running", 2, Some(true)),
        "the replayed drive waits on the engine for the root it called: {replayed:?}"
    );
    assert!(driver.reached("held-0"), "the root runs on");

    let old = driver.deployment.clone();
    let deleted = driver.delete(0).await.expect("delete the session");
    let rolled = driver
        .step(seed, &Step::Roll)
        .await
        .expect("the old generation drains");
    assert!(
        pinned_open(&driver.world, &old).is_empty(),
        "{deleted}; {rolled}"
    );
    assert_eq!(driver.ledger.retired.len(), 1, "{deleted}; {rolled}");
    driver.world.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_answer_drives_its_manual_time_retry() {
    let mut driver = Driver::new(0x4402).await.expect("world");
    let engine = driver.world.double().expect("double").clone();
    let server = engine.server().clone();
    let before = server.now_ms();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        driver.host(async move {
            engine
                .run_crashed_then_redriven(
                    lash_core::AdmittedScope::runtime_operation("retry-budget"),
                    Arc::new(|_| Box::pin(async { panic!("first host attempt fails") })),
                    Arc::new(|_| Box::pin(async {})),
                )
                .await
        }),
    )
    .await;
    let timers = server.timers();
    let advanced = server.now_ms().saturating_sub(before);
    driver.world.finish().await;
    assert!(result.is_ok(), "host waits for virtual retry: {timers:?}");
    assert_eq!(
        result.expect("bounded answer").expect("host"),
        (Some(Ok(())), false)
    );
    assert!(
        advanced >= 500,
        "preserve the retry's backoff: {advanced}ms"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_before_host_admission_is_not_missed() {
    let mut driver = Driver::new(0x4402).await.expect("world");
    driver
        .world
        .trip()
        .fire("between the step and host admission");
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        driver.host(std::future::pending::<()>()),
    )
    .await;
    driver.world.finish().await;
    assert!(
        result.is_ok(),
        "an already-fired crash must interrupt host work instead of waiting for another crash"
    );
    assert_eq!(
        result.expect("bounded host call").expect("restart"),
        (None, true)
    );
    assert_eq!(driver.counts.crashes, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_history_a_refused_send_is_a_typed_host_fact() {
    let seed = 0x4713;
    let mut driver = Driver::new(seed).await.expect("world");
    driver
        .step(
            seed,
            &Step::Open {
                session: 0,
                lane: Lane::Plain,
                parent: None,
            },
        )
        .await
        .expect("open");
    driver
        .step(seed, &Step::Delete { session: 0 })
        .await
        .expect("delete");
    driver
        .step(
            seed,
            &Step::Send {
                session: 0,
                root: "refused".to_owned(),
            },
        )
        .await
        .expect("refused step");
    driver
        .step(
            seed,
            &Step::Open {
                session: 1,
                lane: Lane::Plain,
                parent: None,
            },
        )
        .await
        .expect("second open");
    driver
        .step(
            seed,
            &Step::ArmHostRefusal {
                site: crate::crash_matrix::deployment::HostSite::AdmitInputsBefore,
            },
        )
        .await
        .expect("arm refusal");
    driver
        .step(
            seed,
            &Step::Send {
                session: 1,
                root: "fenced".to_owned(),
            },
        )
        .await
        .expect("refused admission");
    let facts = serde_json::to_value(driver.world.history().facts()).expect("facts");
    let mut history = crate::invariants::History::new("chaos-soak", seed);
    history.extend_from(driver.world.history());
    history
        .capture_store_with_transcripts("engine", driver.world.sqlite_stores().expect("SQLite"))
        .await
        .expect("snapshot");
    let admission = crate::invariants::CHECKERS
        .iter()
        .find(|checker| checker.invariant() == "host-admission")
        .expect("admission checker");
    let report = crate::invariants::check_with(&history, &[*admission]);
    driver.world.finish().await;
    assert!(report.passed(), "{}", report.failure());
    for (root, code) in [("refused", "session_deleted"), ("fenced", "writer_fenced")] {
        let refused = facts
            .as_array()
            .expect("fact array")
            .iter()
            .find(|fact| fact["fact"] == "host_op" && fact["roots"] == serde_json::json!([root]))
            .expect("a refused send is retained in history");
        assert_eq!(refused["outcome"]["refused"]["code"]["runtime"], code);
    }
    assert!(
        facts.as_array().expect("fact array").iter().any(|fact| {
            fact["fact"] == "fault"
                && fact["kind"] == "refusal"
                && fact["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.starts_with("taken"))
        }),
        "the armed refusal fired"
    );
}
