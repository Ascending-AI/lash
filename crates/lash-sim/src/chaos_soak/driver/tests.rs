use super::*;
use crate::crash_matrix::deployment::HostSite;

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

/// The same replay on a session a host holds open (FIG-4755). The host's
/// open session is the runtime the drive's root runs on, and the root
/// keeps that runtime's writer for as long as it runs. The drive's
/// attempt is dropped under the held root: its replay re-serves the
/// recorded admission and waits on its recorded call. An admission reads
/// the session's store and takes no runtime's writer, the host's
/// included, so the replay does not wait in the process for a root that
/// never answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_replayed_under_a_root_on_a_resident_session_waits_on_the_engine() {
    /// The server's inactivity timeout (its default, which the double
    /// keeps).
    const INACTIVITY: Duration = Duration::from_secs(60);
    let seed = 0x4755;
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
    // The host holds the session open: the engine drives it on the
    // host's runtime from here on.
    let resident = driver
        .world
        .core()
        .expect("core")
        .session(id.clone())
        .open()
        .await
        .expect("the host opens the session");
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

    // An attempt that waits on the engine is one the server suspends
    // when its inactivity timeout passes: no attempt fails, so however
    // long the root runs, the wait never spends the drive's retries.
    driver
        .world
        .engine()
        .advance(INACTIVITY + Duration::from_secs(1));
    driver.world.quiesce().await;
    let suspended = drive(&driver);
    assert_eq!(
        (suspended.status, suspended.attempts, suspended.retry_count),
        ("suspended", 2, 0),
        "the waiting drive is suspended, not retried: {suspended:?}"
    );
    assert!(driver.reached("held-0"), "the root runs on");

    // The host lets the session go and deletes it: the close ends the
    // root, and the drive that waited for it on the engine ends with it.
    drop(resident);
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

/// Open plain session 0 of a fresh world.
async fn park_world(seed: u64) -> (Driver, SessionId) {
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
    let id = driver.ledger.sessions[0].id.clone();
    (driver, id)
}

/// Tick recovery until `root` has its terminal, then judge the world's
/// history as the soak's end does.
async fn redriven_history(
    driver: &mut Driver,
    session: &SessionId,
    root: &lash_core::TurnId,
) -> crate::invariants::Report {
    let store = driver.world.backend().session_store_factory();
    for _ in 0..8 {
        driver.world.quiesce().await;
        if store
            .root_terminal(session, root)
            .await
            .expect("read the root's terminal")
            .is_some()
        {
            break;
        }
        driver.tick().await.expect("a recovery tick");
    }
    assert!(
        store
            .root_terminal(session, root)
            .await
            .expect("read the root's terminal")
            .is_some(),
        "the redriven root `{root}` ran to its terminal"
    );
    let report = crate::invariants::report_crash_world(&driver.world, "chaos-soak")
        .await
        .expect("capture the history")
        .expect("a SQLite world");
    driver.world.finish().await;
    report
}

fn facts_of(report_facts: &[Fact], fact: &str) -> Vec<serde_json::Value> {
    report_facts
        .iter()
        .map(|recorded| serde_json::to_value(recorded).expect("a fact encodes"))
        .filter(|recorded| recorded["fact"] == fact)
        .collect()
}

fn redrives_observed(report: &crate::invariants::Report) -> usize {
    report
        .observed
        .iter()
        .find(|(invariant, _)| *invariant == "redrive-resumes")
        .expect("the redrive checker is registered")
        .1
}

/// FIG-4718: a root whose runs fail until the turn handler stops retrying
/// them is parked by the recovery pass, and the host's redrive of that park
/// resumes the execution the engine held: the store acknowledges the
/// redrive's intent, the root commits, and its commit ends the park. The
/// redrive checker judges that one redrive from the park feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_root_is_redriven_and_resumes() {
    let seed = 0x4718;
    let (mut driver, id) = park_world(seed).await;
    let redriven = driver
        .park_and_redrive(&id, "park-1")
        .await
        .expect("the park step");
    let Redriven {
        admission: Admission::Known,
        park: Some((root, Admission::Known)),
    } = redriven.clone()
    else {
        panic!("the root parked and its redrive was accepted: {redriven:?}");
    };
    let facts = driver.world.history().facts();
    let report = redriven_history(&mut driver, &id, &root).await;
    assert!(
        report.passed(),
        "{}\n{}",
        report.summary(),
        report.failure()
    );
    assert_eq!(redrives_observed(&report), 1, "{}", report.summary());
    let resumes = facts_of(&facts, "resume");
    assert_eq!(resumes.len(), 1, "{resumes:?}");
    assert_eq!(
        (&resumes[0]["root"], &resumes[0]["held"]),
        (
            &serde_json::json!(root.to_string()),
            &serde_json::json!(true)
        ),
        "the engine resumed the execution it had stopped retrying"
    );
    let acknowledged = facts_of(&facts, "intent_ack");
    assert_eq!(acknowledged.len(), 1, "{acknowledged:?}");
    assert_eq!(
        (&acknowledged[0]["verb"], &acknowledged[0]["applied"]),
        (&serde_json::json!("redrive"), &serde_json::json!(true)),
        "the redrive's engine half ran and was acknowledged"
    );
    assert!(
        facts_of(&facts, "fault").iter().any(|fault| {
            fault["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("RunRootBefore"))
        }),
        "the park came from the run-root seam"
    );
}

/// FIG-4718: a host that dies after the engine resumed the root and before
/// the store acknowledged the redrive's intent never hears the redrive
/// accepted. The resumed root commits, which ends its park. The intent is
/// durable, so the recovery pass of the next deployment claims it again,
/// finds the root ran past it and settles it acknowledged without resuming
/// the root a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redrive_whose_host_dies_before_its_acknowledgement_is_delivered_again() {
    let seed = 0x4718_0002;
    let (mut driver, id) = park_world(seed).await;
    driver
        .world
        .faults()
        .crash_once(HostSite::AcknowledgeIntentBefore);
    let redriven = driver
        .park_and_redrive(&id, "park-1")
        .await
        .expect("the park step");
    let Redriven {
        admission: Admission::Known,
        park: Some((root, Admission::Maybe)),
    } = redriven.clone()
    else {
        panic!("the host never heard its redrive accepted: {redriven:?}");
    };
    assert_eq!(driver.counts.crashes, 1, "the host died inside the redrive");
    let report = redriven_history(&mut driver, &id, &root).await;
    let facts = driver.world.history().facts();
    assert!(
        report.passed(),
        "{}\n{}",
        report.summary(),
        report.failure()
    );
    assert_eq!(redrives_observed(&report), 1, "{}", report.summary());
    assert!(
        !facts_of(&facts, "resume").is_empty(),
        "the engine was asked to resume the root"
    );
    let acknowledged = facts_of(&facts, "intent_ack");
    assert_eq!(
        acknowledged.len(),
        1,
        "the next deployment acknowledged the intent once: {acknowledged:?}"
    );
    assert_eq!(
        (&acknowledged[0]["verb"], &acknowledged[0]["applied"]),
        (&serde_json::json!("redrive"), &serde_json::json!(false)),
        "the root ran past the redrive, so the store settled it without resuming again"
    );
}
