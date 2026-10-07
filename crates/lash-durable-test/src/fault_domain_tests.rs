//! A cut at a label cuts the domain rows its commit carries: the fault store
//! forwards them inside the commit it labels.
//!
//! The database under the fault store is the harness's [`TestDatabase`],
//! which takes each committed transaction's domain rows and records them
//! only when the commit stands.

use super::*;
use crate::clock::settle;
use crate::life::Life;
use crate::script::Script;
use crate::testing::{FORMATS, TestDatabase, actor, sqlite};
use lash_durable::domain::{DomainWrite, ParkEventWrite};
use lash_durable::{FormatSet, MailKind, NodeId};

const OWNED: CommitLabel = CommitLabel::new("t.owned");

fn park(reason: &str) -> DomainWrite {
    DomainWrite::ParkEvent(ParkEventWrite::Park {
        reason_json: reason.to_owned(),
    })
}

/// Fail-before and abort cut the domain rows with their commit; an
/// uncut commit and commit-then-abort carry them into the store. (Node `b`
/// commits under `a`'s epoch: the fence reads the epoch, not the node.)
#[tokio::test]
async fn a_cut_commit_cuts_the_domain_rows_it_carries() {
    let clock = SimClock::new();
    let database = Arc::new(TestDatabase::new(sqlite(Arc::clone(&clock)).await));
    let script = Script::new();
    let activity = Arc::<Activity>::default();
    let node = |name: &str| {
        let life = NodeLife::new();
        let store = Arc::new(FaultStore::new(
            Arc::clone(&database) as Arc<dyn DurableStore>,
            Arc::from(name),
            Arc::clone(&script.shared),
            Arc::clone(&life),
            Arc::clone(&clock),
            Arc::clone(&activity),
        ));
        (store, life)
    };
    let (a, a_life) = node("a");
    let (b, b_life) = node("b");
    let lease = a
        .register_node(&NodeSpec {
            node: NodeId::new("a"),
            decodes: vec![FormatSet::new(FORMATS)],
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let mut create = MailTx::new();
    create
        .create_actor(actor("one"), FormatSet::new(FORMATS))
        .append(actor("one"), MailKind::new("t"), "body");
    a.commit_mail(create, CommitLabel::new("t.create"))
        .await
        .unwrap();
    let epoch = a.claim(&lease, 1).await.unwrap()[0].epoch;
    script
        .cut_on("a", OWNED, 1, Fault::FailBefore)
        .cut_on("a", OWNED, 3, Fault::CommitThenAbort)
        .cut_on("b", OWNED, 1, Fault::Abort);

    let owned = |store: &Arc<FaultStore>, kind: &str| {
        let a = Arc::clone(store);
        let kind = kind.to_owned();
        async move {
            let mut tx = a.begin(&actor("one"), epoch).await?;
            tx.write(park(&kind));
            a.commit(tx, OWNED).await
        }
    };
    assert!(matches!(
        owned(&a, "failed-before").await,
        Err(DurableError::Store(_))
    ));
    owned(&a, "uncut").await.expect("the uncut commit stands");
    let committed_then_died = tokio::spawn(owned(&a, "committed-then-died"));
    script.settled(script.trace().len() + 1).await;
    settle().await;
    assert_eq!(a_life.get(), Life::Dead);
    committed_then_died.abort();
    let aborted = tokio::spawn(owned(&b, "aborted"));
    script.settled(script.trace().len() + 1).await;
    settle().await;
    assert_eq!(b_life.get(), Life::Dead);
    aborted.abort();

    assert_eq!(
        *database.applied.lock_recover(),
        vec![park("uncut"), park("committed-then-died")],
        "only commits that entered the store carried their domain rows"
    );
}
