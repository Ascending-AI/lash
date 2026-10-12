//! Observes the served node's transactions over the same store as its catalog.
use super::observation::Observation;
use lash_core_execution::facade_support::sql;
use lash_durable::{domain::*, *};
use lash_sansio::{ProcessId, SessionId, TurnId};
use std::sync::Arc;

pub(crate) struct ObservedStore {
    pub(crate) inner: Arc<dyn DurableStore>,
    pub(super) observation: Arc<Observation>,
}

#[async_trait::async_trait]
impl DurableStore for ObservedStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.observe(self.inner.now()).await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.observe(self.inner.register_node(spec)).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.observe(self.inner.heartbeat(node)).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.observe(self.inner.reap(reaper)).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.observe(self.inner.release_node(node)).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.observe(self.inner.claim(node, limit)).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.observe(self.inner.mark_draining(node)).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError> {
        self.observe(self.inner.live_decodes()).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.observe(self.inner.owned(node)).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.observe(self.inner.begin(actor, epoch)).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        self.observation
            .staged(tx.graph().values().map(|node| node.len()).sum());
        self.observation.before_commit(label);
        self.observe(self.inner.commit(tx, label)).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.observe(self.inner.commit_mail(tx, label)).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.observe(self.inner.actor(actor)).await
    }

    async fn actors_in(
        &self,
        formats: &lash_durable::FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.observe(self.inner.actors_in(formats, after, limit))
            .await
    }
}

#[async_trait::async_trait]
impl DurableReads for ObservedStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.observe(self.inner.turn(session)).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnNamespace>, DurableError> {
        self.observe(self.inner.turn_namespaces(session, run)).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.observe(self.inner.turn_end(session, run)).await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        self.observe(self.inner.run_facts(owner)).await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        self.observe(self.inner.run_facts_of(owner, run)).await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        self.observe(self.inner.exec_root(exec)).await
    }

    async fn cell_roots(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.observe(self.inner.cell_roots(session, run)).await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash_durable::domain::checkpoint_graph::NodeId>,
        limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        let answer = self
            .observe(self.inner.hydrate(owner, expected, limits))
            .await;
        if let Ok(graph) = &answer {
            self.observation.hydrated(
                graph.nodes.len() as u64,
                graph
                    .nodes
                    .values()
                    .map(|node| node.bytes.len() as u64)
                    .sum(),
            );
        }
        answer
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.observe(self.inner.pending_waits(owner)).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.observe(self.inner.wait(id)).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.observe(self.inner.process(process)).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.observe(self.inner.live_until_descendants(scope, limit))
            .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.observe(self.inner.until_children(scope, after, limit))
            .await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.observe(self.inner.session_close(session)).await
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.observe(self.inner.ending_scopes(session)).await
    }

    async fn session_mailbox(&self, session: &SessionId) -> Result<SessionMailbox, DurableError> {
        self.observe(self.inner.session_mailbox(session)).await
    }

    async fn prompt_snapshot(
        &self,
        call: &PromptCallKey,
    ) -> Result<Option<PromptSnapshotRow>, DurableError> {
        self.observe(self.inner.prompt_snapshot(call)).await
    }

    async fn prompt_texts(&self, hashes: &[String]) -> Result<Vec<PromptText>, DurableError> {
        self.observe(self.inner.prompt_texts(hashes)).await
    }
}

impl ObservedStore {
    async fn observe<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, DurableError>>,
    ) -> Result<T, DurableError> {
        let (answer, receipt) = sql::collect("code-mode-control", "sqlite", future).await;
        self.observation.read(&receipt);
        answer
    }
}
