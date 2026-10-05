//! FIG-4281: an open run whose `LashTurn` run the engine no longer holds on
//! any lane has an autonomous recovery owner. The recovery pass ends a run
//! that started (it recorded its admission) `SubstrateLost`, with its input
//! disposition and scope close, and never runs it again under a fresh
//! journal; a run that never started is left to its input's ingress
//! obligation, which executes it once.
//!
//! Each law runs on the server double over the world's stores: SQLite, or
//! PostgreSQL when `LASH_POSTGRES_DATABASE_URL` names a database.

use super::*;

/// The `LashTurn` `run` invocations the engine holds for `run`, in any
/// status: the run's executions, never its `outcome` reads or its `close`.
async fn run_executions(
    driver: &driver::Driver,
    session: &lash_core::SessionId,
    run: &str,
) -> Vec<String> {
    driver
        .world
        .run_invocations(session, &lash_core::TurnId::fixture(run))
        .await
        .expect("recorded run invocations")
        .into_iter()
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

async fn run_terminal(
    driver: &driver::Driver,
    session: &lash_core::SessionId,
    run: &str,
) -> Option<lash_core::store::RunTerminal> {
    driver
        .world
        .backend()
        .session_store_factory()
        .run_terminal(session, &lash_core::TurnId::fixture(run))
        .await
        .expect("read the run's terminal")
}

/// A run admits its input and starts its model call; an operator kills its
/// run, the shift consumes the release, and the engine purges the run
/// before any recovery pass sees it. The background pass alone settles the
/// run and its input: no host submission, and no second execution of the
/// run, so no second effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_started_run_is_settled_without_new_ingress() {
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
    let run = "held-missing";
    driver
        .send_held(&session, run, false)
        .await
        .expect("held run");
    assert!(
        driver.wait_reached(run).await,
        "the run admitted its input and started its model call"
    );
    let runs = run_executions(&driver, &session, run).await;
    let [invocation] = runs.as_slice() else {
        panic!("one execution of the started run: {runs:?}");
    };
    driver
        .world
        .kill_invocation(invocation)
        .await
        .expect("kill the run's execution");
    // The shift consumes the killed run as released and stops on the run
    // it admits again; only then is the run's record purged.
    driver.world.quiesce().await;
    purge(&driver, invocation).await;
    assert!(
        run_executions(&driver, &session, run).await.is_empty(),
        "the engine holds no execution of the run on any lane"
    );
    assert!(
        run_terminal(&driver, &session, run).await.is_none(),
        "the run is still open before recovery"
    );

    for _ in 0..4 {
        driver.tick().await.expect("recovery tick");
    }

    let terminal = run_terminal(&driver, &session, run)
        .await
        .expect("the recovery pass ended the run");
    assert_eq!(
        terminal.cause,
        lash_core::store::RunTerminalCause::SubstrateLost { cancelled_by: None },
        "a started run whose history is gone ends substrate-lost"
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
        "the run's input is settled with it: {pending:?}"
    );
    assert!(
        run_executions(&driver, &session, run).await.is_empty(),
        "recovery never ran the run again under a fresh journal"
    );
    let mut report = EpochReport {
        seed,
        ..EpochReport::default()
    };
    finish(&mut driver, &mut report).await;
    assert!(report.passed(), "{}", report.evidence());
    driver.world.finish().await;
}

/// A run whose record is open but which never started — its admission is
/// not recorded, so its input is still owed by its ingress obligation — has
/// no engine run either. Recovery never ends it: the obligation executes it,
/// and it commits exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_unstarted_run_executes_once_from_its_ingress() {
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
    let run = "prestart";
    let hold = driver.world.hold_session_shift(&session).await;
    driver
        .world
        .core()
        .expect("core")
        .session(session.clone())
        .durable()
        .await
        .expect("durable session")
        .send(lash::TurnInput::text(
            crate::crash_matrix::invariants::input_text(run),
        ))
        .id(lash::TurnId::parse(run).expect("nonblank host identity"))
        .await
        .expect("accept the input");
    driver.record_host(
        crate::invariants::HostOp::Send,
        &session,
        vec![run.to_owned()],
        driver::Admission::Known,
    );
    driver.ledger.inputs.push(driver::SentInput {
        session: session.clone(),
        run: run.to_owned(),
        admission: driver::Admission::Known,
    });
    let input = lash_core::PendingTurnInputDraft::keyed_input_id(&session, run);
    driver
        .world
        .backend()
        .session_store_factory()
        .bind_run_inputs(&session, &lash_core::TurnId::fixture(run), &[input])
        .await
        .expect("open the run's record before any execution admits it");

    for _ in 0..4 {
        driver.tick().await.expect("recovery tick");
    }
    assert!(
        run_executions(&driver, &session, run).await.is_empty(),
        "the held shift never started the run"
    );
    assert!(
        run_terminal(&driver, &session, run).await.is_none(),
        "recovery never ends a run that did not start"
    );

    hold.release();
    let mut report = EpochReport {
        seed,
        ..EpochReport::default()
    };
    finish(&mut driver, &mut report).await;
    assert!(report.passed(), "{}", report.evidence());
    let terminal = run_terminal(&driver, &session, run)
        .await
        .expect("the ingress obligation drove the run to its terminal");
    assert!(
        matches!(
            terminal.cause,
            lash_core::store::RunTerminalCause::Committed { .. }
        ),
        "the unstarted run ran and committed: {terminal:?}"
    );
    assert_eq!(
        run_executions(&driver, &session, run).await.len(),
        1,
        "the run ran exactly once"
    );
    driver.world.finish().await;
}
