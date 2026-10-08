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
            turn_budget: crate::TurnBudget::Unbounded,
            max_tool_calls: crate::MaxToolCalls::new(1024),
            no_progress_budget: crate::NoProgressBudget::bounded(12),
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

/// Admit what `run`'s physical turn `turn_id` takes at `checkpoint`, keyed
/// by `step` ([`RunStore::admit_at_checkpoint`](crate::store::RunStore::admit_at_checkpoint)).
#[allow(clippy::too_many_arguments)]
pub async fn admit_at_checkpoint_for_test(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    run: &crate::TurnId,
    turn_id: &crate::TurnId,
    checkpoint: crate::CheckpointKind,
    step: &str,
    max_inputs: usize,
    policy: crate::TurnLaneAdmissionPolicy,
) -> Result<crate::store::CheckpointAdmission, StoreError> {
    store
        .admit_at_checkpoint(&crate::store::CheckpointAdmissionRequest {
            session_id: session_id.clone(),
            run: run.clone(),
            turn_id: turn_id.clone(),
            checkpoint,
            step: step.to_string(),
            max_inputs,
            policy,
        })
        .await
}

/// Have `commit` settle `settlement`: the shape of every commit that settles
/// rows a run admitted (FIG-3927).
pub fn settling_commit_for_test(
    mut commit: RuntimeCommit,
    settlement: crate::store::IngressSettlement,
) -> RuntimeCommit {
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
