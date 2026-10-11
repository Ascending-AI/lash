//! The PostgreSQL durable domain read port.
use super::{
    PostgresDurableStore, park_events, processes, prompts, session_close, session_mail, snapshots,
    turns, waits,
};
use lash_durable::domain::{
    ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow,
    RootOwner, RunFacts, ScopeKey, SessionCloseRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{ActorKey, CommitCapacity, DurableError, DurableReads};

#[async_trait::async_trait]
impl DurableReads for PostgresDurableStore {
    async fn turn(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<TurnRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            turns::turn(&mut *self.reader().await?, session).await
        })
        .await
    }

    async fn turn_end(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        self.within(CommitCapacity::Work, async {
            turns::turn_end(&mut *self.reader().await?, session, run).await
        })
        .await
    }

    async fn turn_namespaces(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        self.within(CommitCapacity::Work, async {
            turns::turn_namespaces(&mut *self.reader().await?, session, run).await
        })
        .await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        self.read_run_facts(owner, None).await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        self.read_run_facts(owner, Some(run)).await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            snapshots::read(&mut *self.reader().await?, exec).await
        })
        .await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash_core_store::checkpoint_graph::NodeId>,
        limits: lash_core_store::checkpoint_graph::DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        self.hydrate_owner(owner, expected, limits).await
    }

    async fn cell_roots(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        let prefix = ExecKey::turn_cells(session, run);
        self.within(CommitCapacity::Work, async {
            snapshots::under(&mut *self.reader().await?, &prefix).await
        })
        .await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            waits::pending(&mut *self.reader().await?, owner).await
        })
        .await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            waits::wait(&mut *self.reader().await?, id).await
        })
        .await
    }

    async fn process(
        &self,
        process: &lash_sansio::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            processes::process(&mut *self.reader().await?, process).await
        })
        .await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        self.within(CommitCapacity::Work, async {
            processes::live_until_descendants(&mut *self.reader().await?, scope, limit).await
        })
        .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash_sansio::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        self.within(CommitCapacity::Work, async {
            processes::until_children(&mut *self.reader().await?, scope, after, limit).await
        })
        .await
    }

    async fn session_close(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            session_close::read(&mut *self.reader().await?, session).await
        })
        .await
    }

    async fn ending_scopes(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        self.within(CommitCapacity::Work, async {
            session_close::ending_scopes(&mut *self.reader().await?, session).await
        })
        .await
    }

    async fn session_mailbox(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        self.within(CommitCapacity::Work, async {
            session_mail::read(&mut *self.reader().await?, session).await
        })
        .await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            park_events::read(&mut *self.reader().await?, after, limit).await
        })
        .await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            prompts::snapshot(&mut *self.reader().await?, call).await
        })
        .await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.within(CommitCapacity::Work, async {
            prompts::texts(&mut *self.reader().await?, hashes).await
        })
        .await
    }
}
