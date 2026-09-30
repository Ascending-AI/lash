//! FIG-4281: an open root whose `LashTurn` run the engine no longer holds on
//! any lane has an autonomous recovery owner. The recovery pass ends a root
//! that started (it recorded its admission) `SubstrateLost`, with its input
//! disposition and scope close, and never runs it again under a fresh
//! journal; a root that never started is left to its input's ingress
//! obligation, which drives it once.
//!
//! Each law runs on the server double over the world's stores: SQLite, or
//! PostgreSQL when `LASH_POSTGRES_DATABASE_URL` names a database.

use super::*;

/// The `LashTurn` `run` invocations the engine holds for `root`, in any
/// status: the root's executions, never its `outcome` reads or its `close`.
async fn root_runs(driver: &driver::Driver, root: &str) -> Vec<String> {
    driver
        .world
        .invocations()
        .await
        .into_iter()
        .filter(|view| {
            view.target
                .starts_with(lash_restate_test::TURN_DRIVER_SERVICE)
                && view.target.contains(root)
                && view.target.ends_with("/run")
        })
        .map(|view| view.id)
        .collect()
}

/// Purge the completed invocation `id` as the retention sweep does: its
/// journal, its state and its record are gone.
async fn purge(driver: &driver::Driver, id: &str) {
    let server = driver.world.double().expect("the double").server();
    for _ in 0..1000 {
        match server.purge(id) {
            Some(true) => return,
            Some(false) => tokio::time::sleep(Duration::from_millis(10)).await,
            None => panic!("the engine never held `{id}`"),
        }
    }
    panic!("`{id}` never completed, so it could not be purged");
}

async fn root_terminal(
    driver: &driver::Driver,
    session: &lash_core::SessionId,
    root: &str,
) -> Option<lash_core::store::RootTerminal> {
    driver
        .world
        .backend()
        .session_store_factory()
        .root_terminal(session, &lash_core::TurnId::from(root))
        .await
        .expect("read the root's terminal")
}

/// A root admits its input and starts its model call; an operator kills its
/// run, the drive consumes the release, and the engine purges the run
/// before any recovery pass sees it. The background pass alone settles the
/// root and its input: no host submission, and no second execution of the
/// root, so no second effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_started_root_is_settled_without_new_ingress() {
    let seed = 0x4281;
    let mut driver = driver::Driver::new(seed).await.expect("world");
    driver
        .step(
            seed,
            &plan::Step::Open {
                session: 0,
                lane: plan::Lane::Held,
                parent: None,
            },
        )
        .await
        .expect("open session");
    let session = driver.ledger.sessions[0].id.clone();
    let root = "held-missing";
    driver
        .send_held(&session, root, false)
        .await
        .expect("held root");
    assert!(
        driver.wait_reached(root).await,
        "the root admitted its input and started its model call"
    );
    let runs = root_runs(&driver, root).await;
    let [run] = runs.as_slice() else {
        panic!("one run of the started root: {runs:?}");
    };
    driver
        .world
        .kill_invocation(run)
        .await
        .expect("kill the root's run");
    // The drive consumes the killed run as released and stops on the root
    // it admits again; only then is the run's record purged.
    driver.world.quiesce().await;
    purge(&driver, run).await;
    assert!(
        root_runs(&driver, root).await.is_empty(),
        "the engine holds no run of the root on any lane"
    );
    assert!(
        root_terminal(&driver, &session, root).await.is_none(),
        "the root is still open before recovery"
    );

    for _ in 0..4 {
        driver.tick().await.expect("recovery tick");
    }

    let terminal = root_terminal(&driver, &session, root)
        .await
        .expect("the recovery pass ended the root");
    assert_eq!(
        terminal.cause,
        lash_core::store::RootTerminalCause::SubstrateLost { cancelled_by: None },
        "a started root whose history is gone ends substrate-lost"
    );
    let pending = driver
        .world
        .backend()
        .session_store_factory()
        .list_pending_turn_inputs(&session)
        .await
        .expect("pending inputs");
    assert!(
        pending.is_empty(),
        "the root's input is settled with it: {pending:?}"
    );
    assert!(
        root_runs(&driver, root).await.is_empty(),
        "recovery never ran the root again under a fresh journal"
    );
    let mut report = EpochReport {
        seed,
        ..EpochReport::default()
    };
    finish(&mut driver, &mut report).await;
    assert!(report.passed(), "{}", report.evidence());
    driver.world.finish().await;
}

/// A root whose record is open but which never started — its admission is
/// not recorded, so its input is still owed by its ingress obligation — has
/// no engine run either. Recovery never ends it: the obligation drives it,
/// and it commits exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_unstarted_root_runs_once_from_its_ingress() {
    let seed = 0x4282;
    let mut driver = driver::Driver::new(seed).await.expect("world");
    driver
        .step(
            seed,
            &plan::Step::Open {
                session: 0,
                lane: plan::Lane::Plain,
                parent: None,
            },
        )
        .await
        .expect("open session");
    let session = driver.ledger.sessions[0].id.clone();
    let root = "prestart";
    let hold = driver.world.hold_session_drive(&session).await;
    driver
        .world
        .core()
        .expect("core")
        .session(session.clone())
        .durable()
        .await
        .expect("durable session")
        .send(lash::TurnInput::text(
            crate::crash_matrix::invariants::input_text(root),
        ))
        .id(root)
        .await
        .expect("accept the input");
    driver.ledger.inputs.push(driver::SentInput {
        session: session.clone(),
        root: root.to_owned(),
        admission: driver::Admission::Known,
    });
    let input = lash_core::InputId::from(lash_core::PendingTurnInputDraft::keyed_input_id(
        &session, root,
    ));
    driver
        .world
        .backend()
        .session_store_factory()
        .bind_root_inputs(&session, &lash_core::TurnId::from(root), &[input])
        .await
        .expect("open the root's record before any execution admits it");

    for _ in 0..4 {
        driver.tick().await.expect("recovery tick");
    }
    assert!(
        root_runs(&driver, root).await.is_empty(),
        "the held drive never started the root"
    );
    assert!(
        root_terminal(&driver, &session, root).await.is_none(),
        "recovery never ends a root that did not start"
    );

    hold.release();
    let mut report = EpochReport {
        seed,
        ..EpochReport::default()
    };
    finish(&mut driver, &mut report).await;
    assert!(report.passed(), "{}", report.evidence());
    let terminal = root_terminal(&driver, &session, root)
        .await
        .expect("the ingress obligation drove the root to its terminal");
    assert!(
        matches!(
            terminal.cause,
            lash_core::store::RootTerminalCause::Committed { .. }
        ),
        "the unstarted root ran and committed: {terminal:?}"
    );
    assert_eq!(
        root_runs(&driver, root).await.len(),
        1,
        "the root ran exactly once"
    );
    driver.world.finish().await;
}
