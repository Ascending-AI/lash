//! The Runner law observes its begin receipt and holds renewal so replacing
//! the boot cannot stop the old activation before its forced hand-back.

use crate::domain;
use crate::{
    ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError, DurableInstant,
    DurableReads, DurableStore, Epoch, FormatSet, HeartbeatOutcome, MailCommit, MailTx, NodeLease,
    NodeSpec, Reaped,
};
use lash_sansio::{ProcessId, SessionId, TurnId};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) type BeginReceipt = (Epoch, Option<(Epoch, Option<Epoch>)>);

pub(super) struct ObservedStore {
    pub inner: Arc<dyn DurableStore>,
    pub begins: mpsc::UnboundedSender<BeginReceipt>,
}

#[async_trait::async_trait]
impl DurableStore for ObservedStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn crate::domain::pages::PageStore>> {
        self.inner.pages()
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.inner.register_node(spec).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        let _ = node;
        std::future::pending().await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.inner.reap(reaper).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.release_node(node).await
    }

    async fn acquire_idle_for_testing(
        &self,
        node: &crate::NodeLease,
        actor: &crate::ActorKey,
        epoch: crate::Epoch,
    ) -> Result<crate::Epoch, crate::DurableError> {
        self.inner
            .acquire_idle_for_testing(node, actor, epoch)
            .await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.inner.claim(node, limit).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let answer = self.inner.begin(actor, epoch).await;
        let lost = match &answer {
            Err(DurableError::OwnershipLost(fenced)) => Some((fenced.held, fenced.current)),
            _ => None,
        };
        if lost.is_some() {
            let _ = self.begins.send((epoch, lost));
        }
        answer
    }

    async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<crate::ActorCommit, DurableError> {
        let epoch = tx.epoch();
        let answer = self.inner.commit(tx, label).await;
        if label == CommitLabel::DRAIN_RELEASE {
            let lost = match &answer {
                Err(DurableError::OwnershipLost(fenced)) => Some((fenced.held, fenced.current)),
                _ => None,
            };
            let _ = self.begins.send((epoch, lost));
        }
        answer
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
        node: Option<&crate::Owner>,
        actor: &ActorKey,
    ) -> Result<crate::ClaimFacts, DurableError> {
        self.inner.claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }

    async fn actors_in(
        &self,
        formats: &crate::FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.actors_in(formats, after, limit).await
    }
}

#[async_trait::async_trait]
impl DurableReads for ObservedStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<domain::TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<domain::TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_facts(&self, owner: &domain::OwnerKey) -> Result<domain::RunFacts, DurableError> {
        self.inner.run_facts(owner).await
    }

    async fn run_facts_of(
        &self,
        owner: &domain::OwnerKey,
        run: domain::RunSeq,
    ) -> Result<domain::RunFacts, DurableError> {
        self.inner.run_facts_of(owner, run).await
    }
    async fn run_facts_within(
        &self,
        owner: &crate::domain::OwnerKey,
        run: Option<crate::domain::RunSeq>,
        max_rows: usize,
        limits: crate::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<crate::domain::RunFacts, DurableError> {
        self.inner
            .run_facts_within(owner, run, max_rows, limits)
            .await
    }

    async fn exec_root(
        &self,
        exec: &domain::ExecKey,
    ) -> Result<Option<domain::ExecRootRow>, DurableError> {
        self.inner.exec_root(exec).await
    }

    async fn cell_roots(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }
    async fn cell_roots_within(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
        max_rows: usize,
    ) -> Result<Vec<crate::domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots_within(session, run, max_rows).await
    }

    async fn hydrate(
        &self,
        owner: domain::RootOwner,
        expected: Option<domain::checkpoint_graph::NodeId>,
        limits: domain::checkpoint_graph::DecodeLimits,
    ) -> Result<domain::HydratedGraph, DurableError> {
        self.inner.hydrate(owner, expected, limits).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<domain::WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &domain::WaitId) -> Result<Option<domain::WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(
        &self,
        process: &ProcessId,
    ) -> Result<Option<domain::ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &domain::ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &domain::ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<domain::SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(
        &self,
        session: &SessionId,
    ) -> Result<Vec<domain::ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &SessionId,
    ) -> Result<domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn prompt_snapshot(
        &self,
        call: &domain::PromptCallKey,
    ) -> Result<Option<domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}
