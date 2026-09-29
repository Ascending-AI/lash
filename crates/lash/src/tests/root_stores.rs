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
        let session_id = request.session_id();
        let key = (session_id.clone(), request.root.clone());
        let recorded = self.root_claim_results.lock_recover().get(&key).cloned();
        if let Some(admission) = recorded {
            return Ok(Some(admission));
        }
        if let Some(unfinished) =
            lash_core::store::RootStore::unfinished_root(self, session_id).await?
        {
            return Err(lash_core::StoreError::UnfinishedRootConflict {
                session_id: session_id.clone(),
                root: unfinished.root,
            });
        }
        // The double keeps no queued work, so only an input head is ever open.
        let lash_core::store::AdmittedHead::Input(head) = &request.head else {
            return Ok(None);
        };
        // The open next-turn prefix, one run spec wide, bound to the root.
        let inputs = {
            let pending = self.pending_turn_inputs.lock_recover();
            let mut admitted = self.admitted_inputs.lock_recover();
            let open = pending
                .iter()
                .filter(|input| {
                    input.session_id == *session_id
                        && input.state == lash_core::TurnInputState::DeferredNextTurn
                        && !admitted.contains_key(&input.input_id)
                })
                .collect::<Vec<_>>();
            let spec = open.first().map(|input| input.run_spec.clone());
            let inputs = open
                .into_iter()
                .take_while(|input| Some(&input.run_spec) == spec.as_ref())
                .take(request.max_inputs)
                .cloned()
                .collect::<Vec<_>>();
            if !inputs.iter().any(|input| input.input_id == *head) {
                return Ok(None);
            }
            for input in &inputs {
                admitted.insert(input.input_id.clone(), request.root.clone());
            }
            inputs
        };
        let mut base = request.base.clone();
        base.generation = lash_core::SessionCommitStore::read_session_state_version(self).await?;
        lash_core::SessionCommitStore::retain_admission_base(self, &request.fence, &base).await?;
        let input_ids = inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>();
        self.roots.bind(session_id, &request.root, &input_ids)?;
        let admission = lash_core::store::RootAdmission {
            head: request.head.clone(),
            inputs: Some(Box::new(lash_core::runtime::AdmittedTurnInputs {
                session_id: session_id.clone(),
                mode: lash_core::TurnInputAdmissionMode::NextTurn,
                inputs,
                applications: Vec::new(),
            })),
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

    // Checkpoints probe for addressed input and ready work on every turn;
    // this double's tests address none, so every checkpoint admits nothing.
    async fn admit_at_checkpoint(
        &self,
        _request: &lash_core::store::CheckpointAdmissionRequest,
    ) -> std::result::Result<lash_core::store::CheckpointAdmission, lash_core::StoreError> {
        Ok(lash_core::store::CheckpointAdmission::default())
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

    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Vec<lash_core::TurnId>, lash_core::StoreError> {
        let pending = self.pending_turn_inputs.lock_recover();
        Ok(pending
            .iter()
            .filter(|input| {
                input.session_id == *session_id
                    && self.roots.binding(session_id, &input.input_id).as_ref() == Some(root)
            })
            .map(|input| {
                lash_core::TurnId::from(
                    input
                        .source_key
                        .as_deref()
                        .unwrap_or_else(|| input.input_id.as_str()),
                )
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect())
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
        unreachable!("test should fail before a root admits input on the reused child store")
    }
}
