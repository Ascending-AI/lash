//! Read-only durable ports and their configured graph hydration.
use super::*;

#[async_trait::async_trait]
impl DurableReads for SqliteDurableStore {
    async fn turn(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<TurnRow>, DurableError> {
        let session = session.clone();
        self.read(move |tx| turns::turn(tx, &session)).await
    }

    async fn turn_end(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        let (session, run) = (session.clone(), run.clone());
        self.read(move |tx| turns::turn_end(tx, &session, &run))
            .await
    }

    async fn turn_namespaces(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        let (session, run) = (session.clone(), run.clone());
        let limits = self.graph_reads.owner();
        self.read(move |tx| turns::turn_namespaces(tx, &session, &run, limits))
            .await
    }

    async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
        let owner = owner.clone();
        let limits = self.graph_reads.owner();
        self.read(move |tx| run_records::facts(tx, &owner, None, limits))
            .await
    }

    async fn run_facts_of(
        &self,
        owner: &OwnerKey,
        run: lash_durable::domain::RunSeq,
    ) -> Result<RunFacts, DurableError> {
        let owner = owner.clone();
        let limits = self.graph_reads.owner();
        self.read(move |tx| run_records::facts(tx, &owner, Some(run), limits))
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        let exec = exec.clone();
        self.read(move |tx| snapshots::read(tx, &exec)).await
    }

    async fn cell_roots(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        let prefix = ExecKey::turn_cells(session, run);
        self.read(move |tx| snapshots::under(tx, &prefix)).await
    }

    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<lash_core_store::checkpoint_graph::NodeId>,
        limits: DecodeLimits,
    ) -> Result<HydratedGraph, DurableError> {
        self.read(move |tx| {
            Ok(graph::hydrate_in(tx, owner, expected, &limits)?
                .map_err(|refusal: GraphRefusal| DurableError::from(refusal)))
        })
        .await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        let owner = owner.clone();
        self.read(move |tx| waits::pending(tx, &owner)).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        let id = *id;
        self.read(move |tx| waits::wait(tx, &id)).await
    }

    async fn process(
        &self,
        process: &lash_sansio::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        processes::process(self, process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        let scope = scope.clone();
        self.read(move |tx| processes::live_until_descendants(tx, &scope, limit))
            .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash_sansio::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        let scope = scope.clone();
        let after = after.cloned();
        self.read(move |tx| processes::until_children(tx, &scope, after.as_ref(), limit))
            .await
    }

    async fn session_close(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        let session = session.clone();
        self.read(move |tx| session_close::read(tx, &session)).await
    }

    async fn ending_scopes(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        let session = session.clone();
        self.read(move |tx| session_close::ending_scopes(tx, &session))
            .await
    }

    async fn session_mailbox(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        let session = session.clone();
        self.read(move |tx| session_mail::read(tx, &session)).await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        let call = call.clone();
        self.read(move |tx| prompts::snapshot(tx, &call)).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        let hashes = hashes.to_vec();
        self.read(move |tx| prompts::texts(tx, &hashes)).await
    }
}
