//! Store construction helpers shared by kernel tests and certification fixtures.
use crate::*;
use std::sync::Arc;
pub fn durable_turn_scope(
    session_id: impl Into<SessionId>,
    turn_id: impl Into<TurnId>,
) -> ExecutionScope {
    ExecutionScope::turn(session_id, turn_id)
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
    state
        .session_graph
        .apply_append(&crate::GraphAppend {
            nodes: vec![node],
            leaf_node_id: Some(id.to_string()),
        })
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
    session_id: &SessionId,
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

/// Authorize and settle the completion gate for a direct store-deferral fixture.
/// This performs the same store-owned promise protocol as a real Native turn.
pub async fn authorize_completion_deferral_for_test(
    store: &dyn RuntimePersistence,
    fence: &SessionExecutionLeaseAuthority,
    mut commit: RuntimeCommit,
) -> Result<RuntimeCommit, RuntimeError> {
    let store_error =
        |error: StoreError| RuntimeError::new(RuntimeErrorCode::RuntimeStore, error.to_string());
    let authority = store
        .turn_cancellation_authority()
        .expect("fixture store owns cancellation");
    let address = TurnAddress::new(
        &commit.session_id,
        commit
            .interrupted_turn_input_turn_id
            .as_ref()
            .expect("fixture defers a turn"),
    );
    let scope = address.execution_scope();
    let resolver = authority.resolver();
    store
        .validate_turn_cancellation_binding(
            &commit.session_id,
            fence,
            authority.binding_id(),
            &scope,
        )
        .await
        .map_err(store_error)?;
    let control =
        crate::runtime::turn_control::ActiveTurnControl::new(resolver.as_ref(), address.clone())
            .await?;
    let observed = store
        .turn_cancel_request_intent(&address)
        .await
        .map_err(store_error)?;
    assert_eq!(
        observed,
        TurnCancelIntentSnapshot::Absent,
        "completion fixture has no cancellation intent"
    );
    assert!(commit.interrupted_turn_input_cancellation.is_none());
    let authorization = control.closure_authorization(
        authority.binding_id(),
        scope,
        fence,
        observed.clone(),
        false,
        None,
    )?;
    store
        .authorize_turn_cancel_closure(fence, &authorization)
        .await
        .map_err(store_error)?;
    commit.turn_cancel_closure_settlement = Some(
        control
            .settle_authorized(resolver.as_ref(), &authorization)
            .await?,
    );
    commit.interrupted_turn_cancel_intent = Some(observed);
    commit.session_execution_lease_fence = Some(fence.clone());
    Ok(commit)
}
