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

pub async fn seal_drive_fence_for_test(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    owner_id: &str,
) -> crate::store::DriveFence {
    let _ = owner_id;
    let stored = match store.drive_epoch(session_id).await {
        Ok(stored) => stored,
        Err(crate::StoreError::DriveEpochUnavailable { .. }) => {
            store
                .admit_and_bind_session(&crate::SessionBinding::root(session_id))
                .await
                .expect("admit drive-fence test session");
            store
                .drive_epoch(session_id)
                .await
                .expect("read admitted drive epoch")
        }
        Err(error) => panic!("read drive-fence test drive epoch: {error}"),
    };
    let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
    match store
        .seal_drive_epoch(
            session_id,
            &admission,
            stored.epoch,
            &crate::store::RootStartNonce::new(admission.as_str()),
        )
        .await
        .expect("seal drive-fence test drive")
    {
        crate::store::DriveEpochSeal::Sealed(fence) => fence,
        other => panic!("drive-fence test drive did not seal: {other:?}"),
    }
}

/// Result of sealing a test drive admission for an admission law.
#[derive(Debug)]
pub enum DriveSealTestOutcome {
    Sealed(crate::store::DriveFence),
    Superseded { current_epoch: u64 },
    ExecutionLost,
}

impl DriveSealTestOutcome {
    pub fn acquired(self) -> Option<crate::store::DriveFence> {
        match self {
            Self::Sealed(fence) => Some(fence),
            Self::Superseded { .. } => None,
            Self::ExecutionLost => None,
        }
    }
}

/// Test support for admission laws that need an independent sealed drive epoch.
#[async_trait::async_trait]
pub trait RuntimePersistenceTestDriveExt: crate::RuntimePersistence {
    async fn seal_drive_epoch_for_test(
        &self,
        session_id: &SessionId,
        _owner: &crate::LeaseOwnerIdentity,
        _executor_id: &str,
        _old_lease_ttl_ms: u64,
    ) -> Result<DriveSealTestOutcome, StoreError> {
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
            crate::store::DriveEpochSeal::Sealed(fence) => DriveSealTestOutcome::Sealed(fence),
            crate::store::DriveEpochSeal::Superseded { epoch } => {
                DriveSealTestOutcome::Superseded {
                    current_epoch: epoch,
                }
            }
            crate::store::DriveEpochSeal::ExecutionLost => DriveSealTestOutcome::ExecutionLost,
        })
    }

    async fn supersede_drive_epoch_for_test(
        &self,
        fence: &crate::store::DriveFence,
    ) -> Result<(), StoreError> {
        let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
        let result = self
            .seal_drive_epoch(
                fence.session(),
                &admission,
                fence.epoch(),
                &crate::store::RootStartNonce::new(admission.as_str()),
            )
            .await?;
        match result {
            crate::store::DriveEpochSeal::Sealed(_) => Ok(()),
            crate::store::DriveEpochSeal::Superseded { epoch } => {
                Err(StoreError::StaleDriveFence {
                    session_id: fence.session().clone(),
                    fence_epoch: fence.epoch(),
                    current_epoch: epoch,
                })
            }
            crate::store::DriveEpochSeal::ExecutionLost => Err(StoreError::Backend(
                "test drive successor lost execution".to_string(),
            )),
        }
    }
}

impl<T: crate::RuntimePersistence + ?Sized> RuntimePersistenceTestDriveExt for T {}

/// The admission request conformance laws present for `root` headed by
/// `head` under `fence`: generous bounds, an empty base, and a test build
/// generation.
pub fn admit_root_request_for_test(
    fence: &crate::store::DriveFence,
    root: &crate::TurnId,
    head: crate::store::AdmittedHead,
) -> crate::store::AdmitRootRequest {
    crate::store::AdmitRootRequest {
        fence: fence.clone(),
        root: root.clone(),
        head,
        max_inputs: 64,
        policy: super::queued_work_admission_policy(64),
        base: crate::store::SessionHeadRef {
            generation: 0,
            revision: 0,
            leaf: None,
            checkpoint: None,
        },
        turn_index: 1,
        generation: None,
        admitted_generation: crate::build_generation::BuildGeneration::for_test("conformance"),
    }
}

/// Admit `root`'s turn-lane run headed by `head` under `fence`
/// ([`RootStore::admit_root`](crate::store::RootStore::admit_root)).
pub async fn admit_root_for_test(
    store: &Arc<dyn RuntimePersistence>,
    fence: &crate::store::DriveFence,
    root: &crate::TurnId,
    head: crate::store::AdmittedHead,
) -> Result<Option<crate::store::RootAdmission>, StoreError> {
    store
        .admit_root(&admit_root_request_for_test(fence, root, head))
        .await
}

/// Admit what `root`'s physical turn `turn_id` takes at `checkpoint`, keyed
/// by `step` ([`RootStore::admit_at_checkpoint`](crate::store::RootStore::admit_at_checkpoint)).
#[allow(clippy::too_many_arguments)]
pub async fn admit_at_checkpoint_for_test(
    store: &Arc<dyn RuntimePersistence>,
    fence: &crate::store::DriveFence,
    root: &crate::TurnId,
    turn_id: &crate::TurnId,
    checkpoint: crate::CheckpointKind,
    step: &str,
    max_inputs: usize,
    policy: crate::TurnLaneAdmissionPolicy,
) -> Result<crate::store::CheckpointAdmission, StoreError> {
    store
        .admit_at_checkpoint(&crate::store::CheckpointAdmissionRequest {
            fence: fence.clone(),
            root: root.clone(),
            turn_id: turn_id.clone(),
            checkpoint,
            step: step.to_string(),
            max_inputs,
            policy,
        })
        .await
}

/// Present `fence` on `commit` and have it settle `settlement`: the shape of
/// every commit that settles rows a root admitted (FIG-3927).
pub fn settling_commit_for_test(
    mut commit: RuntimeCommit,
    fence: &crate::store::DriveFence,
    settlement: crate::store::IngressSettlement,
) -> RuntimeCommit {
    commit.drive_fence = Some(Box::new(fence.clone()));
    commit.ingress = Some(settlement);
    commit
}
