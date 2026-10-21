//! Observes the served node's transactions over the same store as its catalog.
use super::observation::Observation;
use lash_core_ids::operational_metrics::sql;
use lash_durable::{domain::*, *};
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use std::sync::Arc;

pub(crate) struct ObservedStore {
    pub(crate) inner: Arc<dyn DurableStore>,
    pub(super) observation: Arc<Observation>,
}

#[async_trait::async_trait]
impl DurableStore for ObservedStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.observe("now", self.inner.now()).await
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn lash_durable::domain::pages::PageStore>> {
        self.inner.pages()
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.observe("register_node", self.inner.register_node(spec))
            .await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.observe("heartbeat", self.inner.heartbeat(node)).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.observe("reap", self.inner.reap(reaper)).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.observe("release_node", self.inner.release_node(node))
            .await
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
        self.observe("readiness", self.inner.readiness()).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.observe("claim", self.inner.claim(node, limit)).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.observe("mark_draining", self.inner.mark_draining(node))
            .await
    }

    async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
        self.observe("live_nodes", self.inner.live_nodes()).await
    }

    async fn actor_requirements(
        &self,
        after: Option<&lash_durable::ActorKey>,
        limit: usize,
    ) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
        self.observe(
            "actor_requirements",
            self.inner.actor_requirements(after, limit),
        )
        .await
    }

    async fn install_packages(
        &self,
        artifacts: &[lash_durable::packages::ArtifactInstall],
        sets: &[lash_durable::packages::SetInstall],
    ) -> Result<(), DurableError> {
        self.observe(
            "install_packages",
            self.inner.install_packages(artifacts, sets),
        )
        .await
    }

    async fn package_catalog(&self) -> Result<lash_durable::PackageCatalog, DurableError> {
        self.observe("package_catalog", self.inner.package_catalog())
            .await
    }

    async fn package_surface(
        &self,
        digest: &lash_durable::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, DurableError> {
        self.observe("package_surface", self.inner.package_surface(digest))
            .await
    }

    async fn package_paths(
        &self,
        functions: &[lash_vocabulary::legacy_names::FunctionId],
    ) -> Result<
        std::collections::BTreeMap<
            lash_vocabulary::legacy_names::FunctionId,
            std::collections::BTreeSet<String>,
        >,
        DurableError,
    > {
        self.observe("package_paths", self.inner.package_paths(functions))
            .await
    }

    async fn internal_operations(
        &self,
        operations: &[lash_vocabulary::legacy_names::EffectName],
    ) -> Result<
        std::collections::BTreeMap<
            lash_vocabulary::legacy_names::EffectName,
            std::collections::BTreeSet<String>,
        >,
        DurableError,
    > {
        self.observe(
            "internal_operations",
            self.inner.internal_operations(operations),
        )
        .await
    }

    async fn package_labels(
        &self,
        artifacts: &[lash_durable::ArtifactDigest],
    ) -> Result<std::collections::BTreeMap<lash_durable::ArtifactDigest, Vec<String>>, DurableError>
    {
        self.observe("package_labels", self.inner.package_labels(artifacts))
            .await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.observe("owned", self.inner.owned(node)).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.observe("begin", self.inner.begin(actor, epoch)).await
    }

    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
        self.observe("open_process", self.inner.open_process(actor, epoch))
            .await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let mut by_kind = std::collections::BTreeMap::new();
        for node in tx.graph().values() {
            let kind = node.envelope().ok().map(|envelope| envelope.kind);
            let name = kind
                .and_then(|kind| kind.spec())
                .map_or("invalid", |spec| spec.name);
            let share = by_kind
                .entry(name.to_owned())
                .or_insert_with(super::observation::KindBytes::default);
            share.nodes += 1;
            share.bytes += node.len();
        }
        self.observation.staged(super::observation::StagedCommit {
            label: label.as_str().to_owned(),
            by_kind,
            domain_writes: tx.domain().len(),
        });
        self.observation.before_commit(label);
        let before = crate::GLOBAL_ALLOCATOR.stats();
        let started = std::time::Instant::now();
        let answer = self.observe("commit", self.inner.commit(tx, label)).await;
        if answer.is_ok() {
            self.observation.delegated(label, before, started);
        }
        answer
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.observe("commit_mail", self.inner.commit_mail(tx, label))
            .await
    }

    async fn claim_facts(
        &self,
        node: Option<&lash_durable::Owner>,
        actor: &ActorKey,
    ) -> Result<lash_durable::ClaimFacts, DurableError> {
        self.inner.claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.observe("actor", self.inner.actor(actor)).await
    }

    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
        self.observe("await_owner_commit", self.inner.await_owner_commit(actor))
            .await
    }

    async fn actors_in(
        &self,
        formats: &lash_durable::FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.observe("actors_in", self.inner.actors_in(formats, after, limit))
            .await
    }
}

#[async_trait::async_trait]
impl DurableReads for ObservedStore {
    async fn model_call_pins(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        self.observe(
            "model_call_pins",
            self.inner.model_call_pins(session, owner),
        )
        .await
    }

    async fn model_calls(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
        self.observe("model_calls", self.inner.model_calls(session, owner))
            .await
    }

    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.observe("turn", self.inner.turn(session)).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnNamespace>, DurableError> {
        self.observe("turn_namespaces", self.inner.turn_namespaces(session, run))
            .await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.observe("turn_end", self.inner.turn_end(session, run))
            .await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        self.observe("run_facts", self.inner.run_facts(owner)).await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        self.observe("run_facts_of", self.inner.run_facts_of(owner, run))
            .await
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
        self.observe("exec_root", self.inner.exec_root(exec)).await
    }

    async fn cell_roots(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        self.observe("cell_roots", self.inner.cell_roots(session, run))
            .await
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
        let answer = self
            .observe("hydrate", self.inner.hydrate(owner, expected, limits))
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
        self.observe("pending_waits", self.inner.pending_waits(owner))
            .await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.observe("wait", self.inner.wait(id)).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.observe("process", self.inner.process(process)).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.observe(
            "live_until_descendants",
            self.inner.live_until_descendants(scope, limit),
        )
        .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.observe(
            "until_children",
            self.inner.until_children(scope, after, limit),
        )
        .await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.observe("session_close", self.inner.session_close(session))
            .await
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.observe("ending_scopes", self.inner.ending_scopes(session))
            .await
    }

    async fn session_mailbox(&self, session: &SessionId) -> Result<SessionMailbox, DurableError> {
        self.observe("session_mailbox", self.inner.session_mailbox(session))
            .await
    }

    async fn prompt_snapshot(
        &self,
        call: &PromptCallKey,
    ) -> Result<Option<PromptSnapshotRow>, DurableError> {
        self.observe("prompt_snapshot", self.inner.prompt_snapshot(call))
            .await
    }

    async fn prompt_texts(&self, hashes: &[String]) -> Result<Vec<PromptText>, DurableError> {
        self.observe("prompt_texts", self.inner.prompt_texts(hashes))
            .await
    }
}

impl ObservedStore {
    async fn observe<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<Output = Result<T, DurableError>>,
    ) -> Result<T, DurableError> {
        let (answer, receipt) = sql::collect(operation, "sqlite", future).await;
        self.observation.read(&receipt);
        answer
    }
}
