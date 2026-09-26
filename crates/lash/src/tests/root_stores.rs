//! The logical-root segment of the facade tests' store doubles (FIG-3600 S7).

use super::*;

/// SnapshotStore keeps a root ledger in memory and writes a root's terminal
/// evidence from its commit, exactly as a SQL backend does in the commit's
/// transaction.
#[async_trait]
impl lash_core::store::RootStore for SnapshotStore {
    async fn claim_root_inputs(
        &self,
        request: &lash_core::store::RootInputClaimRequest,
    ) -> std::result::Result<Option<lash_core::AcceptedTurnInputDrive>, lash_core::StoreError> {
        let key = (request.session_id.clone(), request.root.clone());
        let recorded = self.root_claim_results.lock_recover().get(&key).cloned();
        if let Some(result) = recorded {
            // Replayed only while the claim's head is undelivered, as the SQL
            // stores do.
            let undelivered =
                lash_core::TurnInputStore::list_pending_turn_inputs(self, &request.session_id)
                    .await?
                    .iter()
                    .any(|read| read.input.input_id == request.head);
            if undelivered {
                return Ok(Some(result));
            }
        }
        let Some(claim) = lash_core::TurnInputStore::claim_next_turn_inputs(
            self,
            &request.session_id,
            &request.lease,
            &request.owner,
            request.max_inputs,
        )
        .await?
        else {
            return Ok(None);
        };
        if !claim
            .inputs
            .iter()
            .any(|input| input.input_id == request.head)
        {
            lash_core::TurnInputStore::abandon_turn_input_claim(self, &claim).await?;
            return Ok(None);
        }
        let mut base = request.base.clone();
        base.generation = lash_core::SessionCommitStore::read_session_state_version(self).await?;
        lash_core::SessionCommitStore::retain_admission_base(self, &request.lease, &base).await?;
        let input_ids = claim
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>();
        self.roots
            .bind(&request.session_id, &request.root, &input_ids)?;
        let result = lash_core::AcceptedTurnInputDrive::Claimed {
            claim: Box::new(claim),
            base,
            turn_index: request.turn_index,
            generation: request.generation.clone(),
        };
        self.root_claim_results
            .lock_recover()
            .insert(key, result.clone());
        Ok(Some(result))
    }

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Ok(self.roots.terminal(session_id, root))
    }

    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(self.roots.binding(session_id, input))
    }

    async fn root_binding(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(self.roots.binding(session_id, input))
    }

    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &lash_core::TurnId,
        inputs: &[lash_core::InputId],
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.roots.bind(session_id, root, inputs)
    }
}

// The reuse test fails before any turn runs, so this double holds no root.
#[async_trait]
impl lash_core::store::RootStore for BoundSessionStore {
    async fn claim_root_inputs(
        &self,
        _request: &lash_core::store::RootInputClaimRequest,
    ) -> std::result::Result<Option<lash_core::AcceptedTurnInputDrive>, lash_core::StoreError> {
        unreachable!("test should fail before a root claims input on the reused child store")
    }

    async fn root_terminal(
        &self,
        _session_id: &SessionId,
        _root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Ok(None)
    }

    async fn root_of_input(
        &self,
        _session_id: &SessionId,
        _input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(None)
    }

    async fn root_binding(
        &self,
        _session_id: &SessionId,
        _input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(None)
    }

    async fn bind_root_inputs(
        &self,
        _session_id: &SessionId,
        _root: &lash_core::TurnId,
        _inputs: &[lash_core::InputId],
    ) -> std::result::Result<(), lash_core::StoreError> {
        unreachable!("test should fail before a root claims input on the reused child store")
    }
}
