use crate::session_identity::SessionCreationHead;
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

/// Admit `session_id` as a root session of the conformance catalog.
pub async fn admit_conformance_session(store: &Arc<dyn RuntimeStore>, session_id: &SessionId) {
    store
        .admit_session(&root_session_request(session_id))
        .await
        .expect("admit the conformance session");
}

/// [`admit_conformance_session`] recording `policy`: a law whose runtime
/// serves its own model or plugin configuration records the configuration it
/// runs under, as `SessionCreationHead::Config` admission does for a created
/// session — a reopen runs under the recorded config, not the deployment's
/// ambient one (FIG-4553).
pub async fn admit_conformance_session_with_policy(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    policy: crate::SessionPolicy,
) {
    store
        .admit_session(&session_store_request_with_policy(
            session_id,
            crate::SessionRelation::Root,
            policy,
        ))
        .await
        .expect("admit the conformance session");
}

/// A run admission request for `session_id`, under the conformance model.
pub fn root_session_request(session_id: &SessionId) -> crate::SessionStoreCreateRequest {
    session_store_request(
        session_id,
        "conformance-model",
        crate::SessionRelation::Root,
    )
}

/// A run admission request for `session_id` recording `policy`.
pub fn root_session_request_with_policy(
    session_id: &SessionId,
    policy: crate::SessionPolicy,
) -> crate::SessionStoreCreateRequest {
    session_store_request_with_policy(session_id, crate::SessionRelation::Root, policy)
}

pub fn append_conformance_event_node(
    state: &mut crate::RuntimeSessionState,
    id: &str,
    content: &str,
) {
    let parent_node_id = state.session_graph.leaf_node_id.clone();
    let node = crate::SessionNodeRecord {
        node_id: crate::NodeId::fixture(id),
        parent_node_id,
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
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
    store: &Arc<dyn crate::RuntimeStore>,
    state: &mut crate::RuntimeSessionState,
) -> Result<(), crate::StoreError> {
    let operation = crate::OperationId::turn(
        &state.session_id,
        TurnId::fixture(format!("conformance-commit-{}", state.head_revision)),
        "commit",
    );
    let (commit, new_node_ids) =
        crate::RuntimeCommit::persisted_state_with_operation(state, operation)?;
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
    session_store_request_with_policy(
        session_id,
        relation,
        crate::SessionPolicy {
            model: Some(crate::LlmProfileConfig::new(
                crate::RecordedLlmProfile::mint(
                    crate::LlmProfileKey::new(model_id),
                    lash_core_llm::llm_profile::LlmProfileMetadata::builder(model_id)
                        .context_window_tokens(200_000)
                        .build()
                        .expect("valid conformance model"),
                ),
            )),
            attachment_acceptance: Default::default(),
            autonomous: false,
            turn_budget: crate::TurnBudget::Unbounded,
            max_tool_calls: crate::MaxToolCalls::new(1024),
            no_progress_budget: Default::default(),
            charge_safety: Default::default(),
            generation: crate::GenerationOptions::default(),
        },
    )
}

/// [`session_store_request`] recording `policy` rather than the
/// conformance-model default.
pub fn session_store_request_with_policy(
    session_id: &SessionId,
    relation: crate::SessionRelation,
    policy: crate::SessionPolicy,
) -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation,
        config: policy.into(),
        head: SessionCreationHead::Config,
    }
}

pub async fn commit_runtime_state_for_test(
    store: &Arc<dyn RuntimeStore>,
    commit: RuntimeCommit,
    _owner_id: &str,
) -> Result<crate::store::RuntimeCommitReceipt, StoreError> {
    store.commit_runtime_state(commit).await
}

pub async fn seal_shift_fence_for_test(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    owner_id: &str,
) -> crate::store::ShiftFence {
    let _ = owner_id;
    let stored = match store.shift_epoch(session_id).await {
        Ok(stored) => stored,
        Err(crate::StoreError::ShiftEpochUnavailable { .. }) => {
            store
                .admit_session(&root_session_request(session_id))
                .await
                .expect("admit shift-fence test session");
            store
                .shift_epoch(session_id)
                .await
                .expect("read admitted shift epoch")
        }
        Err(error) => panic!("read shift-fence test shift epoch: {error}"),
    };
    let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
    match store
        .seal_shift_epoch(
            session_id,
            &admission,
            stored.epoch,
            &crate::store::RunStartNonce::new(admission.as_str()),
            None,
        )
        .await
        .expect("seal shift-fence test shift")
    {
        crate::store::ShiftEpochSeal::Sealed(fence) => fence,
        other => panic!("shift-fence test shift did not seal: {other:?}"),
    }
}

/// Result of sealing a test shift admission for an admission law.
#[derive(Debug)]
pub enum ShiftSealTestOutcome {
    Sealed(crate::store::ShiftFence),
    Superseded { current_epoch: u64 },
    ExecutionLost,
}

impl ShiftSealTestOutcome {
    pub fn acquired(self) -> Option<crate::store::ShiftFence> {
        match self {
            Self::Sealed(fence) => Some(fence),
            Self::Superseded { .. } => None,
            Self::ExecutionLost => None,
        }
    }
}

/// Test support for admission laws that need an independent sealed shift epoch.
#[async_trait::async_trait]
pub trait RuntimeStoreTestShiftExt: crate::RuntimeStore {
    async fn seal_shift_epoch_for_test(
        &self,
        session_id: &SessionId,
        _owner: &crate::store::LeaseOwnerIdentity,
        _executor_id: &str,
        _old_lease_ttl_ms: u64,
    ) -> Result<ShiftSealTestOutcome, StoreError> {
        let stored = match self.shift_epoch(session_id).await {
            Ok(stored) => stored,
            Err(crate::StoreError::ShiftEpochUnavailable { .. }) => {
                self.admit_session(&root_session_request(session_id))
                    .await?;
                self.shift_epoch(session_id).await?
            }
            Err(error) => return Err(error),
        };
        let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
        let seal = self
            .seal_shift_epoch(
                session_id,
                &admission,
                stored.epoch,
                &crate::store::RunStartNonce::new(admission.as_str()),
                None,
            )
            .await?;
        Ok(match seal {
            crate::store::ShiftEpochSeal::Sealed(fence) => ShiftSealTestOutcome::Sealed(fence),
            crate::store::ShiftEpochSeal::Superseded { epoch } => {
                ShiftSealTestOutcome::Superseded {
                    current_epoch: epoch,
                }
            }
            crate::store::ShiftEpochSeal::ExecutionLost => ShiftSealTestOutcome::ExecutionLost,
            held @ crate::store::ShiftEpochSeal::HeldByAnotherExecutor { .. } => {
                return Err(StoreError::Backend(format!(
                    "a test seal that names no run was refused {held:?}"
                )));
            }
        })
    }

    async fn supersede_shift_epoch_for_test(
        &self,
        fence: &crate::store::ShiftFence,
    ) -> Result<(), StoreError> {
        let admission = crate::store::AdmissionId::new(uuid::Uuid::new_v4().to_string());
        let result = self
            .seal_shift_epoch(
                fence.session(),
                &admission,
                fence.epoch(),
                &crate::store::RunStartNonce::new(admission.as_str()),
                None,
            )
            .await?;
        match result {
            crate::store::ShiftEpochSeal::Sealed(_) => Ok(()),
            crate::store::ShiftEpochSeal::Superseded { epoch } => {
                Err(StoreError::StaleShiftFence {
                    session_id: fence.session().clone(),
                    fence_epoch: fence.epoch(),
                    current_epoch: epoch,
                })
            }
            crate::store::ShiftEpochSeal::ExecutionLost => Err(StoreError::Backend(
                "test shift successor lost execution".to_string(),
            )),
            held @ crate::store::ShiftEpochSeal::HeldByAnotherExecutor { .. } => Err(
                StoreError::Backend(format!("test shift successor was refused {held:?}")),
            ),
        }
    }
}

impl<T: crate::RuntimeStore + ?Sized> RuntimeStoreTestShiftExt for T {}

/// The admission request conformance laws present for `run` headed by
/// `head` under `fence`: generous bounds, an empty base, a test build
/// generation, and the run run as its own engine execution.
pub fn admit_run_request_for_test(
    fence: &crate::store::ShiftFence,
    run: &crate::TurnId,
    head: crate::store::AdmittedHead,
) -> crate::store::AdmitRunRequest {
    crate::store::AdmitRunRequest {
        unsealed_epoch: None,
        fence: fence.clone(),
        run: run.clone(),
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
        admitted_generation: crate::build_generation::BuildGeneration::for_test("conformance"),
        executor: crate::store::RunExecutor::run(&crate::store::AdmissionId::new("fixture#0")),
        plugins: Default::default(),
        turn_cancellation: None,
        trace_scopes: std::sync::Arc::new(lash_trace::UntracedScopes),
    }
}

/// Admit `run`'s turn-lane run headed by `head` under `fence`
/// ([`RunStore::admit_run`](crate::store::RunStore::admit_run)).
pub async fn admit_run_for_test(
    store: &Arc<dyn RuntimeStore>,
    fence: &crate::store::ShiftFence,
    run: &crate::TurnId,
    head: crate::store::AdmittedHead,
) -> Result<Option<crate::store::RunAdmission>, StoreError> {
    store
        .admit_run(&admit_run_request_for_test(fence, run, head))
        .await
}

/// Admit what `run`'s physical turn `turn_id` takes at `checkpoint`, keyed
/// by `step` ([`RunStore::admit_at_checkpoint`](crate::store::RunStore::admit_at_checkpoint)).
#[allow(clippy::too_many_arguments)]
pub async fn admit_at_checkpoint_for_test(
    store: &Arc<dyn RuntimeStore>,
    fence: &crate::store::ShiftFence,
    run: &crate::TurnId,
    turn_id: &crate::TurnId,
    checkpoint: crate::CheckpointKind,
    step: &str,
    max_inputs: usize,
    policy: crate::TurnLaneAdmissionPolicy,
) -> Result<crate::store::CheckpointAdmission, StoreError> {
    store
        .admit_at_checkpoint(&crate::store::CheckpointAdmissionRequest {
            fence: fence.clone(),
            run: run.clone(),
            turn_id: turn_id.clone(),
            checkpoint,
            step: step.to_string(),
            max_inputs,
            policy,
        })
        .await
}

/// Present `fence` on `commit` and have it settle `settlement`: the shape of
/// every commit that settles rows a run admitted (FIG-3927).
pub fn settling_commit_for_test(
    mut commit: RuntimeCommit,
    fence: &crate::store::ShiftFence,
    settlement: crate::store::IngressSettlement,
) -> RuntimeCommit {
    commit.shift_fence = Some(Box::new(fence.clone()));
    commit.ingress = Some(settlement);
    commit
}

/// Admit creation facts for store-level fixtures that commit their own first head.
pub fn session_request_from_meta_for_test(
    meta: crate::SessionMeta,
) -> crate::SessionStoreCreateRequest {
    let mut request = root_session_request(&meta.session_id);
    request.relation = meta.relation;
    request.owning_process_id = meta.owning_process_id;
    request.pending_observer_intents = meta.pending_observer_intents;
    request
}
