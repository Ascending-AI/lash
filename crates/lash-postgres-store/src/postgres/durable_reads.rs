//! The PostgreSQL durable domain read port.
use super::{
    PostgresDurableStore, park_events, processes, prompts, run_records, session_close,
    session_mail, snapshots, turns, waits,
};
use lash_durable::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SessionCloseRow, SnapshotRow, TurnRow, WaitId, WaitRow,
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

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            run_records::read(&mut *self.reader().await?, owner).await
        })
        .await
    }

    async fn run_record_owners(&self, actor: &ActorKey) -> Result<Vec<OwnerKey>, DurableError> {
        self.within(CommitCapacity::Work, async {
            run_records::owners(&mut *self.reader().await?, actor).await
        })
        .await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.within(CommitCapacity::Work, async {
            snapshots::read(&mut *self.reader().await?, exec).await
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
