//! The round trips a model call's critical path pays to commit
//! `model.start` (FIG-5275), counted on an isolated database.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use lash_core_execution::store::{AdmittedTurnRows, RunAdmissionRecord};
use lash_durable::domain::{DomainWrite, ModelPin, TurnWrite, UnfinishedPhase};
use lash_durable::{
    ActorKey, CommitLabel, DurableReads as _, DurableStore as _, FormatSet, MailTx, NodeId,
    NodeSpec,
};
use lash_sansio::{SessionId, TurnId};

use crate::observed_sql::ROUND_TRIPS;
use crate::testing::IsolatedDatabase;

/// Count the round trips `work` makes from this task.
async fn round_trips<T>(work: impl std::future::Future<Output = T>) -> (T, u64) {
    ROUND_TRIPS
        .scope(std::cell::Cell::new(0), async {
            let value = work.await;
            (value, ROUND_TRIPS.with(std::cell::Cell::get))
        })
        .await
}

/// The most round trips a model call's critical path pays to commit
/// `model.start`, measured at FIG-5275 (14 before it): the open, which also
/// reads the turn's cancel and the clock, with its checkout; and the commit:
/// its checkout, the fenced `BEGIN`, the envelope's one statement, the
/// phase's `Advance` and `COMMIT`.
const MODEL_START_ROUND_TRIPS: u64 = 7;

/// The owner's path from a model call's first poll to its committed
/// `model.start` stays within its round-trip budget, so a read or an
/// envelope statement added to it cannot land unnoticed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_start_commit_stays_within_its_round_trip_budget() {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the model.start round-trip budget: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let store = storage.durable_store();
    let formats = FormatSet::new("round-trips");
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("round-trips"),
            decodes: vec![formats.clone()],
            ttl_millis: 15_000,
        })
        .await
        .expect("register");
    let session = SessionId::from("round-trips");
    let run = TurnId::from("round-trips-turn");
    let actor = ActorKey::session(session.as_str()).expect("actor key");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats);
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("create");
    let epoch = store.claim(&node, 1).await.expect("claim")[0].epoch;
    let mut admit = store
        .begin(&actor, epoch)
        .await
        .expect("open the admission");
    admit.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Batch {
                id: lash_sansio::BatchId::from("round-trips-batch"),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    store
        .commit(admit, CommitLabel::new("law.admit"))
        .await
        .expect("admit the turn");

    let ((), trips) = round_trips(async {
        let mut tx = store.begin(&actor, epoch).await.expect("open model.start");
        assert!(tx.turn_cancel().is_none(), "no cancel was requested");
        let deadline = tx.opened_at().after_millis(60_000);
        tx.write(DomainWrite::Turn(TurnWrite::Advance {
            session: session.clone(),
            run: run.clone(),
            phase: UnfinishedPhase::Model {
                pin: ModelPin {
                    call: 1,
                    attempt: 1,
                    request_ref: "request".to_owned(),
                    deadline,
                    stream_from: "stream".to_owned(),
                },
                checkpoint: "{}".to_owned(),
            },
            iteration: 0,
        }));
        store
            .commit(tx, CommitLabel::MODEL_START)
            .await
            .expect("commit model.start");
    })
    .await;
    eprintln!("model.start critical path: {trips} round trips");
    assert!(
        trips <= MODEL_START_ROUND_TRIPS,
        "model.start took {trips} round trips, over its budget of {MODEL_START_ROUND_TRIPS}"
    );
    let row = store
        .turn(&session)
        .await
        .expect("read the turn")
        .expect("the turn is open");
    assert!(
        matches!(row.phase, UnfinishedPhase::Model { .. }),
        "model.start committed its pin"
    );
}
