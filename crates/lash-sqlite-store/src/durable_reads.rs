//! Read-only durable ports and their configured graph hydration.
use super::*;

#[async_trait::async_trait]
impl DurableReads for SqliteDurableStore {
    async fn model_call_pins(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
        let (session, owner) = (session.clone(), owner.to_owned());

        self.read(move |tx| model_calls::pins(tx, &session, &owner))
            .await
    }

    async fn model_calls(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
        let (session, owner) = (session.clone(), owner.to_owned());
        let limits = self.graph_reads.owner();
        self.read(move |tx| model_calls::read(tx, &session, &owner, limits))
            .await
    }

    async fn turn(
        &self,
        session: &lash_kernel_value::identity::SessionId,
    ) -> Result<Option<TurnRow>, DurableError> {
        lash_core_ids::operational_metrics::sql::owner_state_read(
            lash_core_ids::operational_metrics::sql::OwnerStateRead::Phase,
        );
        let session = session.clone();
        self.read(move |tx| turns::turn(tx, &session)).await
    }

    async fn turn_end(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        let (session, run) = (session.clone(), run.clone());
        self.query(move |tx| turns::turn_end(tx, &session, &run))
            .await
    }

    async fn turn_namespaces(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
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

    async fn run_facts_within(
        &self,
        owner: &OwnerKey,
        run: Option<lash_durable::domain::RunSeq>,
        max_rows: usize,
        limits: DecodeLimits,
    ) -> Result<RunFacts, DurableError> {
        let owner = owner.clone();
        let policy = self.graph_reads.owner();
        let requested = limits.kernel_data;
        let configured = policy.kernel_data;
        let kernel_data = lash_kernel_value::data_limits::KernelDataLimits::new(
            requested.document_depth().min(configured.document_depth()),
            requested
                .document_json_depth()
                .min(configured.document_json_depth()),
            requested.decode_depth().min(configured.decode_depth()),
            requested
                .state_json_depth()
                .min(configured.state_json_depth()),
            requested.value_depth().min(configured.value_depth()),
        )
        .ok_or_else(|| super::corrupt("diagnostic read limits", "invalid kernel bounds"))?;
        let limits = DecodeLimits {
            kernel_data,
            node_bytes: limits.node_bytes.min(policy.node_bytes),
            total_bytes: limits.total_bytes.min(policy.total_bytes),
            nodes: limits.nodes.min(policy.nodes),
            logical_bytes: limits.logical_bytes.min(policy.logical_bytes),
            depth: limits.depth.min(policy.depth),
        };
        self.read(move |tx| run_records::facts_limited(tx, &owner, run, limits, Some(max_rows)))
            .await
    }

    async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
        let exec = exec.clone();
        self.query(move |tx| snapshots::read(tx, &exec)).await
    }

    async fn cell_roots(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        let prefix = ExecKey::turn_cells(session, run);
        self.query(move |tx| snapshots::under(tx, &prefix)).await
    }

    async fn cell_roots_within(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
        max_rows: usize,
    ) -> Result<Vec<ExecRootRow>, DurableError> {
        let prefix = ExecKey::turn_cells(session, run);
        self.query(move |tx| snapshots::under_limited(tx, &prefix, Some(max_rows)))
            .await
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
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        processes::process(self, process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash_vocabulary::identity::ProcessId>, DurableError> {
        let scope = scope.clone();
        self.read(move |tx| processes::live_until_descendants(tx, &scope, limit))
            .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash_vocabulary::identity::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash_vocabulary::identity::ProcessId>, DurableError> {
        let scope = scope.clone();
        let after = after.cloned();
        self.read(move |tx| processes::until_children(tx, &scope, after.as_ref(), limit))
            .await
    }

    async fn session_close(
        &self,
        session: &lash_kernel_value::identity::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        let session = session.clone();
        self.query(move |tx| session_close::read(tx, &session))
            .await
    }

    async fn ending_scopes(
        &self,
        session: &lash_kernel_value::identity::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        let session = session.clone();
        self.query(move |tx| session_close::ending_scopes(tx, &session))
            .await
    }

    async fn session_mailbox(
        &self,
        session: &lash_kernel_value::identity::SessionId,
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
