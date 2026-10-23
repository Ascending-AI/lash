//! Observes the served node's transactions over the same store as its catalog.
use super::RuntimePerfStoreMetrics;
use lash_durable::{domain::*, *};
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct RuntimePerfDurableStore {
    pub(crate) inner: Arc<dyn DurableStore>,
    pub(crate) metrics: Arc<RuntimePerfStoreMetrics>,
    pub(crate) measure_commit_bytes: bool,
}

#[async_trait::async_trait]
impl DurableStore for RuntimePerfDurableStore {
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
        // Retain the logical head-commit metric name across the substrate change.
        let mut head_commit = false;
        for write in tx.domain() {
            if let DomainWrite::SessionCommit(write) = write {
                head_commit = true;
                if self.measure_commit_bytes
                    && let Ok(commit) = lash_core::store::decode_session_commit(&write.commit_json)
                {
                    self.metrics.record_commit(&commit);
                }
            }
        }
        let observation = head_commit.then(|| self.metrics.observe_call("commit_runtime_state"));
        let started = Instant::now();
        let answer = if std::env::var_os("LASH_PERF_COMMIT_TRACE").is_some() {
            let bytes: u64 = tx.graph().values().map(|node| node.len()).sum();
            let alloc_before = crate::GLOBAL_ALLOCATOR.stats();
            eprintln!("MEASURE_COMMIT_BEGIN {}", label.as_str());
            let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
                label.as_str(),
                if std::env::var("LASH_PERF_COMMIT_TRACE").as_deref() == Ok("postgres") {
                    "postgres"
                } else {
                    "sqlite"
                },
                self.inner.commit(tx, label),
            )
            .await;
            let alloc = crate::GLOBAL_ALLOCATOR.stats() - alloc_before;
            eprintln!(
                "MEASURE_OWNER {}",
                serde_json::json!({
                    "label": label.as_str(), "staged_bytes": bytes,
                    "host_allocations": alloc.allocations, "host_allocated_bytes": alloc.bytes_allocated,
                    "sql": receipt, "success": answer.is_ok(),
                })
            );
            eprintln!("MEASURE_COMMIT_END {}", label.as_str());
            answer
        } else {
            self.inner.commit(tx, label).await
        };
        self.metrics
            .record_timing("store_transaction", started.elapsed());
        drop(observation);
        answer
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let observation = self.metrics.observe_call("durable_commit_mail");
        let started = Instant::now();
        let answer = self.inner.commit_mail(tx, label).await;
        self.metrics
            .record_timing("queue_enqueue", started.elapsed());
        drop(observation);
        answer
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

#[async_trait::async_trait]
impl DurableReads for RuntimePerfDurableStore {
    async fn model_call_pins(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.inner.model_call_pins(session, owner).await
    }

    async fn model_calls(
        &self,
        session: &lash_vocabulary::identity::SessionId,
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
        run: &TurnId,
    ) -> Result<Vec<TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
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
        session: &lash_vocabulary::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }
    async fn cell_roots_within(
        &self,
        session: &lash_vocabulary::identity::SessionId,
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

    async fn session_mailbox(&self, session: &SessionId) -> Result<SessionMailbox, DurableError> {
        let observation = self.metrics.observe_call("durable_session_mailbox");
        let started = Instant::now();
        let answer = self.inner.session_mailbox(session).await;
        self.metrics
            .record_timing("admission_scan", started.elapsed());
        drop(observation);
        answer
    }

    async fn prompt_snapshot(
        &self,
        call: &PromptCallKey,
    ) -> Result<Option<PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(&self, hashes: &[String]) -> Result<Vec<PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}
