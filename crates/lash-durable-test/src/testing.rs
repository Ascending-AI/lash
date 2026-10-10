//! The harness's own test database: SQLite in memory on the virtual clock.

use crate::clock::SimClock;
use lash_durable::domain::{
    DomainWrite, ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow,
    ScopeKey, SessionCloseRow, SnapshotRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
    NodeId, NodeLease, NodeSpec, Reaped,
};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ProcessId, SessionId};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) const FORMATS: &str = "t-v1";

pub(crate) async fn sqlite(clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
        .await
        .expect("an in-memory store set opens");
    Arc::new(stores.durable_store())
}

/// A fresh SQLite file store set on `clock`, under a directory that `dirs`
/// keeps until the test ends.
pub(crate) async fn sqlite_file(
    clock: Arc<SimClock>,
    dirs: &Mutex<Vec<tempfile::TempDir>>,
) -> Arc<dyn DurableStore> {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let stores = lash_sqlite_store::SqliteStoreSet::open_with_clock(
        dir.path().join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
        clock,
    )
    .await
    .expect("a file store set opens");
    dirs.lock_recover().push(dir);
    Arc::new(stores.durable_store())
}

pub(crate) fn actor(id: &str) -> ActorKey {
    ActorKey::session(id).expect("test actor ids are valid")
}

/// The harness database with two seams in front of it: it records the
/// domain rows of every standing commit, in commit order, as a domain apply
/// would, and it can hold one node's registration on the wall clock, as a
/// slow connection would, while the virtual clock stands still.
pub(crate) struct TestDatabase {
    inner: Arc<dyn DurableStore>,
    pub(crate) applied: Mutex<Vec<DomainWrite>>,
    slow_registration: Option<(NodeId, Duration)>,
}

impl TestDatabase {
    pub(crate) fn new(inner: Arc<dyn DurableStore>) -> Self {
        Self {
            inner,
            applied: Mutex::new(Vec::new()),
            slow_registration: None,
        }
    }

    /// Hold `node`'s registrations `delay` of wall time before they enter.
    pub(crate) fn slow_registration(mut self, node: &str, delay: Duration) -> Self {
        self.slow_registration = Some((NodeId::new(node), delay));
        self
    }
}

#[async_trait::async_trait]
impl DurableReads for TestDatabase {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
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

    async fn cell_snapshots(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<SnapshotRow>, DurableError> {
        self.inner.cell_snapshots(session, run).await
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
impl DurableStore for TestDatabase {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        if let Some((node, delay)) = &self.slow_registration
            && *node == spec.node
        {
            tokio::time::sleep(*delay).await;
        }
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

    async fn actors_in(
        &self,
        formats: &lash_durable::FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.actors_in(formats, after, limit).await
    }
}
