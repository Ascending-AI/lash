//! FIG-6399: opening a session actor reads its row, its mail, its
//! unfinished turn with the phase row and the cancel request that turn
//! accepted in one statement, so one SQLite snapshot answers all four. The
//! stored turn is decoded and refused exactly as the turn read refuses it,
//! after the epoch fence, and every open reads cancellation and close mail
//! afresh.

use lash_core_store::store::{AdmittedInputIds, AdmittedTurnRows, RunAdmissionRecord};
use lash_durable::domain::{
    DomainWrite, MailDomainWrite, SESSION_CLOSE_MAIL, TurnCancelRequest, TurnWrite,
};
use lash_durable::{
    ActorKey, CommitLabel, DurableError, DurableReads as _, DurableStore as _, Epoch, FormatSet,
    MailKind, MailTx, NodeId, NodeLease, NodeSpec, Release,
};
use lash_protocol_types::{SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId};

use crate::{SqliteDurableStore, SqliteStoreSet};

const SESSION: &str = "actor-open";
const RUN: &str = "actor-open-turn";

struct World {
    set: SqliteStoreSet,
    store: SqliteDurableStore,
    node: NodeLease,
    actor: ActorKey,
    epoch: Epoch,
}

/// A claimed session actor whose turn `RUN` is admitted and unfinished.
async fn admitted() -> World {
    let set = SqliteStoreSet::memory().await.unwrap();
    let store = set.durable_store();
    let formats = FormatSet::new("actor-open");
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("actor-open"),
            decodes: vec![formats.clone()],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let actor = ActorKey::session(SESSION).unwrap();
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats);
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .unwrap();
    let epoch = store.claim(&node, 1).await.unwrap()[0].epoch;
    let mut tx = store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: SessionId::from(SESSION),
        run: TurnId::from(RUN),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: AdmittedInputIds::one(lash_protocol_types::InputId::from("actor-open-batch")),
            },
            trace: None,
        },
        turn_deadline: None,
    }))
    .ack_seen();
    store
        .commit(tx, CommitLabel::new("law.admit"))
        .await
        .unwrap();
    World {
        set,
        store,
        node,
        actor,
        epoch,
    }
}

fn cancel(request_id: &str, mode: TurnCancelMode) -> TurnCancelRequest {
    TurnCancelRequest {
        session: SessionId::from(SESSION),
        run: TurnId::from(RUN),
        request_id: request_id.to_owned(),
        origin: Some("law".to_owned()),
        reason: None,
        undelivered: TurnCancelUndeliveredInputPolicy::Defer,
        mode,
    }
}

impl World {
    async fn ask(&self, request: TurnCancelRequest) {
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::RequestTurnCancel(request));
        self.store
            .commit_mail(tx, CommitLabel::new("law.cancel"))
            .await
            .unwrap();
    }

    async fn append(&self, kind: &str) {
        let mut tx = MailTx::new();
        tx.append(self.actor.clone(), MailKind::new(kind), "");
        self.store
            .commit_mail(tx, CommitLabel::new("law.mail"))
            .await
            .unwrap();
    }

    async fn execute(&self, sql: &'static str) {
        self.set
            .process_env_store()
            .conn
            .call(move |c| c.execute_batch(sql))
            .await
            .unwrap();
    }
}

fn lost(answer: Result<lash_durable::ActorTx, DurableError>) -> lash_durable::Fenced {
    match answer {
        Err(DurableError::OwnershipLost(fenced)) => fenced,
        other => panic!("expected a lost ownership, got {other:?}"),
    }
}

/// ACTOR-OPEN-WORK: the open is one statement, with no transaction of
/// its own: the statement's snapshot is the read's.
#[tokio::test]
async fn actor_begin_reads_one_snapshot_of_cancellation() {
    let world = admitted().await;
    let request = cancel("counted", TurnCancelMode::Immediate);
    world.ask(request.clone()).await;
    world.append("law.note").await;
    world.append("law.note").await;
    let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
        "actor.begin",
        "sqlite",
        world.store.begin(&world.actor, world.epoch),
    )
    .await;
    let tx = answer.unwrap();
    eprintln!(
        "ACTOR-BEGIN SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    assert_eq!(tx.turn_cancel(), Some(&request));
    assert_eq!(tx.mail().len(), 2, "each pending mail is read once");
    assert_eq!(
        (receipt.statements, receipt.shapes.len()),
        (1, 1),
        "the open is one statement"
    );
    assert_eq!(receipt.rows_returned, 2, "one row per pending mail");
    assert_eq!(
        receipt.sqlite_work.map(|work| work.fullscan_steps),
        Some(0),
        "the open scans no table"
    );
}

/// A cancel accepted between two opens reaches the second, whether
/// after-step or escalated to immediate; the first keeps what it read.
#[tokio::test]
async fn a_cancel_accepted_between_two_begins_reaches_the_second() {
    let world = admitted().await;
    let first = world.store.begin(&world.actor, world.epoch).await.unwrap();
    assert_eq!(first.turn_cancel(), None);
    let after_step = cancel("after-step", TurnCancelMode::AfterStep);
    world.ask(after_step.clone()).await;
    let second = world.store.begin(&world.actor, world.epoch).await.unwrap();
    assert_eq!(first.turn_cancel(), None, "an open is a snapshot");
    assert_eq!(second.turn_cancel(), Some(&after_step));
    let immediate = cancel("immediate", TurnCancelMode::Immediate);
    let mut escalated = after_step.clone();
    escalated.escalate_to(&immediate);
    world.ask(immediate).await;
    world.append("law.note").await;
    let third = world.store.begin(&world.actor, world.epoch).await.unwrap();
    assert_eq!(second.turn_cancel(), Some(&after_step));
    assert_eq!(third.turn_cancel(), Some(&escalated));
    assert_eq!(
        third.turn_cancel().map(|c| c.mode),
        Some(TurnCancelMode::Immediate)
    );
    assert_eq!(third.mail().len(), 1);
}

/// A close and a cancel that land after an open are not in it; its
/// commit acknowledges only the mail it read, so the next open reads
/// both.
#[tokio::test]
async fn a_close_racing_an_open_reaches_the_next_open() {
    let world = admitted().await;
    let mut open = world.store.begin(&world.actor, world.epoch).await.unwrap();
    assert!(open.mail().is_empty());
    let request = cancel("closing", TurnCancelMode::Immediate);
    world.append(SESSION_CLOSE_MAIL).await;
    world.ask(request.clone()).await;
    assert_eq!(open.turn_cancel(), None);
    open.ack_seen();
    let committed = world
        .store
        .commit(open, CommitLabel::new("law.step"))
        .await
        .unwrap();
    assert_eq!(committed.turn_cancel.as_ref(), Some(&request));
    let next = world.store.begin(&world.actor, world.epoch).await.unwrap();
    assert_eq!(next.turn_cancel(), Some(&request));
    assert!(
        next.mail()
            .iter()
            .any(|mail| mail.kind.as_str() == SESSION_CLOSE_MAIL),
        "the next open missed the close: {:?}",
        next.mail()
    );
}

/// An open at an epoch the actor no longer holds is refused with the
/// current epoch, before the turn is read; the new owner reads the
/// cancel.
#[tokio::test]
async fn an_ownership_change_fences_the_open_before_the_turn() {
    let world = admitted().await;
    let mut tx = world.store.begin(&world.actor, world.epoch).await.unwrap();
    tx.ack_seen().give_up(Release::Idle);
    world
        .store
        .commit(tx, CommitLabel::new("law.release"))
        .await
        .unwrap();
    let fenced = lost(world.store.begin(&world.actor, world.epoch).await);
    let released = fenced.current.expect("the released actor stands");
    assert!(released > world.epoch, "a release bumps the epoch");
    let fenced = lost(world.store.begin(&world.actor, released).await);
    assert_eq!(
        (fenced.held, fenced.current),
        (released, Some(released)),
        "an idle actor is not owned at its own epoch"
    );
    let request = cancel("handover", TurnCancelMode::AfterStep);
    world.ask(request.clone()).await;
    let claimed = world.store.claim(&world.node, 1).await.unwrap();
    assert_eq!(claimed.len(), 1, "the cancel readied the session");
    let next = claimed[0].epoch;
    assert!(next > released);
    let fenced = lost(world.store.begin(&world.actor, world.epoch).await);
    assert_eq!((fenced.held, fenced.current), (world.epoch, Some(next)));
    let tx = world.store.begin(&world.actor, next).await.unwrap();
    assert_eq!(tx.turn_cancel(), Some(&request));
    let absent = ActorKey::session("actor-open-absent").unwrap();
    let fenced = lost(world.store.begin(&absent, next).await);
    assert_eq!(fenced.current, None);
}

/// A corrupt stored turn refuses the open with the turn read's own
/// typed cause, in its decode order; a stale epoch is refused first.
#[tokio::test]
async fn a_corrupt_stored_turn_refuses_the_open_with_the_turn_reads_cause() {
    let world = admitted().await;
    world
        .ask(cancel("corrupt", TurnCancelMode::Immediate))
        .await;
    let session = SessionId::from(SESSION);
    world
        .execute(
            "UPDATE session_runs SET admission_json = '{\"not\":\"an admission\"}';
             UPDATE turn_phases SET iteration = -1;",
        )
        .await;
    let read = world.store.turn(&session).await.unwrap_err();
    let opened = world
        .store
        .begin(&world.actor, world.epoch)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &read,
            DurableError::Store(failure)
                if failure.kind == lash_durable::StoreFailureKind::Corrupt
        ),
        "the admission decodes first: {read:?}"
    );
    assert_eq!(format!("{opened:?}"), format!("{read:?}"));
    let fenced = lost(
        world
            .store
            .begin(&world.actor, Epoch(world.epoch.0 + 1))
            .await,
    );
    assert_eq!(fenced.current, Some(world.epoch));

    let corrupt_iteration = admitted().await;
    corrupt_iteration
        .execute("UPDATE turn_phases SET iteration = -1;")
        .await;
    let read = corrupt_iteration.store.turn(&session).await.unwrap_err();
    let opened = corrupt_iteration
        .store
        .begin(&corrupt_iteration.actor, corrupt_iteration.epoch)
        .await
        .unwrap_err();
    assert_eq!(format!("{opened:?}"), format!("{read:?}"));
}
