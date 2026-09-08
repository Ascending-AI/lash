//! Store construction helpers shared by kernel tests and certification fixtures.
use crate::*;
use std::sync::Arc;
pub fn durable_turn_scope(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> ExecutionScope {
    ExecutionScope::turn(session_id, turn_id)
}

pub fn durable_turn_address(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> crate::TurnAddress {
    crate::TurnAddress::new(session_id, turn_id)
}

pub async fn bind_conformance_session(store: &Arc<dyn RuntimePersistence>, session_id: &str) {
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
        node_id: id.to_string(),
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
    state.session_graph.push_node_record(node);
    state.session_graph.set_leaf_node_id(Some(id.to_string()));
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
    session_id: &str,
    model_id: &str,
    relation: crate::SessionRelation,
) -> crate::SessionStoreCreateRequest {
    crate::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation,
        policy: crate::SessionPolicy {
            model: crate::ModelSpec::builder(model_id)
                .context_window_tokens(200_000)
                .build()
                .expect("valid conformance model"),
            provider_id: "conformance-provider".to_string(),
            session_id: Some(session_id.to_string()),
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
    owner_id: &str,
) -> Result<crate::store::RuntimeCommitReceipt, StoreError> {
    let session_id = commit.session_id.clone();
    let lease = claim_session_execution_lease_for_test(store, &session_id, owner_id).await;
    store
        .commit_runtime_state(commit.releasing_session_execution_lease(lease.completion()))
        .await
}

pub async fn claim_session_execution_lease_for_test(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &str,
    owner_id: &str,
) -> crate::SessionExecutionLease {
    let owner = crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"));
    store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "claim-session-execution-lease-for-test-executor",
            60_000,
        )
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease is free")
}
