//! Session mail laws (ADR 0132 §3, §12; L3s, FIG-5196) over a real store
//! set.
//!
//! Every producer of session work writes its row and wakes the session actor
//! in one transaction, and the session actor admits work only through its
//! own claim: `drain_session_mail` binds what it admits inside the owner
//! commit, under the claimed epoch.
//!
//! - **One unfinished run:** two claimers racing the admission of the same
//!   mail admit one run, whatever order their commits land in.
//! - **Mail atomicity:** a producer transaction cut before its commit
//!   leaves neither its row nor the wake; committed, it leaves both.
//! - **WAKE1:** a producer refuses an absent or deleted session before it
//!   wakes anything: no actor is created for it.
#![expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lash_core::durable_port::domain::SESSION_ACTOR_FORMATS;
use lash_core::durable_port::{
    ActorKey, ActorTx, CommitLabel, DurableError, DurableStore, FormatSet, NodeId, NodeLease,
    NodeSpec,
};
use lash_core::runtime::durable::session_mail::{SessionMailDrain, drain_session_mail};
use lash_core::{
    ActorContext, AdmittedScope, Backend, DeliveryPolicy, PendingTurnInputDraft,
    PendingTurnInputReadStatus, SessionId, StoreSet, TurnInput, TurnInputIngress,
};

/// Arms and disarms a cut of the producer transactions on the store under
/// test: while armed, the transaction fails at the session actor's wake, the
/// last write before its commit.
pub trait WakeCut: Send + Sync {
    fn arm(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    fn disarm(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

fn actor_of(session: &SessionId) -> ActorKey {
    ActorKey::session(session.as_str()).expect("a session id names its actor")
}

async fn admit_root(stores: &Arc<dyn StoreSet>, session: &SessionId) {
    stores
        .session_store_factory()
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            session,
        ))
        .await
        .expect("create the session");
}

async fn enqueue_input(
    stores: &Arc<dyn StoreSet>,
    session: &SessionId,
    text: &str,
) -> Result<lash_core::PendingTurnInput, lash_core::StoreError> {
    stores
        .session_store_factory()
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            session,
            TurnInputIngress::NextTurn,
            TurnInput::text(text),
        ))
        .await
}

async fn register(durable: &Arc<dyn DurableStore>, node: &str) -> NodeLease {
    durable
        .register_node(&NodeSpec {
            node: NodeId::new(node),
            decodes: vec![FormatSet::new(SESSION_ACTOR_FORMATS)],
            ttl_millis: 60_000,
        })
        .await
        .expect("register a node")
}

/// One claimer of the session actor: its claim, its open owner transaction
/// and what its drain admitted.
struct Claimer {
    tx: ActorTx,
    drain: SessionMailDrain,
}

async fn claim_and_drain(backend: &Backend, lease: &NodeLease, actor: &ActorKey) -> Claimer {
    let durable = backend.durable();
    let claimed = durable
        .claim(lease, 8)
        .await
        .expect("claim ready actors")
        .into_iter()
        .find(|claimed| &claimed.actor == actor)
        .expect("the woken session actor is claimable");
    let mut tx = durable
        .begin(actor, claimed.epoch)
        .await
        .expect("open the owner transaction");
    let cx = ActorContext::new(
        backend.clone(),
        actor.clone(),
        claimed.epoch,
        AdmittedScope::runtime_operation("session-mail-law"),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(lash_core::durable_port::NoProbe),
    );
    let drain = drain_session_mail(&cx, &mut tx)
        .await
        .expect("drain the session mail");
    Claimer { tx, drain }
}

/// Which claimer commits when.
#[derive(Clone, Copy, Debug)]
enum Race {
    /// The first claimer loses the actor, then commits after the second.
    StaleCommitsLast,
    /// The first claimer loses the actor and commits before the second.
    StaleCommitsFirst,
    /// The first claimer commits while it still owns the actor; the second
    /// claims after it.
    OwnerCommitsBeforeTheSecondClaim,
}

/// One unfinished run: two claimers race the admission of a session's mail,
/// in every commit order, and exactly one run is admitted. The session's
/// first input is bound to it, the second stays open, and every admission
/// commit but one is refused for ownership or admits nothing.
pub async fn two_claimers_racing_admission_admit_one_run(stores: Arc<dyn StoreSet>, prefix: &str) {
    let backend = Backend::for_testing(Arc::clone(&stores));
    let durable = Arc::clone(backend.durable());
    for race in [
        Race::StaleCommitsLast,
        Race::StaleCommitsFirst,
        Race::OwnerCommitsBeforeTheSecondClaim,
    ] {
        let session = SessionId::fixture(format!("{prefix}-one-run-{race:?}"));
        let actor = actor_of(&session);
        admit_root(&stores, &session).await;
        let first = enqueue_input(&stores, &session, "first")
            .await
            .expect("enqueue the first input");
        let second = enqueue_input(&stores, &session, "second")
            .await
            .expect("enqueue the second input");
        let lease_a = register(&durable, &format!("{prefix}-{race:?}-a")).await;
        let lease_b = register(&durable, &format!("{prefix}-{race:?}-b")).await;

        let a = claim_and_drain(&backend, &lease_a, &actor).await;
        let run = a
            .drain
            .admit
            .as_ref()
            .map(|admitted| admitted.run.clone())
            .expect("the first claimer admits the head input");
        let mut a_tx = Some(a.tx);
        let mut landed = Vec::new();
        let b = match race {
            Race::OwnerCommitsBeforeTheSecondClaim => {
                if let Some(tx) = a_tx.take() {
                    landed.push(durable.commit(tx, CommitLabel::TURN_ADMIT).await);
                }
                durable
                    .release_node(&lease_a)
                    .await
                    .expect("the first node releases the actor");
                let b = claim_and_drain(&backend, &lease_b, &actor).await;
                assert_eq!(
                    b.drain.admit, None,
                    "{race:?}: a claimer after the admission admits nothing while its run is bound"
                );
                b
            }
            Race::StaleCommitsFirst | Race::StaleCommitsLast => {
                durable
                    .release_node(&lease_a)
                    .await
                    .expect("the first node releases the actor");
                let b = claim_and_drain(&backend, &lease_b, &actor).await;
                assert_eq!(
                    b.drain.admit.as_ref().map(|admitted| &admitted.run),
                    Some(&run),
                    "{race:?}: both claimers compose the same head run"
                );
                if matches!(race, Race::StaleCommitsFirst)
                    && let Some(tx) = a_tx.take()
                {
                    let stale = durable.commit(tx, CommitLabel::TURN_ADMIT).await;
                    assert!(
                        matches!(stale, Err(DurableError::OwnershipLost(_))),
                        "{race:?}: the stale claimer's admission is refused, got {stale:?}"
                    );
                }
                b
            }
        };
        let b_admits = b.drain.admit.is_some();
        let b_commit = durable.commit(b.tx, CommitLabel::TURN_ADMIT).await;
        if b_admits {
            landed.push(b_commit);
        } else {
            b_commit.expect("the second claimer's empty drain commits");
        }
        if let Some(tx) = a_tx.take() {
            let stale = durable.commit(tx, CommitLabel::TURN_ADMIT).await;
            assert!(
                matches!(stale, Err(DurableError::OwnershipLost(_))),
                "{race:?}: the stale claimer's admission is refused, got {stale:?}"
            );
        }
        assert_eq!(
            landed.iter().filter(|commit| commit.is_ok()).count(),
            1,
            "{race:?}: exactly one admission lands: {landed:?}"
        );

        let mailbox = durable
            .session_mailbox(&session)
            .await
            .expect("read the mailbox");
        assert_eq!(
            mailbox.bound_run,
            Some(run.clone()),
            "{race:?}: the session's one unfinished run is the admitted one"
        );
        assert_eq!(
            mailbox
                .inputs
                .iter()
                .map(|input| input.input.clone())
                .collect::<Vec<_>>(),
            vec![second.input_id.clone()],
            "{race:?}: the second input waits for the run to end"
        );
        let statuses = stores
            .session_store_factory()
            .list_pending_turn_inputs(&session)
            .await
            .expect("list the inputs")
            .into_iter()
            .map(|read| (read.input.input_id, read.status))
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                (
                    first.input_id.clone(),
                    PendingTurnInputReadStatus::Admitted { run: run.clone() }
                ),
                (second.input_id.clone(), PendingTurnInputReadStatus::Open),
            ],
            "{race:?}: the first input is bound to the one run"
        );
    }
}

/// The three producers of session work, each run once on its own session.
#[derive(Clone, Copy, Debug)]
enum Producer {
    Input,
    Batch,
    Close,
}

impl Producer {
    const ALL: [Self; 3] = [Self::Input, Self::Batch, Self::Close];

    async fn produce(self, stores: &Arc<dyn StoreSet>, session: &SessionId) -> bool {
        let factory = stores.session_store_factory();
        match self {
            Self::Input => enqueue_input(stores, session, "mail").await.is_ok(),
            Self::Batch => factory
                .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
                    session.clone(),
                    DeliveryPolicy::EarliestSafeBoundary,
                    lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                        reason: "session mail law".to_owned(),
                    },
                ))
                .await
                .is_ok(),
            Self::Close => factory
                .begin_session_close(session, 1)
                .await
                .is_ok_and(|intent| intent.is_some()),
        }
    }

    /// Whether the producer's row is stored.
    async fn row_stored(self, stores: &Arc<dyn StoreSet>, session: &SessionId) -> bool {
        let factory = stores.session_store_factory();
        match self {
            Self::Input => !factory
                .list_pending_turn_inputs(session)
                .await
                .expect("list the inputs")
                .is_empty(),
            Self::Batch => !factory
                .list_queued_work(session)
                .await
                .expect("list the queued work")
                .is_empty(),
            Self::Close => factory
                .session_close_intent(session)
                .await
                .expect("read the close intent")
                .is_some(),
        }
    }
}

/// Whether `session`'s actor was woken: it exists with mail it has not
/// acknowledged.
async fn woken(durable: &Arc<dyn DurableStore>, session: &SessionId) -> bool {
    durable
        .actor(&actor_of(session))
        .await
        .expect("read the session actor")
        .is_some_and(|actor| actor.has_mail)
}

/// Mail atomicity: each producer's transaction cut at its wake, before its
/// commit, leaves neither its row nor the wake; run again and committed, it
/// leaves both.
pub async fn a_producer_commits_its_row_and_its_wake_together(
    stores: Arc<dyn StoreSet>,
    prefix: &str,
    cut: &dyn WakeCut,
) {
    let durable = stores.durable_store();
    for producer in Producer::ALL {
        let session = SessionId::fixture(format!("{prefix}-atomic-{producer:?}"));
        admit_root(&stores, &session).await;
        cut.arm().await;
        let produced = producer.produce(&stores, &session).await;
        cut.disarm().await;
        assert!(!produced, "{producer:?}: the cut transaction is refused");
        assert!(
            !producer.row_stored(&stores, &session).await,
            "{producer:?}: a cut before the commit leaves no row"
        );
        assert!(
            durable
                .actor(&actor_of(&session))
                .await
                .expect("read the session actor")
                .is_none(),
            "{producer:?}: a cut before the commit leaves no wake"
        );
        assert!(
            producer.produce(&stores, &session).await,
            "{producer:?}: the uncut transaction commits"
        );
        assert!(
            producer.row_stored(&stores, &session).await,
            "{producer:?}: the committed transaction leaves its row"
        );
        assert!(
            woken(&durable, &session).await,
            "{producer:?}: the committed transaction leaves its wake"
        );
    }
}

/// WAKE1: a send against an absent or deleted session wakes no session
/// actor and creates nothing: the session stays absent or deleted, and no
/// actor exists for it. One send to a live session wakes it.
pub async fn a_producer_wakes_no_absent_or_deleted_session(
    stores: Arc<dyn StoreSet>,
    prefix: &str,
) {
    let durable = stores.durable_store();
    let factory = stores.session_store_factory();
    let absent = SessionId::fixture(format!("{prefix}-wake1-absent"));
    let deleted = SessionId::fixture(format!("{prefix}-wake1-deleted"));
    admit_root(&stores, &deleted).await;
    factory
        .delete_session(&deleted)
        .await
        .expect("delete the session");
    for session in [&absent, &deleted] {
        for producer in [Producer::Input, Producer::Batch] {
            // The store may keep or refuse the row; either way it wakes
            // nobody.
            let _ = producer.produce(&stores, session).await;
        }
        assert!(
            durable
                .actor(&actor_of(session))
                .await
                .expect("read the session actor")
                .is_none(),
            "{session} gains no actor"
        );
        assert!(
            matches!(
                factory
                    .lookup_session(session)
                    .await
                    .expect("look the session up"),
                lash_core::SessionLookup::Absent | lash_core::SessionLookup::Deleted
            ),
            "{session} is not created"
        );
    }
    let live = SessionId::fixture(format!("{prefix}-wake1-live"));
    admit_root(&stores, &live).await;
    assert!(
        durable
            .actor(&actor_of(&live))
            .await
            .expect("read the session actor")
            .is_none(),
        "creating a session wakes nothing"
    );
    assert!(Producer::Input.produce(&stores, &live).await);
    assert!(
        woken(&durable, &live).await,
        "one send to a live session wakes it"
    );
}
