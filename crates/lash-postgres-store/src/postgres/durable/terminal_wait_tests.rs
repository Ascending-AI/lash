//! A process-terminal wait pinned while its target's terminal transaction
//! is in flight (FIG-6329), over PostgreSQL: a test transaction holds an
//! engine row the terminal transaction writes, so the law reaches into this
//! module's tables.

// This file is test code; ambient env access is sanctioned here (the
// workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::runtime::actor::wait_laws;
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::projection::NoProjectionProviders;

use crate::PostgresStoreSet;
use crate::testing::IsolatedDatabase;

/// How many sessions of the connected database wait on a lock.
async fn lock_waiters(connection: &mut sqlx::PgConnection) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(connection)
    .await
    .expect("read the lock waiters")
}

/// Poll `observer` until `enough` sessions wait on a lock, or `done`
/// holds, at most ten seconds.
async fn until_waiting(
    what: &str,
    observer: &mut sqlx::PgConnection,
    enough: i64,
    done: impl Fn() -> bool,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() && lock_waiters(observer).await < enough {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A process-terminal wait pinned while its target's terminal transaction
/// is in flight is resolved (FIG-6329). The terminal transaction resolves
/// the waits its statement sees, and the awaiter's check after its pin
/// reads the target's end; under READ COMMITTED a pin that commits after
/// the terminal's resolution ran, checked before the terminal commits, is
/// seen by neither, so the check waits out the target's owner commit
/// before it reads.
///
/// The target's terminal transaction is held, past its resolution, at its
/// own scope's revocation: a test transaction locks the wait that scope
/// holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_wait_pinned_during_the_targets_terminal_commit_is_resolved() {
    use lash_core_execution::runtime::actor::process::record_terminal;
    use lash_core_execution::runtime::actor::waits;
    use lash_core_execution::runtime::actor::waits::WaitPurpose;
    use lash_durable::domain::{ScopeKey, WaitState};
    use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};
    use lash_execution_contracts::waits::WaitSpec;
    use sqlx::Connection as _;

    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let backend = Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: Arc::new(PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        )),
        settings: wait_laws::settings(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
    })
    .expect("assemble the law backend");
    let store = backend.durable();

    // The target: a registered process. The awaiter: an actor of its own.
    lash_execution_contracts::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let target = backend
        .process_registry()
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: "terminal-law".to_owned(),
                payload: serde_json::json!({}),
                launch: lash_execution_contracts::process::EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::LifetimeDecision::Detached,
        ))
        .await
        .expect("register the target")
        .id;
    let target_actor = ActorKey::process(target.as_str()).expect("the target's key");
    let awaiter = lash_protocol_types::ProcessId::fixture("terminal-law-awaiter");
    let awaiter_actor = ActorKey::process(awaiter.as_str()).expect("the awaiter's key");
    let awaiter_formats = FormatSet::new("terminal-law");
    let mut create = MailTx::new();
    create.create_actor(awaiter_actor.clone(), awaiter_formats.clone());
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("create the awaiter");
    let target_formats = store
        .actor(&target_actor)
        .await
        .expect("read the target")
        .expect("the target's actor")
        .formats;
    let lease = store
        .register_node(&NodeSpec {
            node: NodeId::new("terminal-law"),
            decodes: vec![target_formats, awaiter_formats],
            serves: Vec::new(),
            ttl_millis: 60_000,
        })
        .await
        .expect("register the node");
    let claimed = store.claim(&lease, 16).await.expect("claim both");
    let epoch = |actor: &ActorKey| {
        claimed
            .iter()
            .find(|claimed| &claimed.actor == actor)
            .unwrap_or_else(|| panic!("{actor} was not claimed: {claimed:?}"))
            .epoch
    };
    let (target_epoch, awaiter_epoch) = (epoch(&target_actor), epoch(&awaiter_actor));

    // The target's own scope holds a wait, which the test then locks.
    let mut tx = store
        .begin(&target_actor, target_epoch)
        .await
        .expect("open the target");
    let (held, _) = waits::pin(
        &mut tx,
        WaitSpec {
            scope: ScopeKey::Process(target.clone()),
            purpose: WaitPurpose::EngineKey {
                name: "held".to_owned(),
                deadline: None,
            },
        },
    );
    store
        .commit(tx, CommitLabel::WAIT_MINT)
        .await
        .expect("pin the target's own wait");
    let mut observer = sqlx::PgConnection::connect(database.url())
        .await
        .expect("open the observer");
    let mut holder = sqlx::PgConnection::connect(database.url())
        .await
        .expect("open the holder");
    let mut hold = holder.begin().await.expect("begin the hold");
    sqlx::query("SELECT wait_id FROM lash_waits WHERE wait_id = $1 FOR UPDATE")
        .bind(held.id().to_hex())
        .fetch_one(&mut *hold)
        .await
        .expect("lock the target's own wait");

    // The terminal transaction resolves the waits it sees (none yet), then
    // waits at its scope's revocation.
    let outcome = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_protocol_types::tool_output::ToolCallOutput::success(serde_json::json!("ended")),
    );
    let terminal = tokio::spawn({
        let store = Arc::clone(store);
        let target = target.clone();
        let target_actor = target_actor.clone();
        let outcome = outcome.clone();
        async move {
            let mut tx = store.begin(&target_actor, target_epoch).await?;
            record_terminal(&mut tx, &target, &outcome)?;
            tx.ack_seen();
            store.commit(tx, CommitLabel::new("law.terminal")).await
        }
    });
    until_waiting(
        "the terminal transaction waits at its revocation",
        &mut observer,
        1,
        || false,
    )
    .await;

    // The awaiter pins its wait on the target and commits it, then checks
    // whether the target ended.
    let mut tx = store
        .begin(&awaiter_actor, awaiter_epoch)
        .await
        .expect("open the awaiter");
    let (wait, _) = waits::pin(
        &mut tx,
        WaitSpec {
            scope: ScopeKey::Process(awaiter.clone()),
            purpose: WaitPurpose::ProcessTerminal {
                process: target.clone(),
                deadline: None,
            },
        },
    );
    store
        .commit(tx, CommitLabel::WAIT_MINT)
        .await
        .expect("pin the awaiter's wait");
    let check = tokio::spawn({
        let backend = backend.clone();
        async move { waits::resolve_ended_terminal(&backend, &wait).await }
    });
    until_waiting(
        "the awaiter's check ends or waits on the terminal transaction",
        &mut observer,
        2,
        || check.is_finished(),
    )
    .await;

    hold.rollback().await.expect("release the hold");
    terminal
        .await
        .expect("the terminal task")
        .expect("the terminal commits");
    check
        .await
        .expect("the check task")
        .expect("the check answers");
    let row = store
        .wait(&wait.id())
        .await
        .expect("read the awaiter's wait")
        .expect("the awaiter's wait");
    assert_eq!(
        row.lifecycle.state(),
        WaitState::Resolved,
        "a wait pinned during its target's terminal transaction was resolved by neither \
         side: its awaiter never wakes"
    );
}
