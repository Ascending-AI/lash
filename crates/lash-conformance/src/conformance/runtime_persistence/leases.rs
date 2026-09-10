use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn commit_increments_head_and_round_trips_agent_frames(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        policy: SessionPolicy {
            model: ModelSpec::builder("gpt-5.4-mini")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model spec"),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let assignment = state
        .current_agent_frame()
        .expect("initial frame")
        .assignment
        .clone();
    let custom_reason = AgentFrameReason::new("plan_mode");
    let second_frame_key =
        crate::FrameKey::from_caller_material("frame-2").expect("non-empty frame material");
    let second_frame_node_id =
        crate::session_graph::frame_node_id(&state.session_id, second_frame_key.as_str());
    assert!(state.session_graph.append_frame_open_with_id_at(
        second_frame_node_id.to_string(),
        second_frame_key,
        custom_reason.clone(),
        assignment,
        ProtocolTurnOptions::default(),
        "2026-07-27T00:00:00Z".to_string(),
    ));
    state.current_frame_node_id = Some(second_frame_node_id.clone());
    state.agent_frames = state
        .session_graph
        .agent_frame_records(&SessionId::from("root"));
    state.set_execution_state_snapshot(Some(b"frame-vm".to_vec()));

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "commit-round-trip",
    )
    .await
    .expect("commit runtime state");
    let read = store
        .load_session()
        .await
        .expect("load session")
        .expect("session read");

    assert_eq!(
        read.current_frame_node_id.as_deref(),
        Some(second_frame_node_id.as_str())
    );
    let frames = read.graph.agent_frame_records(&SessionId::from("root"));
    assert_eq!(frames.len(), 2);
    let current = frames
        .iter()
        .find(|frame| frame.frame_node_id == second_frame_node_id)
        .expect("current frame");
    assert_eq!(current.reason, custom_reason);
    assert_eq!(
        read.checkpoint.as_ref().and_then(|checkpoint| {
            checkpoint.component_body(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        }),
        Some(&b"frame-vm"[..])
    );
}

pub(super) async fn concurrent_head_revision_cas_applies_exactly_once(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "concurrent-head-cas";
    let lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from(session_id), "cas-owner")
            .await;
    let make_commit = |node_id: &str| {
        let state = RuntimeSessionState {
            session_id: SessionId::from(session_id.to_string()),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        let node = sample_session_node(&SessionId::from(session_id), node_id, None);
        let derived_node_id = node.node_id.clone();
        let commit = RuntimeCommit {
            expected_head_revision: 0,
            current_frame_node_id: Some(crate::FrameNodeId::new(derived_node_id.clone())),
            graph: crate::GraphAppend {
                nodes: vec![node],
                leaf_node_id: Some(derived_node_id),
            },
            ..RuntimeCommit::persisted_state_for_test(&state, &[])
        };
        commit
            .with_operation(crate::OperationId::new(
                crate::ExecutionScope::runtime_operation(format!("head-cas:{node_id}")),
                "commit",
            ))
            .expect("build distinct head-CAS operation")
            .0
    };

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left_commit = make_commit("cas-left");
    let right_commit = make_commit("cas-right");
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store.commit_runtime_state(left_commit).await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store.commit_runtime_state(right_commit).await
    });

    barrier.wait().await;
    let left = left.await.expect("join left head-CAS writer");
    let right = right.await.expect("join right head-CAS writer");
    let winners = [&left, &right]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    let conflicts = [&left, &right]
        .into_iter()
        .filter(|result| matches!(result, Err(StoreError::HeadRevisionConflict { .. })))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent writer must win head CAS, got left={left:?} right={right:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the losing writer must receive HeadRevisionConflict, got left={left:?} right={right:?}"
    );

    let persisted = store
        .load_session()
        .await
        .expect("load state after concurrent head CAS")
        .expect("concurrent head-CAS winner persisted a session");
    assert_eq!(persisted.head_revision, 1, "exactly one commit applied");
    assert_eq!(persisted.graph.nodes.len(), 1, "exactly one graph applied");
    let left_node_id = caller_frame_node_id(&SessionId::from(session_id), "cas-left");
    let right_node_id = caller_frame_node_id(&SessionId::from(session_id), "cas-right");
    assert!(
        persisted.graph.nodes[0].node_id == left_node_id.as_str()
            || persisted.graph.nodes[0].node_id == right_node_id.as_str(),
        "the persisted graph must come from one of the two writers"
    );
    release_session_execution_lease_for_test(&store, &lease).await;
}

pub(super) async fn commit_rejects_a_different_session_id(store: Arc<dyn RuntimePersistence>) {
    let alpha = RuntimeSessionState {
        session_id: SessionId::from("alpha"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&alpha, &[]),
        "bind-alpha",
    )
    .await
    .expect("first commit binds the session");
    let beta = RuntimeSessionState {
        session_id: SessionId::from("beta"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&beta, &[]),
        "bind-beta",
    )
    .await;
    assert!(
        result.is_err(),
        "a single-session store must reject a commit for a different session id"
    );
}

pub(super) async fn load_hydrates_checkpoint_and_usage(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("hydrated"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(9),
    ));
    state.set_plugin_state(Some(PluginState {
        plugins: Default::default(),
    }));
    let usage = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "mock-model".to_string(),
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 5,
        },
        usage_disposition: Default::default(),
    };

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[usage]),
        "hydrate",
    )
    .await
    .expect("commit");

    let read = store.load_session().await.expect("load").expect("session");
    let checkpoint = read.checkpoint.expect("checkpoint");
    assert_eq!(read.session_id, "hydrated");
    assert_eq!(
        checkpoint
            .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .expect("decode dynamic snapshot")
            .expect("dynamic snapshot")
            .generation(),
        9
    );
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(read.token_ledger[0].usage.input_tokens, 11);
}

pub(super) async fn session_execution_lease_contract(store: Arc<dyn RuntimePersistence>) {
    let fresh_retry_owner = lease_owner("fresh-retry-owner");
    let fresh_retry_nonce = crate::LeaseClaimNonce::new();
    let fresh_retry = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("fresh-retry"),
            &fresh_retry_owner,
            "fresh-retry-executor",
            &fresh_retry_nonce,
            120_000,
        )
        .await
        .expect("fresh claim with retry nonce")
        .acquired()
        .expect("fresh retry claim acquired");
    assert_eq!(fresh_retry.lease_token, fresh_retry_nonce.as_str());
    assert_eq!(fresh_retry.lease_term_ms, 120_000);
    let fresh_retried = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("fresh-retry"),
            &fresh_retry_owner,
            "fresh-retry-executor",
            &fresh_retry_nonce,
            120_000,
        )
        .await
        .expect("retry fresh claim")
        .acquired()
        .expect("fresh claim retry remains acquired");
    assert_eq!(fresh_retried.lease_token, fresh_retry.lease_token);
    assert_eq!(fresh_retried.fencing_token, fresh_retry.fencing_token);
    assert_eq!(
        fresh_retried.claimed_at_epoch_ms, fresh_retry.claimed_at_epoch_ms,
        "a retry after an ambiguous fresh acquire must not double-bump generation or claimed-at"
    );
    release_session_execution_lease_for_test(&store, &fresh_retried).await;

    let takeover_predecessor = store
        .try_claim_session_execution_lease(
            &SessionId::from("takeover-retry"),
            &lease_owner("takeover-old"),
            "session-execution-lease-contract-executor",
            0,
        )
        .await
        .expect("claim immediately-expiring takeover predecessor")
        .acquired()
        .expect("takeover predecessor acquired");
    let takeover_owner = lease_owner("takeover-new");
    let takeover_nonce = crate::LeaseClaimNonce::new();
    let takeover = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("takeover-retry"),
            &takeover_owner,
            "takeover-new-executor",
            &takeover_nonce,
            120_000,
        )
        .await
        .expect("take over expired lease")
        .acquired()
        .expect("takeover acquired");
    assert!(takeover.fencing_token > takeover_predecessor.fencing_token);
    assert_eq!(takeover.lease_token, takeover_nonce.as_str());
    let takeover_retried = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("takeover-retry"),
            &takeover_owner,
            "takeover-new-executor",
            &takeover_nonce,
            120_000,
        )
        .await
        .expect("retry takeover")
        .acquired()
        .expect("takeover retry remains acquired");
    assert_eq!(takeover_retried.lease_token, takeover.lease_token);
    assert_eq!(takeover_retried.fencing_token, takeover.fencing_token);
    assert_eq!(
        takeover_retried.claimed_at_epoch_ms, takeover.claimed_at_epoch_ms,
        "a retry after an ambiguous takeover must not double-bump generation or claimed-at"
    );
    release_session_execution_lease_for_test(&store, &takeover_retried).await;

    let owner_a = lease_owner("owner-a");
    let first_nonce = crate::LeaseClaimNonce::for_testing("owner-a-first-token");
    let first = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("root"),
            &owner_a,
            "owner-a-executor",
            &first_nonce,
            120_000,
        )
        .await
        .expect("owner A first claim")
        .acquired()
        .expect("owner A first claim acquired");
    let owner_a_next = crate::LeaseOwnerIdentity::opaque("owner-a", "owner-a:next-incarnation");
    let owner_b = lease_owner("owner-b");
    let owner_c = lease_owner("owner-c");
    let owner_expired = lease_owner("owner-expired");
    let reentry_nonce = crate::LeaseClaimNonce::new();
    let reentered = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("root"),
            &owner_a,
            "owner-a-executor",
            &reentry_nonce,
            130_000,
        )
        .await
        .expect("same incarnation may re-enter live session lease")
        .acquired()
        .expect("same incarnation receives existing session lease");
    assert_ne!(
        reentered.lease_token, first.lease_token,
        "every same-incarnation claim must rotate the lock-lifecycle token"
    );
    assert_eq!(reentered.fencing_token, first.fencing_token);
    assert_eq!(reentered.lease_token, reentry_nonce.as_str());
    assert_eq!(reentered.lease_term_ms, 130_000);
    assert_eq!(
        reentered.claimed_at_epoch_ms, first.claimed_at_epoch_ms,
        "same-incarnation rotation preserves when the lane was first acquired"
    );
    assert!(reentered.expires_at_epoch_ms >= first.expires_at_epoch_ms);
    let retried = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("root"),
            &owner_a,
            "owner-a-executor",
            &reentry_nonce,
            140_000,
        )
        .await
        .expect("retry same claim attempt")
        .acquired()
        .expect("retry observes the claim it already rotated");
    assert_eq!(retried.lease_token, reentered.lease_token);
    assert_eq!(retried.fencing_token, reentered.fencing_token);
    assert_eq!(retried.claimed_at_epoch_ms, reentered.claimed_at_epoch_ms);
    assert_eq!(retried.lease_term_ms, 140_000);
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    &SessionId::from("root"),
                    &owner_a_next,
                    "session-execution-lease-contract-executor-2",
                    60_000
                )
                .await
                .expect("try same owner next incarnation"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude the same owner in a different incarnation"
    );
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    &SessionId::from("root"),
                    &owner_b,
                    "session-execution-lease-contract-executor-3",
                    60_000
                )
                .await
                .expect("try concurrent session lease"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a live session execution lease must exclude concurrent owners"
    );
    let renewed = store
        .renew_session_execution_lease(&reentered.fence(), 150_000)
        .await
        .expect("renew live session lease");
    assert_eq!(renewed.session_id, reentered.session_id);
    assert_eq!(renewed.owner, reentered.owner);
    assert_eq!(renewed.lease_token, reentered.lease_token);
    assert_eq!(renewed.fencing_token, reentered.fencing_token);
    assert_eq!(renewed.lease_term_ms, 150_000);
    assert!(renewed.expires_at_epoch_ms >= reentered.expires_at_epoch_ms);
    let mut lock_lifecycle_authority = reentered.fence();
    lock_lifecycle_authority.fencing_token = lock_lifecycle_authority
        .fencing_token
        .saturating_add(10_000);
    let renewed_by_owner_and_token = store
        .renew_session_execution_lease(&lock_lifecycle_authority, 120_000)
        .await
        .expect("renewal lock lifecycle is predicated only on owner and lease token");
    assert_eq!(
        renewed_by_owner_and_token.fencing_token, reentered.fencing_token,
        "renewal returns the durable generation rather than trusting caller input"
    );

    let mut wrong_owner_current_token = reentered.fence();
    wrong_owner_current_token.owner = owner_b.clone();
    let err = store
        .renew_session_execution_lease(&wrong_owner_current_token, 120_000)
        .await
        .expect_err("the current token alone must not authorize a wrong owner renewal");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseRenewalRefused { .. }
    ));
    let err = store
        .release_session_execution_lease(&wrong_owner_current_token)
        .await
        .expect_err("the current token alone must not authorize a wrong owner release");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));

    let mut stale_fence = reentered.fence();
    stale_fence.lease_token.push_str(":stale");
    let err = store
        .renew_session_execution_lease(&stale_fence, 60_000)
        .await
        .expect_err("stale session lease renew must fail");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseRenewalRefused { .. }
    ));
    let err = store
        .release_session_execution_lease(&crate::SessionExecutionLeaseAuthority {
            session_id: first.session_id.clone(),
            owner: first.owner.clone(),
            executor_id: "owner-a-executor".to_string(),
            lease_token: format!("{}:stale", first.lease_token),
            fencing_token: first.fencing_token,
        })
        .await
        .expect_err("stale release must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    &SessionId::from("root"),
                    &owner_b,
                    "session-execution-lease-contract-executor-4",
                    60_000
                )
                .await
                .expect("try after stale release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "stale release must not clear the live lease"
    );
    // The token scopes the lock lifecycle: a completion retained by the prior
    // holder no longer identifies the successor claim, even though the owner
    // incarnation and fencing generation are unchanged.
    let retained_stale_completion = first.completion();
    let err = store
        .release_session_execution_lease(&retained_stale_completion)
        .await
        .expect_err("a retained predecessor completion must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    &SessionId::from("root"),
                    &owner_b,
                    "session-execution-lease-contract-executor-5",
                    60_000
                )
                .await
                .expect("claim after stale retained release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "a retained predecessor completion must not free the successor claim"
    );

    let mut current_completion = reentered.completion();
    current_completion.fencing_token = current_completion.fencing_token.saturating_add(10_000);
    store
        .release_session_execution_lease(&current_completion)
        .await
        .expect("release lock lifecycle is predicated only on owner and lease token");
    let err = store
        .release_session_execution_lease(&current_completion)
        .await
        .expect_err("repeating an acknowledged release must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    let second =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner-b").await;
    assert!(
        second.fencing_token > first.fencing_token,
        "reclaimed session leases must advance the fencing token"
    );
    let err = store
        .release_session_execution_lease(&first.completion())
        .await
        .expect_err("old release must be refused by name");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseReleaseRefused { .. }
    ));
    assert!(
        matches!(
            store
                .try_claim_session_execution_lease(
                    &SessionId::from("root"),
                    &owner_c,
                    "session-execution-lease-contract-executor-6",
                    60_000
                )
                .await
                .expect("try after old release"),
            crate::SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "old release must not clear a newer lease"
    );
    release_session_execution_lease_for_test(&store, &second).await;

    let expired = store
        .try_claim_session_execution_lease(
            &SessionId::from("root"),
            &owner_expired,
            "session-execution-lease-contract-executor-7",
            0,
        )
        .await
        .expect("claim expiring lease")
        .acquired()
        .expect("expiring lease");
    let reclaimed =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner-reclaim")
            .await;
    assert!(reclaimed.fencing_token > expired.fencing_token);
    release_session_execution_lease_for_test(&store, &reclaimed).await;

    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let lease_free_commit = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("head CAS, not the advisory lease, authorizes commit");
    state.head_revision = lease_free_commit.head_revision;

    let commit_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "commit-owner")
            .await;
    let lease_commit = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(commit_lease.completion()),
        )
        .await
        .expect("advisory lease-bearing commit");
    state.head_revision = lease_commit.head_revision;
    let after_commit =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "after-commit")
            .await;
    release_session_execution_lease_for_test(&store, &after_commit).await;

    let turn_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index: 1,
        head_revision: state.head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let turn_commit = RuntimeCommit::persisted_state_for_test(&turn_state, &[]);
    let mut turn_commit = turn_commit;
    turn_commit.turn_commit = RuntimeTurnCommitStamp::new(crate::OperationId::turn(
        "root",
        "lease-replay-turn",
        "final",
    ));
    let turn_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "turn-owner")
            .await;
    let first_result = store
        .commit_runtime_state(
            turn_commit
                .clone()
                .releasing_session_execution_lease(turn_lease.completion()),
        )
        .await
        .expect("first final commit under session lease");
    let replay = store
        .commit_runtime_state(turn_commit)
        .await
        .expect("idempotent replay returns without live session lease");
    assert_eq!(replay.head_revision, first_result.head_revision);

    let batch = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "fenced queue",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue fenced queue work");
    let err = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &commit_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect_err("queued-work claims require a live session lease");
    assert!(matches!(
        err,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    let queue_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "queue-owner")
            .await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &queue_lease.fence(),
            &lease_owner("queue-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim fenced queue work")
        .claim()
        .expect("queue work claim");
    assert_eq!(claim.batches[0].batch_id, batch.batch_id);
    release_session_execution_lease_for_test(&store, &queue_lease).await;
}

/// A borrowed commit validates the ordinary current-token fence without
/// participating in the lease lifecycle. Run through the shared conformance
/// suite so in-memory, SQLite, PostgreSQL, and perf backends cannot drift.
pub async fn borrowed_session_execution_lease_commit_contract(store: Arc<dyn RuntimePersistence>) {
    let session_id = "borrowed-commit-fence";
    let owner = lease_owner("borrowed-commit-owner");
    let first_nonce = crate::LeaseClaimNonce::for_testing("borrowed-commit-first-token");
    let held = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "borrowed-commit-executor",
            &first_nonce,
            120_000,
        )
        .await
        .expect("claim borrowed-commit lane")
        .acquired()
        .expect("borrowed-commit lane acquired");
    let mut state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let operation = crate::OperationId::new(
        crate::ExecutionScope::runtime_operation("borrowed-commit-replay"),
        "commit",
    );
    let same_operation =
        RuntimeCommit::persisted_state_with_operation_for_testing(&state, &[], operation);
    let committed = store
        .commit_runtime_state(
            same_operation
                .clone()
                .borrowing_session_execution_lease(held.fence()),
        )
        .await
        .expect("borrowed commit accepts the current held authority");
    state.head_revision = committed.head_revision;

    let renewed = store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await
        .expect("borrowed commit leaves the outer guard fence-valid");
    assert_eq!(renewed.lease_token, held.lease_token);
    assert_eq!(renewed.fencing_token, held.fencing_token);

    let replay_successor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "borrowed-commit-executor",
            &crate::LeaseClaimNonce::for_testing("borrowed-commit-replay-token"),
            120_000,
        )
        .await
        .expect("rotate the fence before replaying the same operation")
        .acquired()
        .expect("same-incarnation replay rotation acquired");
    let error = store
        .commit_runtime_state(same_operation.borrowing_session_execution_lease(held.fence()))
        .await
        .expect_err("a stale borrowed fence must veto receipt replay");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    assert_ne!(replay_successor.lease_token, held.lease_token);

    let lapsed = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "borrowed-commit-executor",
            &crate::LeaseClaimNonce::for_testing("borrowed-commit-lapsed-token"),
            0,
        )
        .await
        .expect("rotate to an immediately lapsed borrowed-commit lane")
        .acquired()
        .expect("same-incarnation lapsed lane acquired");
    let error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .borrowing_session_execution_lease(lapsed.fence()),
        )
        .await
        .expect_err("a lapsed guard cannot authorize a borrowed commit");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    let rotated = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "borrowed-commit-executor",
            &crate::LeaseClaimNonce::for_testing("borrowed-commit-rotated-token"),
            120_000,
        )
        .await
        .expect("rotate borrowed-commit lane")
        .acquired()
        .expect("same-incarnation rotation acquired");
    let error = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .borrowing_session_execution_lease(held.fence()),
        )
        .await
        .expect_err("a stale outer guard cannot authorize a borrowed commit");
    assert!(matches!(
        error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));
    let after_rejection = store
        .get_session_execution_lease(&SessionId::from(session_id))
        .await
        .expect("read lane after stale borrow rejection")
        .lease
        .expect("stale borrow rejection leaves successor live");
    assert_eq!(after_rejection.lease_token, rotated.lease_token);
    release_session_execution_lease_for_test(&store, &rotated).await;
}

pub(super) async fn same_incarnation_rotation_gates_claims_not_commits(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let overlap_owner = lease_owner("same-incarnation-overlap");
    let overlap_predecessor_nonce = crate::LeaseClaimNonce::new();
    let overlap_predecessor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("root"),
            &overlap_owner,
            "same-incarnation-executor",
            &overlap_predecessor_nonce,
            60_000,
        )
        .await
        .expect("claim overlap predecessor")
        .acquired()
        .expect("overlap predecessor acquired");
    let overlap_successor_nonce = crate::LeaseClaimNonce::new();
    let overlap_successor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("root"),
            &overlap_owner,
            "same-incarnation-executor",
            &overlap_successor_nonce,
            60_000,
        )
        .await
        .expect("claim overlap successor")
        .acquired()
        .expect("same incarnation overlap successor acquired");
    assert_eq!(
        overlap_successor.fencing_token, overlap_predecessor.fencing_token,
        "same-incarnation overlap must preserve the claim generation"
    );

    let overlap_win = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(overlap_predecessor.completion()),
        )
        .await
        .expect("predecessor may win purely by the current-head CAS");
    state.head_revision = overlap_win.head_revision;
    let live_after_win = store
        .get_session_execution_lease(&SessionId::from("root"))
        .await
        .expect("read overlap successor after predecessor win")
        .lease
        .expect("stale predecessor release must leave successor live");
    assert_eq!(live_after_win.lease_token, overlap_successor.lease_token);

    let stale_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .releasing_session_execution_lease(overlap_predecessor.completion()),
        )
        .await
        .expect_err("predecessor may lose only because the head CAS is stale");
    assert!(matches!(err, StoreError::HeadRevisionConflict { .. }));
    let live_after_loss = store
        .get_session_execution_lease(&SessionId::from("root"))
        .await
        .expect("read overlap successor after predecessor loss")
        .lease
        .expect("CAS-losing predecessor must leave successor live");
    assert_eq!(live_after_loss.lease_token, overlap_successor.lease_token);
    release_session_execution_lease_for_test(&store, &overlap_successor).await;
}

/// One host may open the same session more than once. The runtime-minted
/// executor discriminator keeps those opens out of the reentry arm while the
/// stable host owner remains shared. This law runs unchanged on in-memory,
/// SQLite, PostgreSQL, and the perf conformance backend.
pub async fn same_host_distinct_executors_are_lane_less_without_revoking_holder(
    store: Arc<dyn RuntimePersistence>,
) {
    let owner = crate::LeaseOwnerIdentity::opaque("fig1133-host", "fig1133-boot");
    let first_nonce = crate::LeaseClaimNonce::for_testing("fig1133-first-token");
    let first = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("fig1133-same-host-session"),
            &owner,
            "fig1133-executor-a",
            &first_nonce,
            120_000,
        )
        .await
        .expect("first executor claim")
        .acquired()
        .expect("first executor acquires the lane");
    assert_eq!(first.session_id, "fig1133-same-host-session");
    assert_eq!(first.owner.owner_id, "fig1133-host");
    assert_eq!(first.owner.incarnation_id, "fig1133-boot");
    assert_eq!(first.executor_id, "fig1133-executor-a");
    assert_eq!(first.lease_token, "fig1133-first-token");
    assert_eq!(first.fencing_token, 1);

    let second_nonce = crate::LeaseClaimNonce::for_testing("fig1133-second-token");
    let holder = match store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from("fig1133-same-host-session"),
            &owner,
            "fig1133-executor-b",
            &second_nonce,
            120_000,
        )
        .await
        .expect("second executor receives a typed claim outcome")
    {
        crate::SessionExecutionLeaseClaimOutcome::Busy { holder } => holder,
        crate::SessionExecutionLeaseClaimOutcome::Acquired(_) => {
            panic!("second same-host executor must be lane-less")
        }
    };
    assert_eq!(holder.session_id, "fig1133-same-host-session");
    assert_eq!(holder.owner.owner_id, "fig1133-host");
    assert_eq!(holder.owner.incarnation_id, "fig1133-boot");
    assert_eq!(holder.executor_id, "fig1133-executor-a");
    assert_eq!(holder.lease_token, "fig1133-first-token");
    assert_eq!(holder.fencing_token, 1);
    let holder_after_busy = store
        .get_session_execution_lease(&SessionId::from("fig1133-same-host-session"))
        .await
        .expect("read holder after busy result")
        .lease
        .expect("busy result leaves holder row present");
    assert_eq!(holder_after_busy, first);

    let renewed = store
        .renew_session_execution_lease(&first.fence(), 120_000)
        .await
        .expect("the first holder renews after the second executor is refused");
    assert_eq!(renewed.owner.owner_id, "fig1133-host");
    assert_eq!(renewed.owner.incarnation_id, "fig1133-boot");
    assert_eq!(renewed.executor_id, "fig1133-executor-a");
    assert_eq!(renewed.lease_token, "fig1133-first-token");
    assert_eq!(renewed.fencing_token, 1);

    let mut committed_state = RuntimeSessionState {
        session_id: SessionId::from("fig1133-same-host-session"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let fenced_commit = RuntimeCommit::persisted_state_with_operation_for_testing(
        &committed_state,
        &[],
        crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("fig1133-holder-mid-turn"),
            "commit",
        ),
    );
    let fenced_result = store
        .commit_runtime_state(fenced_commit.borrowing_session_execution_lease(first.fence()))
        .await
        .expect("first holder's fenced mid-turn commit remains authorized");
    assert_eq!(fenced_result.head_revision, 1);
    committed_state.head_revision = 1;

    let make_lane_less_commit = |executor: &'static str| {
        RuntimeCommit::persisted_state_with_operation_for_testing(
            &committed_state,
            &[],
            crate::OperationId::new(
                crate::ExecutionScope::runtime_operation(format!("fig1133-lane-less-{executor}")),
                "commit",
            ),
        )
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_store = Arc::clone(&store);
    let second_store = Arc::clone(&store);
    let first_barrier = Arc::clone(&barrier);
    let second_barrier = Arc::clone(&barrier);
    let first_commit = make_lane_less_commit("a");
    let second_commit = make_lane_less_commit("b");
    let first_writer = crate::task::spawn(async move {
        first_barrier.wait().await;
        first_store.commit_runtime_state(first_commit).await
    });
    let second_writer = crate::task::spawn(async move {
        second_barrier.wait().await;
        second_store.commit_runtime_state(second_commit).await
    });
    barrier.wait().await;
    let first_result = first_writer.await.expect("join first lane-less writer");
    let second_result = second_writer.await.expect("join second lane-less writer");
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StoreError::HeadRevisionConflict {
                    expected: 1,
                    actual: 2
                })
            ))
            .count(),
        1
    );
}

/// Probe the claim/renew race through the public API on every backend. Embedded
/// stores serialize the operations under one writer lock; PostgreSQL uses the
/// same per-session advisory lock. Either linearization is legal, but a renewal
/// that runs after rotation must return the named refusal rather than fabricate
/// success for the stale token.
pub(super) async fn concurrent_session_execution_lease_rotation_and_stale_renewal_are_linearizable(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "concurrent-rotation-renewal";
    let owner = lease_owner("concurrent-rotation-owner");
    let predecessor_nonce = crate::LeaseClaimNonce::for_testing("concurrent-predecessor-token");
    let predecessor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "concurrent-claim-executor",
            &predecessor_nonce,
            120_000,
        )
        .await
        .expect("claim concurrent predecessor")
        .acquired()
        .expect("concurrent predecessor acquired");
    let successor_nonce = crate::LeaseClaimNonce::new();
    let successor_token = successor_nonce.as_str().to_string();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));

    let claim_store = Arc::clone(&store);
    let claim_owner = owner.clone();
    let claim_barrier = Arc::clone(&barrier);
    let claim = crate::task::spawn(async move {
        claim_barrier.wait().await;
        claim_store
            .try_claim_session_execution_lease_with_token(
                &SessionId::from(session_id),
                &claim_owner,
                "concurrent-claim-executor",
                &successor_nonce,
                120_000,
            )
            .await
    });
    let renew_store = Arc::clone(&store);
    let predecessor_fence = predecessor.fence();
    let renew_barrier = Arc::clone(&barrier);
    let renewal = crate::task::spawn(async move {
        renew_barrier.wait().await;
        renew_store
            .renew_session_execution_lease(&predecessor_fence, 120_000)
            .await
    });
    barrier.wait().await;

    let successor = claim
        .await
        .expect("join concurrent rotating claim")
        .expect("concurrent rotating claim")
        .acquired()
        .expect("same-incarnation rotating claim acquired");
    assert_eq!(successor.lease_token, successor_token);
    match renewal.await.expect("join concurrent stale renewal") {
        Ok(renewed) => assert_eq!(
            renewed.lease_token, predecessor.lease_token,
            "a successful renewal must have linearized before token rotation"
        ),
        Err(StoreError::SessionExecutionLeaseRenewalRefused { .. }) => {}
        Err(error) => panic!("concurrent stale renewal returned the wrong error: {error}"),
    }
    let durable = store
        .get_session_execution_lease(&SessionId::from(session_id))
        .await
        .expect("read durable lease after concurrent probe")
        .lease
        .expect("successor remains live after concurrent probe");
    assert_eq!(durable.lease_token, successor.lease_token);
    release_session_execution_lease_for_test(&store, &successor).await;
}

pub(super) async fn session_execution_lease_expires_by_ttl_contract<F>(
    make: &F,
    lease_timing: &RuntimePersistenceLeaseTiming,
) where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    // Pre-expiry observation within a tight semantic TTL window requires an
    // injected clock: on loaded hosts, real database operations can be
    // descheduled beyond the TTL window before the claimant executes.
    // Controlled-clock backends drive this observation deterministically
    // through the injected time source. Realtime backends (such as PostgreSQL)
    // skip this case; their TTL-takeover coverage lives in the
    // `claim_session_execution_lease_after_expiry` contracts (real TTL plus a
    // generous poll allowance), and the server-clock claim-stamp authority is
    // proven by the postgres clock contract.
    let RuntimePersistenceLeaseTiming::Controlled(_) = lease_timing else {
        eprintln!(
            "skipping session_execution_lease_expires_by_ttl_contract: \
             realtime lease timing cannot observe the pre-expiry window \
             deterministically; covered by the after-expiry claim contracts"
        );
        return;
    };

    let store = make();
    let session_id = "ttl-expiry";
    let holder_owner = lease_owner("stale-holder");
    let claimant = lease_owner("ttl-claimant");
    let holder = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &holder_owner,
            "session-execution-lease-expires-by-ttl-contract-executor",
            CONTROLLED_LEASE_TTL_MS,
        )
        .await
        .expect("claim stale-holder lease")
        .acquired()
        .expect("stale-holder lease acquired");

    lease_timing.advance_to_just_before_semantic_expiry();
    let outcome = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &claimant,
            "session-execution-lease-expires-by-ttl-contract-executor-2",
            60_000,
        )
        .await
        .expect("claimant observes stale-holder lease");
    match outcome {
        crate::SessionExecutionLeaseClaimOutcome::Busy {
            holder: busy_holder,
        } => {
            assert_eq!(
                busy_holder.lease_token, holder.lease_token,
                "the busy observation must name the stale-holder lease"
            );
        }
        crate::SessionExecutionLeaseClaimOutcome::Acquired(acquired) => {
            panic!(
                "an unexpired stale lease must remain busy rather than being reclaimed: \
                 successor claimed at {} before holder expiry {}",
                acquired.lease.claimed_at_epoch_ms, holder.expires_at_epoch_ms
            );
        }
    }

    lease_timing.advance_to_semantic_expiry();
    let acquired = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &claimant,
            "session-execution-lease-expires-by-ttl-contract-executor-3",
            60_000,
        )
        .await
        .expect("claim after stale-holder TTL")
        .acquired()
        .expect("stale lease must become claimable after TTL");
    assert!(
        acquired.fencing_token > holder.fencing_token,
        "TTL takeover must advance the fencing token"
    );
    release_session_execution_lease_for_test(&store, &acquired).await;
}

pub(super) async fn claim_session_execution_lease_after_expiry(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    claimant: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
    context: &str,
) -> crate::SessionExecutionLease {
    lease_timing.wait_until_expired().await;
    claim_session_execution_lease_until_acquired(store, session_id, claimant, lease_timing, context)
        .await
}

pub(super) async fn claim_session_execution_lease_until_acquired(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    claimant: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
    context: &str,
) -> crate::SessionExecutionLease {
    let deadline = std::time::Instant::now() + REALTIME_LEASE_STALL_ALLOWANCE;
    loop {
        match store
            .try_claim_session_execution_lease(
                session_id,
                claimant,
                "claim-session-execution-lease-until-acquired-executor",
                60_000,
            )
            .await
            .unwrap_or_else(|error| panic!("claim after {context}: {error}"))
        {
            crate::SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => {
                return acquisition.lease;
            }
            crate::SessionExecutionLeaseClaimOutcome::Busy { holder: _ }
                if matches!(lease_timing, RuntimePersistenceLeaseTiming::Realtime)
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(REALTIME_LEASE_EXPIRY_POLL).await;
            }
            crate::SessionExecutionLeaseClaimOutcome::Busy { holder } => {
                panic!("lease remained busy after {context}: {holder:?}")
            }
        }
    }
}

pub(super) async fn claim_queued_work_under_short_lease(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    owner: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
) -> (crate::SessionExecutionLease, crate::QueuedWorkClaim) {
    let deadline = std::time::Instant::now() + REALTIME_LEASE_STALL_ALLOWANCE;
    loop {
        let lease = store
            .try_claim_session_execution_lease(
                session_id,
                owner,
                "claim-queued-work-under-short-lease-executor",
                lease_timing.scaffolding_lease_ttl_ms(),
            )
            .await
            .expect("claim dead-owner lease")
            .acquired()
            .expect("dead-owner lease acquired");
        match store
            .claim_ready_queued_work(
                session_id,
                &lease.fence(),
                owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .map(crate::QueuedWorkClaimOutcome::claim)
        {
            Ok(Some(claim)) => return (lease, claim),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
                if matches!(lease_timing, RuntimePersistenceLeaseTiming::Realtime)
                    && std::time::Instant::now() < deadline => {}
            Ok(None) => panic!("dead-owner queued-work claim must exist"),
            Err(error) => panic!("dead-owner queued-work claim: {error}"),
        }
    }
}

pub(super) async fn claim_turn_input_under_short_lease(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    owner: &crate::LeaseOwnerIdentity,
    lease_timing: &RuntimePersistenceLeaseTiming,
) -> (crate::SessionExecutionLease, crate::TurnInputClaim) {
    let deadline = std::time::Instant::now() + REALTIME_LEASE_STALL_ALLOWANCE;
    loop {
        let lease = store
            .try_claim_session_execution_lease(
                session_id,
                owner,
                "claim-turn-input-under-short-lease-executor",
                lease_timing.scaffolding_lease_ttl_ms(),
            )
            .await
            .expect("claim dead-owner lease")
            .acquired()
            .expect("dead-owner lease acquired");
        match store
            .claim_next_turn_inputs(session_id, &lease.fence(), owner, 10)
            .await
        {
            Ok(Some(claim)) => return (lease, claim),
            Err(StoreError::SessionExecutionLeaseExpired { .. })
                if matches!(lease_timing, RuntimePersistenceLeaseTiming::Realtime)
                    && std::time::Instant::now() < deadline => {}
            Ok(None) => panic!("dead-owner next-turn claim must exist"),
            Err(error) => panic!("dead-owner next-turn claim: {error}"),
        }
    }
}

/// The diagnostic read reports the durable lease row as a raw fact and never
/// mutates it: unknown sessions and released rows read as absent, a held row
/// reports the exact holder facts, a lapsed row is still reported (expiry is not
/// filtered), and a takeover is visible as a strictly higher generation under a
/// different holder.
pub(super) async fn session_execution_lease_diagnostic_read_contract(
    store: Arc<dyn RuntimePersistence>,
) {
    assert!(
        store
            .get_session_execution_lease(&SessionId::from("lease-diagnostics-unknown"))
            .await
            .expect("diagnostic read of an unknown session succeeds")
            .lease
            .is_none(),
        "an unknown session id must read as no lease rather than erroring"
    );

    let held = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("lease-diagnostics"),
        "diag-a",
    )
    .await;
    let observation = store
        .get_session_execution_lease(&SessionId::from("lease-diagnostics"))
        .await
        .expect("diagnostic read of a held lease");
    assert!(
        observation.observed_at_epoch_ms >= held.claimed_at_epoch_ms,
        "diagnostic store-now must not precede the lease claim"
    );
    let observed = observation.lease.expect("a held lease must be reported");
    assert_eq!(observed.session_id, held.session_id);
    assert_eq!(observed.owner, held.owner);
    assert_eq!(observed.fencing_token, held.fencing_token);
    assert_eq!(observed.lease_token, held.lease_token);
    assert_eq!(observed.claimed_at_epoch_ms, held.claimed_at_epoch_ms);
    assert_eq!(observed.expires_at_epoch_ms, held.expires_at_epoch_ms);

    // Reading must not renew, expire, or re-fence anything: the holder's own
    // renewal still succeeds against the fence it presented before the read.
    store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await
        .expect("a diagnostic read must not invalidate the holder's fence");

    release_session_execution_lease_for_test(&store, &held).await;
    assert!(
        store
            .get_session_execution_lease(&SessionId::from("lease-diagnostics"))
            .await
            .expect("diagnostic read after release")
            .lease
            .is_none(),
        "a released row must read as no holder even though its generation persists"
    );

    // A lapsed holder is the ambiguous case triage must see, so expiry is
    // reported rather than filtered out.
    let lapsing = store
        .try_claim_session_execution_lease(
            &SessionId::from("lease-diagnostics"),
            &lease_owner("diag-lapsed"),
            "session-execution-lease-diagnostic-read-contract-executor",
            0,
        )
        .await
        .expect("claim an immediately expiring lease")
        .acquired()
        .expect("expiring lease acquired");
    let lapsed = store
        .get_session_execution_lease(&SessionId::from("lease-diagnostics"))
        .await
        .expect("diagnostic read of a lapsed lease")
        .lease
        .expect("a lapsed holder must still be reported");
    assert_eq!(lapsed.owner, lapsing.owner);
    assert_eq!(lapsed.fencing_token, lapsing.fencing_token);

    let successor = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("lease-diagnostics"),
        "diag-b",
    )
    .await;
    let after_takeover = store
        .get_session_execution_lease(&SessionId::from("lease-diagnostics"))
        .await
        .expect("diagnostic read after takeover")
        .lease
        .expect("the successor holds the row");
    assert_eq!(after_takeover.owner, successor.owner);
    assert!(
        after_takeover.fencing_token > lapsing.fencing_token,
        "takeover must be visible read-side as a strictly higher generation"
    );
    release_session_execution_lease_for_test(&store, &successor).await;
}

/// A granted claim must name the lapsed holder it displaced, read inside the same
/// atomic operation.
///
/// This is the only truthful report of a takeover. The displaced runner is
/// usually *why* the lease lapsed, so it is frequently dead, frozen, or already
/// replaced; a takeover inferred from its own renewal-failure path is missing in
/// exactly that case, and can name whichever holder happens to be current by the
/// time it wakes rather than the one that displaced it.
///
/// The same vector carries the generation law the displacement is measured
/// against: [`crate::store::SessionExecutionLeaseStore`] is a fencing trait, and
/// ADR 0029 requires every fresh acquisition after release or TTL expiry to mint
/// `previous + 1`. Both halves are checked against the generations this run
/// actually observed, never against a constant, so a store frozen at one
/// generation cannot pass.
///
/// Every implementation answers this, in-process doubles included. A double that
/// reports no displacement silently disables the takeover event for whatever it
/// stands in for, and one that restarts the fence after release reissues a
/// generation that stale claims still pin, which stops fencing working at all.
/// Callers pass a session id they own, because a claim mutates the lane.
pub async fn session_execution_lease_displacement(
    store: &(dyn crate::store::SessionExecutionLeaseStore + '_),
    session_id: &SessionId,
) {
    let first = lease_owner("displacement-first");
    let second = lease_owner("displacement-second");

    // A first claim on a row nobody ever held displaces nobody.
    let opening = store
        .try_claim_session_execution_lease(
            session_id,
            &first,
            "session-execution-lease-displacement-executor",
            0,
        )
        .await
        .expect("first claim")
        .acquisition()
        .expect("an unheld lane is acquirable");
    assert!(
        opening.displaced.is_none(),
        "a first claim must not report displacing anyone: {:?}",
        opening.displaced
    );

    // Taking over a lapsed holder must name that exact holder and generation.
    let takeover = store
        .try_claim_session_execution_lease(
            session_id,
            &second,
            "session-execution-lease-displacement-executor-2",
            60_000,
        )
        .await
        .expect("claim the lapsed lane")
        .acquisition()
        .expect("a lapsed lane is claimable");
    let displaced = takeover.displaced.as_ref().unwrap_or_else(|| {
        panic!(
            "displacing a lapsed holder must be reported on the claim; \
             this store reported nothing, which disables the takeover event"
        )
    });
    assert_eq!(
        displaced.owner, opening.lease.owner,
        "the displacement must name the holder actually displaced"
    );
    assert_eq!(
        displaced.fencing_token, opening.lease.fencing_token,
        "the displacement must name the generation actually displaced"
    );
    assert_eq!(
        takeover.lease.fencing_token,
        opening.lease.fencing_token + 1,
        "a claim over an expired lease must mint exactly the previous generation plus one \
         (ADR 0029): displaced {}, acquired {}",
        opening.lease.fencing_token,
        takeover.lease.fencing_token
    );
    assert_eq!(
        displaced.expired_at_epoch_ms, opening.lease.expires_at_epoch_ms,
        "the displacement must report the lapsed holder's own expiry"
    );

    // Exact same-executor reentry advances nothing, so it displaces nobody.
    let reentry_nonce = crate::LeaseClaimNonce::for_testing("displacement-reentry-token");
    let reentry = store
        .try_claim_session_execution_lease_with_token(
            session_id,
            &second,
            &takeover.lease.executor_id,
            &reentry_nonce,
            60_000,
        )
        .await
        .expect("reenter the live lane")
        .acquisition()
        .expect("the same incarnation reenters its own lease");
    assert_eq!(reentry.lease.fencing_token, takeover.lease.fencing_token);
    assert!(
        reentry.displaced.is_none(),
        "reentry must not report a displacement: {:?}",
        reentry.displaced
    );

    // A holder that released its lane hands it over; the next claimant took
    // nothing from anyone and must not report a takeover.
    store
        .release_session_execution_lease(&reentry.lease.completion())
        .await
        .expect("release the lane");
    let after_release = store
        .try_claim_session_execution_lease(
            session_id,
            &first,
            "session-execution-lease-displacement-executor-3",
            60_000,
        )
        .await
        .expect("claim a released lane")
        .acquisition()
        .expect("a released lane is acquirable");
    assert!(
        after_release.displaced.is_none(),
        "claiming a cleanly released lane displaces nobody: {:?}",
        after_release.displaced
    );
    assert_eq!(
        after_release.lease.fencing_token,
        reentry.lease.fencing_token + 1,
        "a claim after a release must mint exactly the released generation plus one \
         (ADR 0029): released {}, acquired {}. Restarting or repeating the generation here \
         reissues one that stale claims still pin, so fencing stops working",
        reentry.lease.fencing_token,
        after_release.lease.fencing_token
    );
    store
        .release_session_execution_lease(&after_release.lease.completion())
        .await
        .expect("release the reclaimed lane");
}

/// Prove the core-owned execution-fence predicate through a backend's real
/// load/lock/claim path.
///
/// The same vector runs against in-memory, SQLite, PostgreSQL, and the perf
/// store so no implementation can weaken one term locally. Queued-work and
/// leading-command paths receive this coverage transitively through their
/// shared fence-ensure helper rather than duplicating the vector three times.
pub async fn session_execution_lease_fence_authority(store: &dyn RuntimePersistence) {
    let session_id = "lease-fence-authority";
    let owner = lease_owner("lease-fence-owner");
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "lease fence input",
        ))
        .await
        .expect("enqueue input behind the lease fence");
    let predecessor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "lease-fence-executor",
            &crate::LeaseClaimNonce::for_testing("lease-fence-predecessor-token"),
            60_000,
        )
        .await
        .expect("claim fence predecessor")
        .acquired()
        .expect("fence predecessor acquired");
    let successor = store
        .try_claim_session_execution_lease_with_token(
            &SessionId::from(session_id),
            &owner,
            "lease-fence-executor",
            &crate::LeaseClaimNonce::for_testing("lease-fence-successor-token"),
            60_000,
        )
        .await
        .expect("rotate fence token for the same owner")
        .acquired()
        .expect("same-owner successor acquired");
    assert_eq!(predecessor.fencing_token, successor.fencing_token);
    assert_ne!(predecessor.lease_token, successor.lease_token);

    let stale_token = store
        .claim_next_turn_inputs(
            &SessionId::from(session_id),
            &predecessor.fence(),
            &owner,
            1,
        )
        .await
        .expect_err("a retained guard must be rejected after same-owner token rotation");
    assert!(matches!(
        stale_token,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    let mut stale_incarnation = successor.fence();
    stale_incarnation.owner.incarnation_id.push_str(":stale");
    let stale_incarnation = store
        .claim_next_turn_inputs(&SessionId::from(session_id), &stale_incarnation, &owner, 1)
        .await
        .expect_err("a stale holder incarnation must be rejected");
    assert!(matches!(
        stale_incarnation,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    store
        .release_session_execution_lease(&successor.completion())
        .await
        .expect("release live successor before expiry case");
    let expired = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &lease_owner("lease-fence-expired"),
            "session-execution-lease-fence-authority-executor",
            0,
        )
        .await
        .expect("claim immediately expired fence")
        .acquired()
        .expect("immediately expired fence acquired");
    let expired_error = store
        .claim_next_turn_inputs(
            &SessionId::from(session_id),
            &expired.fence(),
            &expired.owner,
            1,
        )
        .await
        .expect_err("an expired lease must be rejected");
    assert!(matches!(
        expired_error,
        StoreError::SessionExecutionLeaseExpired { .. }
    ));

    let current = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &lease_owner("lease-fence-current"),
            "session-execution-lease-fence-authority-executor-2",
            60_000,
        )
        .await
        .expect("claim current fence")
        .acquired()
        .expect("current fence acquired");
    let claim = store
        .claim_next_turn_inputs(
            &SessionId::from(session_id),
            &current.fence(),
            &current.owner,
            1,
        )
        .await
        .expect("the current-token holder must be accepted")
        .expect("the current-token holder claims the pending input");
    assert_eq!(claim.inputs.len(), 1);
    store
        .release_session_execution_lease(&current.completion())
        .await
        .expect("release current fence after acceptance case");
}

/// The durable-backend entry point for the shared lease-acquisition contract.
///
/// The contract itself lives in [`session_execution_lease_displacement`] because
/// it binds every implementation of the fencing trait, doubles included. There is
/// nothing extra a durable backend owes here: the displacement report and the
/// `previous + 1` generation law are both trait-level obligations.
pub(super) async fn session_execution_lease_displacement_contract(
    store: Arc<dyn RuntimePersistence>,
) {
    session_execution_lease_displacement(store.as_ref(), &SessionId::from("lease-displacement"))
        .await;
}

pub(super) async fn session_read_loads_persisted_history(store: Arc<dyn RuntimePersistence>) {
    let root = sample_session_node(&SessionId::from("branchy"), "root-node", None);
    let root_node_id = root.node_id.clone();
    let graph = crate::SessionGraph::from_nodes(
        vec![
            root,
            sample_session_node(
                &SessionId::from("branchy"),
                "left-node",
                Some(&root_node_id),
            ),
            sample_session_node(&SessionId::from("branchy"), "left-leaf", Some("left-node")),
        ],
        Some("left-leaf".to_string()),
    )
    .expect("branch fixture graph is valid");
    let state = RuntimeSessionState {
        session_id: SessionId::from("branchy"),
        current_frame_node_id: Some(crate::FrameNodeId::new(root_node_id.clone())),
        session_graph: graph,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let expected_node_ids = commit
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let expected_leaf_node_id = commit.graph.leaf_node_id.clone();
    commit_runtime_state_for_test(&store, commit, "active-path")
        .await
        .expect("commit linear graph");

    let read = store
        .load_session()
        .await
        .expect("load session history")
        .expect("session history exists");
    assert_eq!(
        read.graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        expected_node_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "session reads must return the persisted leaf-to-root history"
    );
    assert_eq!(read.graph.leaf_node_id, expected_leaf_node_id);
}
