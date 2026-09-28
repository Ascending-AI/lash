//! The logical-root segment of the facade tests' store doubles (FIG-3600 S7).

use super::*;

/// SnapshotStore keeps a root ledger in memory and writes a root's terminal
/// evidence from its commit, exactly as a SQL backend does in the commit's
/// transaction.
#[async_trait]
impl lash_core::store::RootStore for SnapshotStore {
    async fn unfinished_root(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Option<lash_core::store::UnfinishedRoot>, lash_core::StoreError> {
        Ok(self.root_claim_results.lock_recover().iter().find_map(
            |((session, root), admission)| {
                (session == session_id && self.roots.terminal(session, root).is_none()).then(|| {
                    lash_core::store::UnfinishedRoot {
                        root: root.clone(),
                        head: admission.head.clone(),
                    }
                })
            },
        ))
    }

    async fn admit_root(
        &self,
        request: &lash_core::store::AdmitRootRequest,
    ) -> std::result::Result<Option<lash_core::store::RootAdmission>, lash_core::StoreError> {
        let key = (request.session_id.clone(), request.root.clone());
        let recorded = self.root_claim_results.lock_recover().get(&key).cloned();
        if let Some(admission) = recorded {
            return Ok(Some(admission));
        }
        if let Some(unfinished) =
            lash_core::store::RootStore::unfinished_root(self, &request.session_id).await?
        {
            return Err(lash_core::StoreError::UnfinishedRootConflict {
                session_id: request.session_id.clone(),
                root: unfinished.root,
            });
        }
        // The double keeps no queued work, so only an input head is ever open.
        let lash_core::store::AdmittedHead::Input(head) = &request.head else {
            return Ok(None);
        };
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
        if !claim.inputs.iter().any(|input| input.input_id == *head) {
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
        let admission = lash_core::store::RootAdmission {
            head: request.head.clone(),
            inputs: Some(Box::new(claim)),
            queued: None,
            base,
            turn_index: request.turn_index,
            generation: request.generation.clone(),
        };
        self.root_claim_results
            .lock_recover()
            .insert(key, admission.clone());
        Ok(Some(admission))
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
    async fn unfinished_root(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Option<lash_core::store::UnfinishedRoot>, lash_core::StoreError> {
        unreachable!("test should fail before reading unfinished roots")
    }

    async fn admit_root(
        &self,
        _request: &lash_core::store::AdmitRootRequest,
    ) -> std::result::Result<Option<lash_core::store::RootAdmission>, lash_core::StoreError> {
        unreachable!("test should fail before a root is admitted on the reused child store")
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
