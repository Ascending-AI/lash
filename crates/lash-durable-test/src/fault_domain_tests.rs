//! A cut at a label cuts the domain rows its commit carries: the fault store
//! forwards them inside the commit it labels.
//!
//! The database under the fault store is the harness's SQLite with a dummy
//! domain apply in front of it, which takes each committed transaction's
//! domain rows and records them only when the commit stands.

use super::*;
use crate::clock::settle;
use crate::life::Life;
use crate::script::Script;
use crate::testing::{FORMATS, actor, sqlite};
use lash_durable::domain::{DomainWrite, ParkEventWrite};
use lash_durable::{FormatSet, MailKind, NodeId};
use lash_sansio::sync::MutexExt as _;
use std::sync::Mutex;

const OWNED: CommitLabel = CommitLabel::new("t.owned");

/// The harness database with a dummy domain apply: the domain rows of every
/// standing commit, in commit order.
struct DummyDomainApply {
    inner: Arc<dyn DurableStore>,
    applied: Mutex<Vec<DomainWrite>>,
}

#[async_trait::async_trait]
impl DurableReads for DummyDomainApply {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.inner.snapshot(exec).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.inner.park_events(after, limit).await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

#[async_trait::async_trait]
impl DurableStore for DummyDomainApply {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.inner.register_node(spec).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.inner.heartbeat(node).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.inner.reap(reaper).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.release_node(node).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.inner.claim(node, limit).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(
        &self,
        mut tx: ActorTx,
        label: CommitLabel,
    ) -> Result<ActorCommit, DurableError> {
        let domain = tx.take_domain();
        let committed = self.inner.commit(tx, label).await?;
        self.applied.lock_recover().extend(domain);
        Ok(committed)
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}

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
    let database = Arc::new(DummyDomainApply {
        inner: sqlite(Arc::clone(&clock)).await,
        applied: Mutex::new(Vec::new()),
    });
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
