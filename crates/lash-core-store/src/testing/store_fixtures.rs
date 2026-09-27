use crate::*;
use std::sync::Arc;
pub fn durable_turn_scope(
    session_id: impl Into<SessionId>,
    turn_id: impl Into<TurnId>,
) -> ExecutionScope {
    ExecutionScope::turn(session_id, turn_id)
}

/// Admit a scope a test minted directly, standing in for the admission
/// authority's answer.
pub fn durable_admission(scope: &ExecutionScope) -> crate::admitted_scope::AdmittedScope {
    crate::admitted_scope::AdmittedScope::new(scope.clone())
}

pub fn durable_turn_address(
    session_id: impl Into<SessionId>,
    turn_id: impl Into<TurnId>,
) -> crate::TurnAddress {
    crate::TurnAddress::new(session_id, turn_id)
}

pub async fn bind_conformance_session(store: &Arc<dyn RuntimePersistence>, session_id: &SessionId) {
    store
        .admit_and_bind_session(&crate::SessionBinding::root(session_id))
        .await
        .expect("bind conformance store to its explicit session");
}

pub fn append_conformance_event_node(
    state: &mut crate::RuntimeSessionState,
    id: &str,
    content: &str,
) {
    let parent_node_id = state.session_graph.leaf_node_id.clone();
    let node = crate::SessionNodeRecord {
        node_id: crate::NodeId::from(id),
        parent_node_id,
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                crate::ProtocolEvent::typed(
                    "conformance-event",
                    serde_json::json!({ "content": content }),
                )
                .expect("conformance event"),
            ),
        },
    };
    state
        .session_graph
        .apply_append(&crate::GraphAppend::Extend { nodes: vec![node] })
        .expect("append conformance event node");
}

pub async fn commit_conformance_state(
    store: &Arc<dyn crate::RuntimePersistence>,
    state: &mut crate::RuntimeSessionState,
) -> Result<(), crate::StoreError> {
    let operation = crate::OperationId::turn(
        &state.session_id,
        format!("conformance-commit-{}", state.head_revision),
        "commit",
    );
    let (commit, new_node_ids) =
        crate::RuntimeCommit::persisted_state_with_operation(state, &[], operation)?;
    let result = store.commit_runtime_state(commit).await?;
    state.apply_persisted_commit_result(result);
    state.mark_node_ids_persisted(new_node_ids);
    Ok(())
}

pub fn session_store_request(
    session_id: &SessionId,
    model_id: &str,
    relation: crate::SessionRelation,
) -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation,
        policy: crate::SessionPolicy {
            model: crate::ModelSpec::builder(model_id)
                .context_window_tokens(200_000)
                .build()
                .expect("valid conformance model"),
            provider_id: "conformance-provider".to_string(),
            session_id: Some(SessionId::from(session_id.to_string())),
            autonomous: false,
            turn_budget: crate::TurnBudget::Unbounded,
            no_progress_budget: Default::default(),
            charge_safety: Default::default(),
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        },
    }
}

pub async fn commit_runtime_state_for_test(
    store: &Arc<dyn RuntimePersistence>,
    commit: RuntimeCommit,
    _owner_id: &str,
) -> Result<crate::store::RuntimeCommitReceipt, StoreError> {
    store.commit_runtime_state(commit).await
}

pub async fn seal_claim_authority_for_test(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    owner_id: &str,
) -> crate::ClaimAuthority {
    let _ = owner_id;
    let stored = match store.drive_epoch(session_id).await {
        Ok(stored) => stored,
        Err(crate::StoreError::DriveEpochUnavailable { .. }) => {
            store
                .admit_and_bind_session(&crate::SessionBinding::root(session_id))
                .await
                .expect("admit claim-fence test session");
            store
                .drive_epoch(session_id)
                .await
                .expect("read admitted drive epoch")
        }
        Err(error) => panic!("read claim-fence test drive epoch: {error}"),
    };
    let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
    let fence = match store
        .seal_drive_epoch(
            session_id,
            &admission,
            stored.epoch,
            &crate::store::RootStartNonce::new(admission.as_str()),
        )
        .await
        .expect("seal claim-fence test drive")
    {
        crate::store::DriveEpochSeal::Sealed(fence) => fence,
        other => panic!("claim-fence test drive did not seal: {other:?}"),
    };
    crate::ClaimAuthority::from_drive_fence(&fence)
}

/// Result of sealing a test drive admission for a claim law.
#[derive(Debug)]
pub enum DriveClaimTestOutcome {
    Sealed(crate::ClaimAuthority),
    Superseded { current_epoch: u64 },
    ExecutionLost,
}

impl DriveClaimTestOutcome {
    pub fn acquired(self) -> Option<crate::ClaimAuthority> {
        match self {
            Self::Sealed(authority) => Some(authority),
            Self::Superseded { .. } => None,
            Self::ExecutionLost => None,
        }
    }
}

/// Test support for claim laws that need an independent sealed drive epoch.
#[async_trait::async_trait]
pub trait RuntimePersistenceTestClaimExt: crate::RuntimePersistence {
    async fn seal_claim_epoch_for_test(
        &self,
        session_id: &SessionId,
        _owner: &crate::LeaseOwnerIdentity,
        _executor_id: &str,
        _old_lease_ttl_ms: u64,
    ) -> Result<DriveClaimTestOutcome, StoreError> {
        let stored = match self.drive_epoch(session_id).await {
            Ok(stored) => stored,
            Err(crate::StoreError::DriveEpochUnavailable { .. }) => {
                self.admit_and_bind_session(&crate::SessionBinding::root(session_id))
                    .await?;
                self.drive_epoch(session_id).await?
            }
            Err(error) => return Err(error),
        };
        let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
        let seal = self
            .seal_drive_epoch(
                session_id,
                &admission,
                stored.epoch,
                &crate::store::RootStartNonce::new(admission.as_str()),
            )
            .await?;
        Ok(match seal {
            crate::store::DriveEpochSeal::Sealed(fence) => {
                DriveClaimTestOutcome::Sealed(crate::ClaimAuthority::from_drive_fence(&fence))
            }
            crate::store::DriveEpochSeal::Superseded { epoch } => {
                DriveClaimTestOutcome::Superseded {
                    current_epoch: epoch,
                }
            }
            crate::store::DriveEpochSeal::ExecutionLost => DriveClaimTestOutcome::ExecutionLost,
        })
    }

    async fn supersede_claim_epoch_for_test(
        &self,
        authority: &crate::ClaimAuthority,
    ) -> Result<(), StoreError> {
        let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
        let result = self
            .seal_drive_epoch(
                &authority.session_id,
                &admission,
                authority.fencing_token,
                &crate::store::RootStartNonce::new(admission.as_str()),
            )
            .await?;
        match result {
            crate::store::DriveEpochSeal::Sealed(_) => Ok(()),
            crate::store::DriveEpochSeal::Superseded { epoch } => {
                Err(StoreError::StaleDriveFence {
                    session_id: authority.session_id.clone(),
                    fence_epoch: authority.fencing_token,
                    current_epoch: epoch,
                })
            }
            crate::store::DriveEpochSeal::ExecutionLost => Err(StoreError::Backend(
                "test claim successor lost execution".to_string(),
            )),
        }
    }
}

impl<T: crate::RuntimePersistence + ?Sized> RuntimePersistenceTestClaimExt for T {}
