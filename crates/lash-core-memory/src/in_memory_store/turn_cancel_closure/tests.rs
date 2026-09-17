use super::*;
use crate::store::{SessionCommitStore, SessionExecutionLeaseStore, TurnInputStore};

struct Fixture {
    store: InMemorySessionStore,
    clock: Arc<crate::testing::TestClock>,
    lease: crate::SessionExecutionLease,
    state: crate::RuntimeSessionState,
    authorization: crate::TurnCancelClosureAuthorization,
}

impl Fixture {
    async fn new() -> Self {
        let clock = Arc::new(crate::testing::TestClock::new(1_000));
        let store = InMemorySessionStore::with_clock(clock.clone());
        let mut state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        state.session_id = SessionId::from("advisory-closure");
        state.ensure_agent_frame_initialized();
        let lease = store
            .try_claim_session_execution_lease(
                &state.session_id,
                &crate::LeaseOwnerIdentity::opaque("first", "first:1"),
                "first:executor",
                100,
            )
            .await
            .unwrap()
            .acquired()
            .unwrap();
        let address = crate::TurnAddress::new(&state.session_id, crate::TurnId::from("first-turn"));
        let scope = crate::ExecutionScope::process("advisory-closure-process");
        let binding =
            crate::runtime::turn_control_binding_id_for_scope("advisory-closure-binding", &scope)
                .unwrap();
        store
            .validate_turn_cancellation_binding(&state.session_id, &lease.fence(), &binding, &scope)
            .await
            .unwrap();
        let key = |wait, suffix: &str| crate::AwaitEventKey {
            scope: address.execution_scope(),
            wait,
            key_id: suffix.into(),
            signature: suffix.into(),
        };
        let authorization = crate::TurnCancelClosureAuthorization::new(
            address.clone(),
            binding,
            scope,
            key(crate::AwaitEventWaitIdentity::TurnCancelGate, "cancel"),
            key(
                crate::AwaitEventWaitIdentity::TurnCancelEscalation,
                "escalation",
            ),
            key(crate::AwaitEventWaitIdentity::TurnTerminal, "terminal"),
            crate::TurnCancelClosureProposal::CompletionSealed,
            crate::TurnCancelIntentSnapshot::Absent,
            &lease.fence(),
        )
        .unwrap();
        Self {
            store,
            clock,
            lease,
            state,
            authorization,
        }
    }

    async fn expire_and_authorize(&self) {
        self.clock.advance(101);
        assert!(matches!(
            self.store.verify_session_execution_lease(
                &self.state.session_id,
                &self.lease.fence(),
                self.store.clock.timestamp_ms(),
            ),
            Err(crate::StoreError::SessionExecutionLeaseExpired { .. })
        ));
        self.store
            .authorize_turn_cancel_closure(&self.lease.fence(), &self.authorization)
            .await
            .unwrap();
    }

    fn commit(&self) -> crate::store::RuntimeCommit {
        let (mut commit, _) =
            crate::store::RuntimeCommit::persisted_state_for_test(&self.state, &[])
                .with_operation(crate::OperationId::turn(
                    &self.state.session_id,
                    self.authorization.turn_id(),
                    "final",
                ))
                .unwrap();
        commit.interrupted_turn_input_turn_id = Some(self.authorization.turn_id().clone());
        commit.interrupted_turn_cancel_intent = Some(crate::TurnCancelIntentSnapshot::Absent);
        commit.turn_cancel_closure_settlement =
            Some(crate::TurnCancelClosureSettlement::settled_for_test(
                self.authorization.clone(),
                None,
                None,
            ));
        commit.release_session_execution_lease = Some(self.lease.completion());
        commit
    }
}

#[tokio::test]
async fn expired_lease_unchanged_head_without_cancel_record_commits() {
    let fixture = Fixture::new().await;
    fixture.expire_and_authorize().await;
    assert!(
        fixture
            .store
            .load_session_head_meta()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .store
            .retired_turn_cancel_scopes
            .lock_recover()
            .is_empty()
    );
    let commit = fixture.commit();
    fixture
        .store
        .commit_runtime_state(commit.clone())
        .await
        .unwrap();
    let head = fixture
        .store
        .load_session_head_meta()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.head_revision, 1);
    assert!(
        fixture
            .store
            .pending_turn_cancel_closure_pins()
            .await
            .unwrap()
            .is_empty()
    );
    // A later input may require a repair authorization for this same turn.
    fixture
        .store
        .authorize_turn_cancel_closure(&fixture.lease.fence(), &fixture.authorization)
        .await
        .unwrap();
    let mut fresh = fixture.commit();
    fresh.expected_head_revision = 1;
    fresh.turn_commit.operation = crate::OperationId::turn(
        &fixture.state.session_id,
        fixture.authorization.turn_id(),
        "late-final",
    );
    assert!(matches!(
        fixture.store.commit_runtime_state(fresh).await,
        Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch { .. })
    ));
    fixture
        .store
        .commit_runtime_state(commit.clone())
        .await
        .unwrap();
    assert!(
        fixture
            .store
            .pending_turn_cancel_closure_pins()
            .await
            .unwrap()
            .is_empty(),
        "exact receipt replay must consume its matching closure pin"
    );
    let mut different = serde_json::to_value(&fixture.authorization).unwrap();
    different["authorizing_fencing_token"] = serde_json::json!(fixture.lease.fencing_token + 1);
    let different: crate::TurnCancelClosureAuthorization =
        serde_json::from_value(different).unwrap();
    fixture
        .store
        .turn_cancel_closure_authorizations
        .lock_recover()
        .insert(fixture.authorization.turn_id().clone(), different.clone());
    fixture.store.commit_runtime_state(commit).await.unwrap();
    assert_eq!(
        fixture
            .store
            .pending_turn_cancel_closure_pins()
            .await
            .unwrap(),
        vec![different],
        "receipt replay must retain a different pending closure authorization"
    );

    assert_eq!(
        fixture
            .store
            .load_session_head_meta()
            .await
            .unwrap()
            .unwrap()
            .head_revision,
        1
    );
}

#[tokio::test]
async fn expired_lease_successor_head_rejects_predecessor_without_duplicate_publication() {
    let fixture = Fixture::new().await;
    fixture.expire_and_authorize().await;
    let successor = fixture
        .store
        .try_claim_session_execution_lease(
            &fixture.state.session_id,
            &crate::LeaseOwnerIdentity::opaque("successor", "successor:1"),
            "successor:executor",
            60_000,
        )
        .await
        .unwrap()
        .acquired()
        .unwrap();
    assert!(successor.fencing_token > fixture.lease.fencing_token);
    let (successor_commit, _) =
        crate::store::RuntimeCommit::persisted_state_for_test(&fixture.state, &[])
            .with_operation(crate::OperationId::turn(
                &fixture.state.session_id,
                crate::TurnId::from("successor-turn"),
                "final",
            ))
            .unwrap();
    fixture
        .store
        .commit_runtime_state(successor_commit)
        .await
        .unwrap();
    let before = fixture
        .store
        .load_session_head_meta()
        .await
        .unwrap()
        .unwrap();
    let graph_before =
        serde_json::to_value(&*fixture.store.global_session_graph.lock_recover()).unwrap();
    assert!(matches!(
        fixture.store.commit_runtime_state(fixture.commit()).await,
        Err(crate::StoreError::HeadRevisionConflict {
            expected: 0,
            actual: 1
        })
    ));
    let after = fixture
        .store
        .load_session_head_meta()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.schema_version, before.schema_version);
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(
        serde_json::to_value(&after.config).unwrap(),
        serde_json::to_value(&before.config).unwrap()
    );
    assert_eq!(after.current_frame_node_id, before.current_frame_node_id);
    assert_eq!(after.checkpoint_ref, before.checkpoint_ref);
    assert_eq!(after.leaf_node_id, before.leaf_node_id);
    assert_eq!(
        serde_json::to_value(&*fixture.store.global_session_graph.lock_recover()).unwrap(),
        graph_before
    );
    assert_eq!(fixture.store.runtime_turn_commits.lock_recover().len(), 1);
    assert_eq!(
        fixture
            .store
            .pending_turn_cancel_closure_pins()
            .await
            .unwrap(),
        vec![fixture.authorization]
    );
}

#[tokio::test]
async fn expired_lease_retired_scope_refuses_unchanged_head_without_commit() {
    let fixture = Fixture::new().await;
    fixture.expire_and_authorize().await;
    let scope_id = fixture
        .authorization
        .admitted_scope()
        .journal_identity()
        .unwrap()
        .key()
        .to_string();
    fixture
        .store
        .retired_turn_cancel_scopes
        .lock_recover()
        .insert(scope_id.clone());
    assert!(
        fixture
            .store
            .load_session_head_meta()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        matches!(fixture.store.commit_runtime_state(fixture.commit()).await,
        Err(crate::StoreError::TurnCancelClosureScopeRetired { scope_id: actual }) if actual == scope_id)
    );
    assert!(
        fixture
            .store
            .load_session_head_meta()
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.store.runtime_turn_commits.lock_recover().is_empty());
    assert_eq!(
        fixture
            .store
            .pending_turn_cancel_closure_pins()
            .await
            .unwrap(),
        vec![fixture.authorization]
    );
}
