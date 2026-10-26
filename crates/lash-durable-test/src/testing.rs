//! The harness's own test database: SQLite in memory on the virtual clock.

use crate::clock::SimClock;
use lash_durable::domain::{
    ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, RootOwner, RunFacts, ScopeKey,
    SessionCloseRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
    NodeId, NodeLease, NodeSpec, Reaped,
};
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::ProcessId;
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

/// The harness database can hold one node's registration on the wall
/// clock, as a slow connection would, while the virtual clock stands still.
pub(crate) struct TestDatabase {
    inner: Arc<dyn DurableStore>,
    slow_registration: Option<(NodeId, Duration)>,
}

impl TestDatabase {
    pub(crate) fn new(inner: Arc<dyn DurableStore>) -> Self {
        Self {
            inner,
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
    async fn model_call_pins(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.inner.model_call_pins(session, owner).await
    }

    async fn model_calls(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
        self.inner.model_calls(session, owner).await
    }

    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        self.inner.run_facts(owner).await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        self.inner.run_facts_of(owner, run).await
    }
    async fn run_facts_within(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
        max_rows: usize,
        limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<lash_durable::domain::RunFacts, DurableError> {
        self.inner
            .run_facts_within(owner, run, max_rows, limits)
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        self.inner.exec_root(exec).await
    }

    async fn cell_roots(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }
    async fn cell_roots_within(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
        max_rows: usize,
    ) -> Result<Vec<lash_durable::domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots_within(session, run, max_rows).await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash_durable::domain::checkpoint_graph::NodeId>,
        limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        self.inner.hydrate(owner, expected, limits).await
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
        session: &lash_kernel_value::identity::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
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
    fn environments(&self) -> std::sync::Arc<dyn lash_durable::environment::EnvironmentStore> {
        self.inner.environments()
    }

    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn lash_durable::domain::pages::PageStore>> {
        self.inner.pages()
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

    async fn acquire_idle_for_testing(
        &self,
        node: &lash_durable::NodeLease,
        actor: &lash_durable::ActorKey,
        epoch: lash_durable::Epoch,
    ) -> Result<lash_durable::Epoch, lash_durable::DurableError> {
        self.inner
            .acquire_idle_for_testing(node, actor, epoch)
            .await
    }

    async fn readiness(&self) -> Result<lash_durable::Readiness, DurableError> {
        self.inner.readiness().await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.inner.claim(node, limit).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
        self.inner.live_nodes().await
    }

    async fn actor_requirements(
        &self,
        after: Option<&lash_durable::ActorKey>,
        limit: usize,
    ) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
        self.inner.actor_requirements(after, limit).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn begin_retained(
        &self,
        actor: &ActorKey,
        epoch: lash_durable::Epoch,
        expected: lash_durable::StateRevision,
        max_mail_bytes: u64,
    ) -> Result<ActorTx, DurableError> {
        self.inner
            .begin_retained(actor, epoch, expected, max_mail_bytes)
            .await
    }

    async fn begin_process_retained(
        &self,
        actor: &lash_durable::ActorKey,
        epoch: lash_durable::Epoch,
        expected: lash_durable::StateRevision,
        max_mail_bytes: u64,
    ) -> Result<
        (lash_durable::ActorTx, lash_durable::domain::ProcessFrontier),
        lash_durable::DurableError,
    > {
        self.inner
            .begin_process_retained(actor, epoch, expected, max_mail_bytes)
            .await
    }

    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
        self.inner.open_process(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        self.inner.commit(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn claim_facts(
        &self,
        node: Option<&lash_durable::Owner>,
        actor: &ActorKey,
    ) -> Result<lash_durable::ClaimFacts, DurableError> {
        self.inner.claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }

    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
        self.inner.await_owner_commit(actor).await
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

/// FIG-7132: a waiting release's acknowledgement can arrive after the
/// reconciliation already claimed its timer. That late hint owes no empty
/// claim. Time and the commit/claim/hint order are explicit.
#[tokio::test]
async fn a_late_due_hint_after_a_completed_claim_opens_no_empty_claim() {
    use lash_durable::laws::node_wakes::{Claims, CountingStore};
    use lash_durable::runner::CommitEffects;
    use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig};
    use lash_durable::{DurableSettings, FormatSet, NodeCapabilities, Release};

    let clock = SimClock::new();
    let store = sqlite(Arc::clone(&clock)).await;
    let formats = FormatSet::new("law-formats");
    let actor = actor("late-timer-ack");
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), formats.clone());
    store
        .commit_mail(mail, CommitLabel::new("law.create"))
        .await
        .unwrap();
    let owner = store
        .register_node(&NodeSpec {
            node: NodeId::new("releasing"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: NodeCapabilities::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = store.claim(&owner, 1).await.unwrap();
    let mut tx = store.begin(&actor, claimed[0].epoch).await.unwrap();
    let due = store.now().await.unwrap().after_millis(40);
    tx.ack_seen().give_up(Release::Waiting {
        next_due: Some(due),
    });
    let effects = CommitEffects::of(&tx);
    let receipt = store
        .commit(tx, CommitLabel::new("law.timer"))
        .await
        .unwrap();
    eprintln!("timer-order: release committed; acknowledgement delivery held");

    clock.advance_to(40).await;
    let claims = Claims::default();
    struct Hold(tokio::sync::Notify);
    #[async_trait::async_trait]
    impl Activation for Hold {
        async fn activate(&self, _owned: Owned) -> Exit {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    let activation = Arc::new(Hold(tokio::sync::Notify::new()));
    let config = DurableSettings::default().validate().unwrap();
    let runner = Runner::new(
        Arc::new(CountingStore {
            inner: store,
            node: "claiming".to_owned(),
            claims: claims.clone(),
        }),
        clock.clone(),
        RunnerConfig::new(
            NodeId::new("claiming"),
            vec![FormatSet::new("law-formats")],
            &config,
        ),
        activation.clone(),
    );
    let hints = runner.hints();
    let mut task = tokio::spawn(runner.run(std::future::pending()));
    tokio::select! {
        () = activation.0.notified() => {}
        ended = &mut task => panic!("runner ended before activating the due timer: {ended:?}"),
    }
    eprintln!("timer-order: due timer claimed and activation waiting");
    hints.owner_committed(&effects, &receipt);
    eprintln!("timer-order: old acknowledgement delivered with due hint");
    clock.wait_for_sleepers(41, 1).await;
    clock.advance_to(41).await;
    // Both implementations return to reconciliation at 290. Waiting for
    // that live sleep joins the stale deadline's complete scheduling turn.
    clock.wait_for_sleepers(290, 1).await;
    task.abort();
    let _ = task.await;
    assert_eq!(claims.take(), vec![("claiming".to_owned(), 1)]);
}
