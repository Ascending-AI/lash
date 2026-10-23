//! The K1, first-winner and T1 laws of waits
//! (`lash_core_execution::runtime::actor::wait_laws`; L5, FIG-5173) over
//! PostgreSQL, each on its own isolated database.

// This file is test code; ambient env access is sanctioned here (the
// workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::runtime::actor::wait_laws;
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;

use crate::PostgresStoreSet;
use crate::testing::IsolatedDatabase;

macro_rules! law {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(database_url) = crate::postgres_test_support::database_url() else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
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

            })
            .expect("assemble the law backend");
            wait_laws::$name(&backend)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

law!(
    k1_a_key_that_is_not_an_issued_wait_id_is_refused_and_writes_nothing,
    the_first_resolution_wins,
    a_waiting_actor_past_its_deadline_times_out_within_the_claim_poll,
    a_parked_call_is_listed_from_its_wait_row_alone,
);

/// A process releases its pending wait durably, then resumes only when the
/// runner claims it again. The host reaches the same resolve path as a webhook.
struct ParkOnWait {
    pinned: tokio::sync::mpsc::UnboundedSender<lash_durable::domain::WaitId>,
    resumed: tokio::sync::mpsc::UnboundedSender<lash_durable::runner::Wake>,
    wait: lash_durable::domain::WaitId,
}

#[async_trait::async_trait]
impl lash_durable::runner::Activation for ParkOnWait {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        use lash_durable::domain::{DomainWrite, ScopeKey, WaitPurpose, WaitWrite};
        use lash_durable::{CommitLabel, Release};
        let mut tx = owned.begin().await.expect("open the process");
        if owned
            .store()
            .wait(&self.wait)
            .await
            .expect("read wait")
            .is_none()
        {
            tx.write(DomainWrite::Wait(WaitWrite::Pin {
                id: self.wait,
                scope: ScopeKey::Process(lash_vocabulary::identity::ProcessId::fixture(
                    "resolved-wait",
                )),
                purpose: WaitPurpose::EngineKey {
                    name: "law".to_owned(),
                    deadline: None,
                },
            }));
            tx.give_up(Release::Waiting { next_due: None });
            owned
                .commit(tx, CommitLabel::WAIT_MINT)
                .await
                .expect("park the process");
            self.pinned.send(self.wait).expect("report parked wait");
        } else {
            let row = owned
                .store()
                .wait(&self.wait)
                .await
                .expect("read resolved wait")
                .expect("the wait remains");
            assert_eq!(
                row.lifecycle.state(),
                lash_durable::domain::WaitState::Resolved
            );
            self.resumed
                .send(owned.claimed_on())
                .expect("report resumption");
            tx.ack_seen().give_up(Release::Idle);
            owned
                .commit(tx, CommitLabel::new("law.release"))
                .await
                .expect("release");
        }
        lash_durable::runner::Exit::Released
    }
}

async fn resolved_wait_wake(hint: bool) {
    use lash_durable::runner::{Runner, RunnerConfig, Wake};
    use lash_durable::{
        ActorKey, ActorState, CommitLabel, DurableSettings, FormatSet, MailTx, NodeId, NodeWakes,
        WakeBatch,
    };
    use std::time::Duration;
    let database_url =
        crate::postgres_test_support::database_url().expect("this law requires PostgreSQL");
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open store");
    let mut settings = DurableSettings::default();
    settings.lease.claim_poll = Duration::from_secs(10);
    settings.lease.claim_backoff = Duration::from_secs(10);
    settings.lease.reap_every = Duration::from_secs(10);
    settings.lease.self_stop_after = Duration::from_secs(20);
    settings.lease.ttl = Duration::from_secs(30);
    let backend = Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: Arc::new(PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        )),
        settings,
        engines: Vec::new(),
    })
    .expect("assemble backend");
    let store = backend.durable();
    let actor = ActorKey::process("resolved-wait").expect("process key");
    let formats = FormatSet::new("wake-law");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats.clone());
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("create process actor");
    let (pinned, mut parked) = tokio::sync::mpsc::unbounded_channel();
    let (resumed, mut arrived) = tokio::sync::mpsc::unbounded_channel();
    let clock = lash_durable_test::SimClock::new();
    let node = NodeId::new("wake-law");
    let wakes = Arc::new(storage.node_wakes());
    let runner = Runner::new(
        Arc::clone(store),
        clock.clone(),
        RunnerConfig::new(node.clone(), vec![formats], backend.config()),
        Arc::new(ParkOnWait {
            pinned,
            resumed,
            wait: lash_durable::domain::WaitId([43; 16]),
        }),
    )
    .with_node_wakes(wakes.clone());
    let task = tokio::spawn(runner.run(std::future::pending()));
    let key = tokio::time::timeout(Duration::from_secs(5), parked.recv())
        .await
        .expect("process parks")
        .expect("wait pinned");
    assert_eq!(
        store
            .actor(&actor)
            .await
            .expect("read actor")
            .expect("actor")
            .state,
        ActorState::Waiting
    );
    // The initial scan has returned and the runner has armed its next scan.
    // Its only ten-second timer is the claim/reap loop; startup, renewal and
    // operation watchdogs have different deadlines. Virtual time stays held.
    tokio::time::timeout(Duration::from_secs(10), clock.wait_for_sleep(10_000))
        .await
        .expect("the initial scan completed and the next poll is armed");
    let answer = lash_core_execution::runtime::actor::waits::resolve_host(
        &backend,
        &key.to_hex(),
        lash_core_execution::Resolution::Ok(serde_json::json!("done")),
    )
    .await
    .expect("resolution commits even without a hint");
    assert_eq!(answer, lash_durable::domain::ResolveAnswer::Resolved);
    assert_eq!(
        store
            .actor(&actor)
            .await
            .expect("read resolved actor")
            .expect("actor")
            .state,
        ActorState::Ready,
        "the committed resolution readied the released actor"
    );
    assert!(
        arrived.try_recv().is_err(),
        "no resume before a wake or poll"
    );
    assert_eq!(clock.logical_ms(), 0, "no timer fired during setup");
    if hint {
        // The producer's post-commit delivery goes through the real PG
        // transport. The runner has no attachment to the producer's local
        // hints, and no poll can fire while its clock remains held.
        wakes
            .committed(&WakeBatch {
                ready: [node].into(),
                ..WakeBatch::default()
            })
            .await
            .expect("deliver the committed resolution's PG notification");
    } else {
        // Drop that delivery. Only an explicitly released poll can discover
        // the durable Ready row; the same listener remains attached.
        clock.advance_to(10_000).await;
    }
    let result = tokio::time::timeout(Duration::from_secs(10), arrived.recv()).await;
    task.abort();
    let _ = task.await;
    let cause = result
        .expect("the runner resumes after the causal witness")
        .expect("activation resumed");
    assert_eq!(cause, if hint { Wake::Hint } else { Wake::Poll });
    assert_eq!(clock.logical_ms(), if hint { 0 } else { 10_000 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_resolved_wait_wakes_a_parked_process_by_its_notification() {
    resolved_wait_wake(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_resolved_wait_with_a_dropped_hint_wakes_by_poll() {
    resolved_wait_wake(false).await;
}
