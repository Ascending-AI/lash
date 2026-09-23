//! Store-side contract tests for the SQLite effect journal.
//!
//! The group tests exercise [`SqliteEffectReplayRowStore`] directly rather than through
//! a host: what is under test is the *allocation* discipline — which transaction
//! may move a group's settlement counter, and what a fenced-out finalize is
//! allowed to leave behind — and a host would only obscure which write did what.

use super::*;
use lash_core::{
    ChildDrainOutcome, RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome,
};

use lash_core::facade_support::effect_replay_driver::{
    AcceptedGroupChild, EffectCancelOutcome, EffectCancelRequest, EffectClaimObservation,
    EffectCommitState, EffectDischargeOutcome, EffectDischargeRequest, EffectFinalizeOutcome,
    EffectGroupChildCommitOutcome, EffectGroupChildCommitRequest, EffectGroupLifecycle,
    EffectGroupLifecyclePhase, EffectGroupRecord, EffectLeaseFence, EffectTerminal,
    MintingEffectRef,
};

#[test]
fn stored_effect_corruption_is_non_retryable() {
    let error = effect_sqlite_error(sqlite_conversion_error(StoreError::StoredDataCorrupt {
        record_kind: "RuntimeEffectReplay",
        message: "lease_expires_at_ms must be non-negative, got -1".to_string(),
    }));
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SqliteEffectReplayStore
    );
    assert!(!error.code.is_retryable());
}

const SCOPE: &str =
    r#"{"version":2,"kind":"turn","session_id":"s1","execution_id":"effect-group"}"#;
const GROUP: &str = "session:s1/group-1";

async fn row_store() -> SqliteEffectReplayRowStore {
    let conn = SqliteConnection::open_in_memory()
        .await
        .expect("open the in-memory effect database");
    ensure_versioned_schema(&conn, SqliteDatabase::EffectReplay)
        .await
        .expect("provision the effect schema");
    SqliteEffectReplayRowStore {
        completion_keys: CompletionKeys::Unsupported,
        conn,
        clock: Arc::new(lash_core::facade_support::SystemClock),
        registry: Arc::new(crate::scope_fence::RegistryAttachment::default()),
        settlement_key: super::settlement_notify::SettlementNotifierKey::for_memory(),
    }
}

#[tokio::test]
async fn strict_replay_refuses_a_pre_cutover_tool_intent_row_without_reexecution() {
    let scope = ExecutionScope::turn("cutover-session", "cutover-turn");
    let controller = SqliteRuntimeEffectController::memory(scope.clone())
        .await
        .expect("open the in-memory effect journal");
    let v2_identity = lash_core::derive_tool_intent_identity(
        &SessionId::from("cutover-session"),
        "cutover-turn",
        Some("cutover-call"),
        0,
    )
    .expect("derive the v2 identity");
    let v2_invocation = lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(scope.clone(), v2_identity.replay_key.clone())
            .expect("valid cutover address"),
        lash_core::RuntimeAttribution::for_turn("cutover-session", "cutover-turn", 0, 0),
        "cutover-effect",
    )
    .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(
        v2_identity.clone(),
    ));
    let v1_replay_key = lash_core::facade_support::legacy_tool_intent_v1_lookup_key(&v2_invocation)
        .expect("derive the pre-cutover lookup key");
    let mut v1_identity = v2_identity;
    v1_identity.replay_key = v1_replay_key.clone();
    let v1_invocation = lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(scope, v1_replay_key).expect("valid v1 cutover address"),
        lash_core::RuntimeAttribution::for_turn("cutover-session", "cutover-turn", 0, 0),
        "cutover-effect",
    )
    .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(v1_identity));
    let command = lash_core::RuntimeEffectCommand::ExecCode {
        language: "cutover-witness".to_string(),
        code: "return 1".to_string(),
    };
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outcome = || lash_core::RuntimeEffectOutcome::ExecCode {
        result: Box::new(Ok(lash_core::ExecResponse {
            observations: Vec::new(),
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            degraded_bindings: Vec::new(),
            terminal_finish: None,
        })),
    };

    let first_executions = Arc::clone(&executions);
    controller
        .execute_effect(
            lash_core::RuntimeEffectEnvelope::new(v1_invocation, command.clone()),
            lash_core::RuntimeEffectLocalExecutor::testing(move |_| async move {
                first_executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(outcome())
            }),
        )
        .await
        .expect("seed the pre-cutover row");

    controller.start_replay();
    let replay_executions = Arc::clone(&executions);
    let error = controller
        .execute_effect(
            lash_core::RuntimeEffectEnvelope::new(v2_invocation, command),
            lash_core::RuntimeEffectLocalExecutor::testing(move |_| async move {
                replay_executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(outcome())
            }),
        )
        .await
        .expect_err("the v1 row cannot satisfy a v2 strict replay");

    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::ToolIntentReplayKeyFormatCutover
    );
    assert!(error.message.contains("tool-intent:v1:"));
    assert!(error.message.contains("tool-intent:v2:"));
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "strict replay must refuse before invoking the local executor again"
    );
}

fn group_record() -> EffectGroupRecord {
    EffectGroupRecord {
        group_key: GROUP.to_string(),
        scope_id: SCOPE.to_string(),
        session_id: Some(SessionId::from("s1")),
        wake: lash_core::GroupWakePolicy::All,
        loser_disposition: lash_core::LoserPolicy::RunToCompletion,
        expected_children: 2,
        lifecycle: EffectGroupLifecycle::Live,
        created_at_ms: 1_000,
    }
}

fn claim(replay_key: &str, owner: &str) -> EffectClaimRequest {
    EffectClaimRequest {
        scope_id: SCOPE.to_string(),
        session_id: Some(SessionId::from("s1")),
        replay_key: replay_key.to_string(),
        envelope_hash: format!("hash-{replay_key}"),
        envelope_json: format!(r#"{{"json":"{replay_key}","hash":"hash-{replay_key}"}}"#),
        owner_id: owner.to_string(),
        lease_token: format!("token-{owner}"),
        lease_ttl_ms: 30_000,
        sleep: None,
        group_key: Some(GROUP.to_string()),
        minting_effect: None,
        strict_replay: false,
    }
}

fn fence(request: &EffectClaimRequest) -> EffectLeaseFence {
    EffectLeaseFence {
        scope_id: request.scope_id.clone(),
        replay_key: request.replay_key.clone(),
        envelope_hash: request.envelope_hash.clone(),
        owner_id: request.owner_id.clone(),
        lease_token: request.lease_token.clone(),
    }
}

fn terminal(replay_key: &str) -> EffectTerminal {
    EffectTerminal::Completed {
        outcome_json: format!(r#"{{"key":"{replay_key}"}}"#),
    }
}

/// The accepted membership the group's two declared children carry.
///
/// `envelope_json` is opaque to the store — the retained accepted envelope is
/// the raw child request, not the canonical replay form — so the same bytes
/// the claim request writes stand in for it; what the tests assert is which
/// row's copy a read reports.
fn membership() -> Vec<AcceptedGroupChild> {
    [("k1", 0), ("k2", 1)]
        .into_iter()
        .map(|(replay_key, position)| AcceptedGroupChild {
            position,
            replay_key: replay_key.to_string(),
            envelope_json: format!(r#"{{"json":"{replay_key}","hash":"hash-{replay_key}"}}"#),
            command_version: 2,
        })
        .collect()
}

async fn open_and_claim(
    store: &SqliteEffectReplayRowStore,
    keys: &[(&str, &str)],
) -> Vec<EffectLeaseFence> {
    store
        .open_group(&group_record(), &membership())
        .await
        .expect("open the group row");
    let mut fences = Vec::new();
    for (replay_key, owner) in keys {
        let request = claim(replay_key, owner);
        let observation = store.claim(&request).await.expect("claim the child");
        assert!(
            matches!(observation, EffectClaimObservation::Claimed { .. }),
            "the child must be freshly claimed"
        );
        fences.push(fence(&request));
    }
    fences
}

/// The durable commit position a grouped child's finalize took, insisting it
/// took one.
fn allocated_commit_seq(outcome: EffectFinalizeOutcome) -> u64 {
    match outcome {
        EffectFinalizeOutcome::Written {
            commit_seq: Some(sequence),
        } => sequence,
        other => {
            panic!("a grouped child's finalize must allocate a commit position, got {other:?}")
        }
    }
}

async fn next_seq(store: &SqliteEffectReplayRowStore) -> i64 {
    store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT next_seq FROM runtime_effect_group WHERE group_key = ?1",
                [GROUP],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("read the group counter")
}

async fn next_commit_seq(store: &SqliteEffectReplayRowStore) -> i64 {
    store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT next_commit_seq FROM runtime_effect_group WHERE group_key = ?1",
                [GROUP],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("read the commit counter")
}

/// Discharge one committed child, insisting the barrier admits it.
async fn discharge(store: &SqliteEffectReplayRowStore, replay_key: &str) -> u64 {
    match store
        .discharge_child(&EffectDischargeRequest {
            group_key: GROUP.to_string(),
            scope_id: SCOPE.to_string(),
            replay_key: replay_key.to_string(),
            terminal: None,
        })
        .await
        .expect("discharge the child")
    {
        EffectDischargeOutcome::Discharged { settlement_seq }
        | EffectDischargeOutcome::AlreadyDischarged { settlement_seq } => settlement_seq,
        EffectDischargeOutcome::Blocked => {
            panic!(
                "no lower commit position is outstanding, so the barrier must admit {replay_key}"
            )
        }
    }
}

/// Two siblings finalizing must take *different* commit positions, and only
/// their discharges allocate settlement ranks.
///
/// This is the anchor for the group row carrying both counters: an allocator
/// that reads `MAX(commit_seq) + 1` and writes the result is a read-then-write
/// that no fence covers, so two finalizes that interleave both observe the same
/// maximum and both write the same commit position — one child's final silently
/// loses its place in the commit order a drain replays. Each counter lives on a
/// single row precisely so the bump is the transaction's own write.
///
/// **On this backend the race is not reachable, and that is stated rather than
/// implied.** SQLite writes through one `BEGIN IMMEDIATE` connection, so two
/// finalizes serialize whatever the caller does; a read-then-write allocator
/// would still hand out distinct positions here. What this test actually holds
/// is the *invariant* that survives serialization: the commit counter equals
/// the number of committed finals, which a `MAX`-based allocator fails
/// immediately because it never moves the counter at all — plus the §5 law
/// that finalize moves no settlement rank, so a consumer cannot observe a
/// child as settled before its drain discharged. The genuinely concurrent
/// version of this proof is the Postgres sibling, where separate connections
/// can interleave inside one `READ COMMITTED` window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_finalize_allocates_distinct_commit_positions() {
    let store = row_store().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;

    let (first, second) = (terminal("k1"), terminal("k2"));
    let (left, right) = tokio::join!(
        store.finalize(&fences[0], &first),
        store.finalize(&fences[1], &second),
    );
    let mut positions = vec![
        allocated_commit_seq(left.expect("finalize the first child")),
        allocated_commit_seq(right.expect("finalize the second child")),
    ];
    positions.sort_unstable();
    assert_eq!(
        positions,
        vec![1, 2],
        "each committing child must take its own durable commit position"
    );
    assert_eq!(
        next_commit_seq(&store).await,
        2,
        "the commit counter counts finals"
    );
    assert_eq!(
        next_seq(&store).await,
        0,
        "finalize allocates no settlement rank — the discharge does"
    );

    // The barrier holds a later commit behind an undrained earlier one: k2's
    // commit position is 2 only because k1 committed first.
    let outcome = store
        .discharge_child(&EffectDischargeRequest {
            group_key: GROUP.to_string(),
            scope_id: SCOPE.to_string(),
            replay_key: "k2".to_string(),
            terminal: None,
        })
        .await
        .expect("probe the out-of-order discharge");
    assert_eq!(
        outcome,
        EffectDischargeOutcome::Blocked,
        "a committed child may not jump its undrained lower-commit sibling"
    );

    // Discharged in commit order, the children take ranks in that order.
    assert_eq!(discharge(&store, "k1").await, 1);
    assert_eq!(discharge(&store, "k2").await, 2);
    assert_eq!(next_seq(&store).await, 2, "the counter counts settlements");
}

/// A boundary-committed row is `committed` but still `in_progress`: its drain
/// is owed. A terminal-less discharge against it must report `Blocked` and
/// roll its rank bump back — never seat a rank on a child whose projected
/// terminal does not exist yet.
#[tokio::test]
async fn a_committed_child_still_draining_cannot_take_a_rank_without_its_terminal() {
    let store = row_store().await;
    open_and_claim(&store, &[("k1", "owner-a")]).await;

    let outcome = store
        .commit_group_child(&EffectGroupChildCommitRequest {
            group_key: Some(GROUP.to_string()),
            scope_id: SCOPE.to_string(),
            replay_key: "k1".to_string(),
            drain_input: "{}".to_string(),
            owner_id: "owner-a".to_string(),
        })
        .await
        .expect("commit the child's final at the boundary");
    assert!(
        matches!(
            outcome,
            EffectGroupChildCommitOutcome::Committed { commit_seq: 1, .. }
        ),
        "the boundary commit allocates the first commit position: {outcome:?}"
    );

    let outcome = store
        .discharge_child(&EffectDischargeRequest {
            group_key: GROUP.to_string(),
            scope_id: SCOPE.to_string(),
            replay_key: "k1".to_string(),
            terminal: None,
        })
        .await
        .expect("probe the mid-drain discharge");
    assert_eq!(
        outcome,
        EffectDischargeOutcome::Blocked,
        "a committed row still in_progress owes its drain; no rank seats"
    );

    let (commit_state, settlement_seq) = store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT commit_state, settlement_seq FROM runtime_effect_replay
                 WHERE scope_id = ?1 AND replay_key = 'k1'",
                [SCOPE],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
        })
        .await
        .expect("read the child back");
    assert_eq!(
        commit_state, "committed",
        "the blocked discharge seats nothing"
    );
    assert_eq!(
        settlement_seq, None,
        "a blocked discharge allocates no rank"
    );
    assert_eq!(
        next_seq(&store).await,
        0,
        "the rank bump rolled back with the blocked discharge"
    );

    // The finishing write carries the projected terminal and discharges.
    let outcome = store
        .discharge_child(&EffectDischargeRequest {
            group_key: GROUP.to_string(),
            scope_id: SCOPE.to_string(),
            replay_key: "k1".to_string(),
            terminal: Some(terminal("k1")),
        })
        .await
        .expect("discharge with the drained terminal");
    assert_eq!(
        outcome,
        EffectDischargeOutcome::Discharged { settlement_seq: 1 },
        "the terminal seats the drain and the rank together"
    );
}

/// A finalize that loses the lease fence must change nothing at all — including
/// both of the group's counters.
///
/// Bumping first and writing the child second would burn a commit position on a
/// write that never lands: the group would count a final no row carries, and a
/// drain ordered over commit positions would wait on a slot nothing occupies.
/// This is why the fenced `UPDATE` runs first and the counter moves only on
/// rowcount 1.
#[tokio::test]
async fn a_fence_miss_allocates_nothing() {
    let store = row_store().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a")]).await;
    let stale = EffectLeaseFence {
        lease_token: "token-superseded".to_string(),
        ..fences[0].clone()
    };

    let outcome = store
        .finalize(&stale, &terminal("k1"))
        .await
        .expect("a fenced-out finalize is not an error");
    assert!(
        matches!(outcome, EffectFinalizeOutcome::FenceMoved),
        "a stale lease token must report the fence moved"
    );
    assert_eq!(
        next_commit_seq(&store).await,
        0,
        "a finalize that wrote no child row must not consume a commit position"
    );
    assert_eq!(
        next_seq(&store).await,
        0,
        "a finalize that wrote no child row must not consume a settlement rank"
    );
    assert!(
        store
            .read_group_settlement(GROUP, 1)
            .await
            .expect("read rank 1")
            .is_none(),
        "no settlement may be readable when no child settled"
    );
}

/// The commit-position CHECK is group-aware: a grouped row's `commit_seq` is
/// mandatory in `committed` and `drained` alike, while an ungrouped row's
/// terminal commit state legitimately carries none — a naive biconditional
/// would reject the second case, and the old state-only guard admitted the
/// first.
#[tokio::test]
async fn the_commit_seq_check_holds_grouped_rows_to_their_position() {
    let store = row_store().await;
    let _fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;

    for commit_state in ["committed", "drained"] {
        let error = store
            .conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE runtime_effect_replay
                     SET commit_state = ?1, commit_seq = NULL
                     WHERE replay_key = 'k1'",
                    [commit_state],
                )
            })
            .await
            .expect_err("a grouped terminal commit state with no position is unwritable");
        assert!(
            error
                .to_string()
                .contains("ck_runtime_effect_replay_commit_seq"),
            "the group-aware CHECK is what rejects it: {error}"
        );
    }

    // The ungrouped case the naive biconditional would have broken: a row with
    // no group may be `committed`/`drained` with no commit position — commit
    // order is only a group property.
    let ungrouped = EffectClaimRequest {
        replay_key: "u1".to_string(),
        group_key: None,
        ..claim("u1", "owner-u")
    };
    let observation = store
        .claim(&ungrouped)
        .await
        .expect("claim the ungrouped row");
    assert!(matches!(
        observation,
        EffectClaimObservation::Claimed { .. }
    ));
    for commit_state in ["committed", "drained"] {
        // The rank-pairing CHECK seats a settlement rank on `drained` even
        // without a group: the state, not the grouping, owns that pairing.
        let settlement = if commit_state == "drained" {
            ", settlement_seq = 1"
        } else {
            ""
        };
        store
            .conn
            .call(move |conn| {
                conn.execute(
                    &format!(
                        "UPDATE runtime_effect_replay
                         SET commit_state = ?1, commit_seq = NULL{settlement}
                         WHERE replay_key = 'u1'"
                    ),
                    [commit_state],
                )
            })
            .await
            .unwrap_or_else(|error| {
                panic!("an ungrouped {commit_state} row may carry no commit position: {error}")
            });
    }
}

/// `drain_input` is a committed-row fact: it is unwritable on a pending or
/// ungrouped row, where nothing could legitimately have staged a resumable
/// drain.
#[tokio::test]
async fn the_drain_input_check_confines_it_to_decided_rows() {
    let store = row_store().await;
    let _fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;

    let error = store
        .conn
        .call(|conn| {
            conn.execute(
                "UPDATE runtime_effect_replay SET drain_input = '{\"x\":1}'
                 WHERE replay_key = 'k1'",
                [],
            )
        })
        .await
        .expect_err("a pending grouped row must not carry drain input");
    assert!(
        error
            .to_string()
            .contains("ck_runtime_effect_replay_drain_input"),
        "the drain-input CHECK is what rejects it: {error}"
    );

    let ungrouped = EffectClaimRequest {
        replay_key: "u1".to_string(),
        group_key: None,
        ..claim("u1", "owner-u")
    };
    store
        .claim(&ungrouped)
        .await
        .expect("claim the ungrouped row");
    let error = store
        .conn
        .call(|conn| {
            conn.execute(
                "UPDATE runtime_effect_replay SET drain_input = '{\"x\":1}'
                 WHERE replay_key = 'u1'",
                [],
            )
        })
        .await
        .expect_err("an ungrouped row has no drain to resume");
    assert!(
        error
            .to_string()
            .contains("ck_runtime_effect_replay_drain_input"),
        "the drain-input CHECK is what rejects it: {error}"
    );
}

/// Ranks are read by position, not by sequence equality: the (n+1)-th smallest
/// sequence is the (n+1)-th settlement even when the numbers are not contiguous.
#[tokio::test]
async fn settlements_are_read_by_rank_not_by_sequence_value() {
    let store = row_store().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;
    for fence in &fences {
        store
            .finalize(fence, &terminal(&fence.replay_key))
            .await
            .expect("finalize the child");
        discharge(&store, &fence.replay_key).await;
    }
    // Open a gap the way a retried allocation would: the ranks a consumer reads
    // must not move.
    store
        .conn
        .write(|tx| {
            tx.execute(
                "UPDATE runtime_effect_replay SET settlement_seq = 9 WHERE replay_key = 'k2'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("widen the settlement gap");

    let first = store
        .read_group_settlement(GROUP, 1)
        .await
        .expect("read rank 1")
        .expect("rank 1 is settled");
    let second = store
        .read_group_settlement(GROUP, 2)
        .await
        .expect("read rank 2")
        .expect("rank 2 is settled");
    assert_eq!(first.replay_key, "k1");
    assert_eq!(second.replay_key, "k2");
    assert_eq!(second.sequence, 9, "the gap is real, the rank is unchanged");
    assert!(
        store
            .read_group_settlement(GROUP, 3)
            .await
            .expect("read rank 3")
            .is_none(),
        "a rank past the settled count must read as absent, never as a wrapped rank"
    );
}

/// Retirement takes a group and its children together: a group row outliving its
/// children keeps a counter no settlement can be matched to, and children
/// outliving their group settle against a missing allocator.
#[tokio::test]
async fn retirement_removes_a_group_and_its_children_together() {
    let store = row_store().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;
    store
        .finalize(&fences[0], &terminal("k1"))
        .await
        .expect("finalize the first child");
    discharge(&store, "k1").await;

    // Session retirement refuses while the group is live (ADR 0099 §7);
    // settle the lifecycle first so the delete is the thing under test.
    store
        .transition_group_lifecycle(
            GROUP,
            &[EffectGroupLifecyclePhase::Live],
            &EffectGroupLifecycle::Settled {
                disposition: lash_core::LoserPolicy::RunToCompletion,
            },
        )
        .await
        .expect("settle the group lifecycle");

    let removed = store
        .retire_journal(&lash_core::EffectJournalRetirement::Session {
            session_id: SessionId::from("s1"),
        })
        .await
        .expect("retire the session journal");
    assert_eq!(removed, 2, "retirement reports the children it removed");

    let groups: i64 = store
        .conn
        .call(|conn| {
            conn.query_row("SELECT count(*) FROM runtime_effect_group", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .await
        .expect("count group rows");
    assert_eq!(groups, 0, "the group row goes with its children");
    let pending = store
        .pending_artifact_owner_retirements()
        .await
        .expect("read session-scope artifact cleanup evidence");
    assert_eq!(
        pending.len(),
        1,
        "the deleted session scope stays recoverable"
    );
    let identity = pending[0]
        .journal_identity()
        .expect("durable scope identity");
    store
        .complete_artifact_owner_retirement(identity.key())
        .await
        .expect("ack artifact owner cleanup");
    assert!(
        store
            .pending_artifact_owner_retirements()
            .await
            .expect("read acknowledged cleanup")
            .is_empty()
    );
}

/// The unsettled read is the exact complement of the rank read: every child of
/// the group is in one answer or the other, never both and never neither.
///
/// It is what makes "this group is complete" a single question instead of a walk
/// up the ranks, and it is the drain queue FIG-1536 reads — which is why the
/// read is anchored on the retained membership rather than the replay row: an
/// accepted-but-never-claimed child is unsettled work with no replay row, and
/// a committed-but-undrained child is unsettled work whose terminal already
/// committed.
#[tokio::test]
async fn unsettled_children_are_exactly_the_children_without_a_rank() {
    let store = row_store().await;
    store
        .open_group(&group_record(), &membership())
        .await
        .expect("open the group row");

    // Before any claim the whole membership is already the unsettled set: an
    // accepted child with no replay row is work the drain must still see.
    let unsettled = store
        .read_unsettled_group_children(GROUP)
        .await
        .expect("read the unsettled children before any claim");
    assert_eq!(
        unsettled
            .iter()
            .map(|child| child.replay_key.as_str())
            .collect::<Vec<_>>(),
        vec!["k1", "k2"],
        "accepted membership is unsettled from the moment the group opens"
    );
    assert_eq!(unsettled[0].scope_id, SCOPE);
    assert_eq!(
        unsettled[0].state, None,
        "a never-claimed child has no replay row state"
    );
    assert_eq!(unsettled[0].commit_state, None);
    assert_eq!(
        unsettled[0].envelope_json, r#"{"json":"k1","hash":"hash-k1"}"#,
        "the row carries the retained envelope a drain re-executes from"
    );

    let fences = {
        let mut fences = Vec::new();
        for (replay_key, owner) in [("k1", "owner-a"), ("k2", "owner-b")] {
            let request = claim(replay_key, owner);
            let observation = store.claim(&request).await.expect("claim the child");
            assert!(
                matches!(observation, EffectClaimObservation::Claimed { .. }),
                "the child must be freshly claimed"
            );
            fences.push(fence(&request));
        }
        fences
    };

    let unsettled = store
        .read_unsettled_group_children(GROUP)
        .await
        .expect("read the unsettled children");
    assert_eq!(
        unsettled
            .iter()
            .map(|child| child.replay_key.as_str())
            .collect::<Vec<_>>(),
        vec!["k1", "k2"],
        "a claimed child with no terminal holds no rank, so it is unsettled"
    );
    assert_eq!(
        unsettled[0].state,
        Some(effect_replay_driver::EffectRowState::InProgress)
    );
    assert!(
        unsettled[0].lease_expires_at_ms > 0,
        "the row carries the lease boundary a drain decides takeover against"
    );

    // A committed child stays unsettled until its drain discharges: finalize
    // allocates the commit position, not the settlement rank.
    store
        .finalize(&fences[0], &terminal("k1"))
        .await
        .expect("finalize the first child");
    let unsettled = store
        .read_unsettled_group_children(GROUP)
        .await
        .expect("read the unsettled children again");
    assert_eq!(
        unsettled
            .iter()
            .map(|child| child.replay_key.as_str())
            .collect::<Vec<_>>(),
        vec!["k1", "k2"],
        "a committed-but-undrained child holds no rank, so it stays unsettled"
    );
    assert!(
        unsettled[0].commit_state == Some(EffectCommitState::Committed)
            && unsettled[0].commit_seq == Some(1),
        "the committed child leads, carrying its commit position: {:?}/{:?}",
        unsettled[0].commit_state,
        unsettled[0].commit_seq
    );
    assert!(
        matches!(
            unsettled[0].state,
            Some(effect_replay_driver::EffectRowState::Settled(
                EffectTerminal::Completed { .. }
            ))
        ),
        "the committed child's terminal is already on its replay row: {:?}",
        unsettled[0].state
    );

    discharge(&store, "k1").await;
    let unsettled = store
        .read_unsettled_group_children(GROUP)
        .await
        .expect("read the unsettled children after the discharge");
    assert_eq!(
        unsettled
            .iter()
            .map(|child| child.replay_key.as_str())
            .collect::<Vec<_>>(),
        vec!["k2"],
        "a child that took a rank leaves the unsettled set in the same write"
    );

    store
        .finalize(&fences[1], &terminal("k2"))
        .await
        .expect("finalize the second child");
    discharge(&store, "k2").await;
    assert!(
        store
            .read_unsettled_group_children(GROUP)
            .await
            .expect("read the unsettled children a third time")
            .is_empty(),
        "an empty unsettled set is what completeness reads as"
    );
    assert!(
        store
            .read_unsettled_group_children("session:s1/no-such-group")
            .await
            .expect("read an unknown group")
            .is_empty(),
        "an unknown group has no unsettled children rather than an error"
    );
}

/// `open_group` reports the row **as it stands durably**, which is what lets a
/// reopen be fenced against the journal rather than against one process's
/// memory: the second open below is refused by the host because the record it
/// gets back is the first open's, not its own.
#[tokio::test]
async fn reopening_a_group_reports_the_recorded_row_rather_than_the_one_offered() {
    let store = row_store().await;
    let recorded = store
        .open_group(&group_record(), &[])
        .await
        .expect("open the group row");
    assert_eq!(
        recorded,
        group_record(),
        "a fresh open records what it was given"
    );

    let mut shrunk = group_record();
    shrunk.expected_children = 1;
    shrunk.loser_disposition = lash_core::LoserPolicy::Cancel;
    shrunk.created_at_ms = 9_999;
    let reopened = store
        .open_group(&shrunk, &[])
        .await
        .expect("reopening an existing group is idempotent at the store seam");
    assert_eq!(
        reopened,
        group_record(),
        "the recorded row wins: a reopen may not restate a group's children or \
         its declared disposition, and the store is what says so"
    );
}

#[tokio::test]
async fn cold_successor_claim_gets_its_full_lease_after_sqlite_admission() {
    use crate::testing::{SqliteFaultInjector, SqliteFaultPoint};

    let dir = tempfile::tempdir().expect("effect journal directory");
    let path = dir.path().join("effects.db");
    let clock = Arc::new(lash_core::testing::TestClock::new(1_000));
    let scope = ExecutionScope::turn("cold-session", "cold-turn");
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope.clone(), "cold-effect")
                .expect("valid cold effect address"),
            lash_core::RuntimeAttribution::for_turn("cold-session", "cold-turn", 1, 0),
            "cold-effect",
        ),
        lash_core::RuntimeEffectCommand::ExecCode {
            language: "conformance".to_string(),
            code: "external-effect".to_string(),
        },
    );
    let options = SqliteEffectReplayOptions {
        lease_timings: LeaseTimings::from_ttl(std::time::Duration::from_millis(300))
            .expect("lease timings"),
        drain_budget: Default::default(),
    };
    let predecessor = SqliteRuntimeEffectController::open_with_options_and_clock(
        &path,
        scope.clone(),
        options.clone(),
        clock.clone(),
    )
    .await
    .expect("open predecessor");
    let (executed_tx, executed_rx) = tokio::sync::oneshot::channel();
    let predecessor_envelope = envelope.clone();
    let abandoned = tokio::spawn(async move {
        predecessor
            .execute_effect(
                predecessor_envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    executed_tx.send(()).expect("signal external effect");
                    std::future::pending().await
                }),
            )
            .await
    });
    executed_rx.await.expect("predecessor acquired its lease");
    abandoned.abort();
    assert!(
        abandoned
            .await
            .expect_err("abandon without finalizing")
            .is_cancelled()
    );
    clock.advance(300);

    // A fresh driver/connection sees precisely the in-progress row a killed
    // process leaves. Pause admission before the claim reads or writes it.
    let injector = SqliteFaultInjector::default();
    let conn = SqliteConnection::open_with_fault_injector(
        &path,
        crate::conn::SqliteConnectionPolicy::default(),
        Some(injector.clone()),
    )
    .await
    .expect("open successor connection");
    let successor = build_effect_replay_driver(
        conn,
        options,
        clock.clone(),
        vec![0; 32],
        CompletionKeys::Issued,
        std::sync::Arc::new(crate::scope_fence::RegistryAttachment::default()),
        super::settlement_notify::SettlementNotifierKey::for_file(&path),
    );
    let pause = injector.pause(SqliteFaultPoint::AfterBegin);
    let completing = tokio::spawn(async move {
        successor
            .execute_effect(
                &scope,
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    Ok(RuntimeEffectOutcome::ExecCode {
                        result: Box::new(Ok(lash_core::ExecResponse {
                            observations: Vec::new(),
                            calls: Vec::new(),
                            printed_images: Vec::new(),
                            error: None,
                            duration_ms: 0,
                            degraded_bindings: Vec::new(),
                            terminal_finish: Some(serde_json::json!("recorded")),
                        })),
                    })
                }),
                None,
            )
            .await
    });
    pause.wait_until_reached().await;
    clock.advance(300);
    pause.release();
    completing
        .await
        .expect("successor task")
        .expect("successor must finalize after waiting a TTL for SQLite admission");
}

#[tokio::test]
async fn effect_lease_writes_refuse_expiry_during_sqlite_admission() {
    use crate::testing::{SqliteFaultInjector, SqliteFaultPoint};

    #[derive(Clone, Copy, Debug)]
    enum Write {
        Renew,
        Finalize,
        ReleaseDerivation,
    }
    for operation in [Write::Finalize, Write::Renew, Write::ReleaseDerivation] {
        let dir = tempfile::tempdir().expect("effect journal directory");
        let injector = SqliteFaultInjector::default();
        let conn = SqliteConnection::open_with_fault_injector(
            &dir.path().join("effects.db"),
            crate::SqliteConnectionPolicy::default(),
            Some(injector.clone()),
        )
        .await
        .expect("open effect connection");
        ensure_versioned_schema(&conn, SqliteDatabase::EffectReplay)
            .await
            .expect("provision effect schema");
        let clock = Arc::new(lash_core::testing::TestClock::new(1_000));
        let store = Arc::new(SqliteEffectReplayRowStore {
            completion_keys: CompletionKeys::Issued,
            conn,
            clock: clock.clone(),
            registry: Arc::new(crate::scope_fence::RegistryAttachment::default()),
            settlement_key: super::settlement_notify::SettlementNotifierKey::for_file(
                &dir.path().join("effects.db"),
            ),
        });
        let mut request = claim("queued-write", "owner");
        request.lease_ttl_ms = 300;
        request.group_key = None;
        assert!(matches!(
            store.claim(&request).await.expect("claim effect"),
            EffectClaimObservation::Claimed { .. }
        ));
        let pause = injector.pause(SqliteFaultPoint::AfterBegin);
        let writing = tokio::spawn(async move {
            match operation {
                Write::Renew => !store.renew(&fence(&request), 300).await.expect("renew"),
                Write::ReleaseDerivation => !store
                    .release_uncommitted_derivation(&fence(&request))
                    .await
                    .expect("release derivation"),
                Write::Finalize => matches!(
                    store
                        .finalize(&fence(&request), &terminal("queued-write"))
                        .await
                        .expect("finalize"),
                    EffectFinalizeOutcome::FenceMoved
                ),
            }
        });
        pause.wait_until_reached().await;
        clock.advance(300);
        pause.release();
        assert!(
            writing.await.expect("lease write task"),
            "{operation:?} must refuse a lease that expired before the fenced write"
        );
    }
}

// ============================================================================
// The §4 admission fence (ADR 0099)
// ============================================================================
//
// These tests pin the claim-level fence: a *fresh* effect admission minted
// under a cancel-decided group child is refused before a row exists for it,
// while the admitted commands the decision protects — replay rows already
// journaled — claim, take over, and finalize exactly as before.

/// Journals the cancel decision for `replay_key`, insisting it wins, and
/// returns the settlement rank the decision seated the child at.
async fn cancel_child(store: &SqliteEffectReplayRowStore, replay_key: &str) -> u64 {
    let envelope_json = format!(r#"{{"json":"{replay_key}","hash":"hash-{replay_key}"}}"#);
    let outcome = store
        .decide_cancel(&EffectCancelRequest {
            group_key: GROUP.to_string(),
            replay_key: replay_key.to_string(),
            terminal: EffectTerminal::Failed {
                error_json: r#"{"code":"cancelled"}"#.to_string(),
            },
            envelope_json,
            envelope_hash: format!("hash-{replay_key}"),
        })
        .await
        .expect("journal the cancel decision");
    match outcome {
        EffectCancelOutcome::Decided { settlement_seq } => settlement_seq,
        other => panic!("the child must be decided by this call, got {other:?}"),
    }
}

/// A late §4 final commit against a cancel-decided child reports the typed
/// `CancelDecided` outcome carrying the rank the decision seated — the same
/// rank `decide_cancel` returned — never a store error: the schema CHECK
/// forbids `commit_seq` on a `cancel_decided` row, so the rank must come from
/// `settlement_seq`.
#[tokio::test]
async fn a_late_final_commit_on_a_cancel_decided_child_reports_the_seated_rank() {
    let store = row_store().await;
    open_and_claim(&store, &[("k1", "owner-a")]).await;
    let rank = cancel_child(&store, "k1").await;

    let outcome = store
        .commit_group_child(&EffectGroupChildCommitRequest {
            group_key: Some(GROUP.to_string()),
            scope_id: SCOPE.to_string(),
            replay_key: "k1".to_string(),
            drain_input: "{}".to_string(),
            owner_id: "owner-a".to_string(),
        })
        .await
        .expect("a late final on a decided child is an observation, not corruption");
    assert!(
        matches!(
            outcome,
            EffectGroupChildCommitOutcome::CancelDecided { commit_seq, .. }
            if commit_seq == rank
        ),
        "the refusal must carry the seated settlement rank {rank}: {outcome:?}"
    );
}

/// A claim on an ungrouped effect minted by `minting_replay_key`.
fn minted_claim(replay_key: &str, owner: &str, minting_replay_key: &str) -> EffectClaimRequest {
    EffectClaimRequest {
        replay_key: replay_key.to_string(),
        group_key: None,
        minting_effect: Some(MintingEffectRef {
            scope_id: SCOPE.to_string(),
            replay_key: minting_replay_key.to_string(),
        }),
        ..claim(replay_key, owner)
    }
}

/// §4: a new admission beneath a cancel-decided child is refused with no write.
///
/// The fence lives inside the claim transaction, on the `Insert` arm only:
/// what it must produce is the typed observation *and* the absence of the row
/// — a refusal that had inserted first would leave an orphan the next claim
/// could take over.
#[tokio::test]
async fn a_claim_minted_by_a_cancel_decided_child_is_refused_without_a_row() {
    let store = row_store().await;
    store
        .open_group(&group_record(), &membership())
        .await
        .expect("open the group row");
    cancel_child(&store, "k1").await;

    let observation = store
        .claim(&minted_claim("n1", "owner-n", "k1"))
        .await
        .expect("the fence refuses through the observation vocabulary");
    assert_eq!(
        observation,
        EffectClaimObservation::MintingChildCancelled,
        "a fresh admission under a cancelled minting child must be refused"
    );
    assert!(
        !store
            .replay_row_exists(SCOPE, "n1")
            .await
            .expect("read the row"),
        "the refused admission must not leave a replay row behind"
    );
}

/// The fence is discriminating, not broad: only `Cancelled` refuses.
///
/// Every other state the join can answer is admission-permitting — a committed
/// parent's descendants are the protected side of the same decision and an
/// undecided one is still racing. A minting key that resolves to no group
/// child is different: `minting_effect` is minted from a `GroupChildBinding`,
/// so an ungrouped or missing minting row is journal corruption, not a live
/// parent — the claim fails closed rather than admitting under a binding the
/// journal contradicts. `minting_effect: None` covers the common case, where
/// the envelope carries no bound child at all.
#[tokio::test]
async fn the_fence_refuses_only_a_cancelled_minting_child() {
    let store = row_store().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a")]).await;

    // Undecided parent: the child is claimed and running, so a minted
    // admission is ordinary work.
    let observation = store
        .claim(&minted_claim("n1", "owner-n", "k1"))
        .await
        .expect("claim under an undecided parent");
    assert!(
        matches!(observation, EffectClaimObservation::Claimed { .. }),
        "an undecided minting child must not fence admissions: {observation:?}"
    );

    // Committed parent: the decision went the other way, so its declared
    // intents are exactly the work §4 protects.
    store
        .finalize(&fences[0], &terminal("k1"))
        .await
        .expect("finalize the parent");
    discharge(&store, "k1").await;
    let observation = store
        .claim(&minted_claim("n2", "owner-n", "k1"))
        .await
        .expect("claim under a committed parent");
    assert!(
        matches!(observation, EffectClaimObservation::Claimed { .. }),
        "a committed minting child must not fence admissions: {observation:?}"
    );

    // A minting key that resolves to no group child — an ungrouped effect's
    // replay key, or a key no row holds — is corruption the binding cannot
    // have produced, so the claim fails closed rather than admitting.
    let mut ungrouped = claim("sibling", "owner-s");
    ungrouped.group_key = None;
    assert!(matches!(
        store.claim(&ungrouped).await.expect("claim the sibling"),
        EffectClaimObservation::Claimed { .. }
    ));
    let error = store
        .claim(&minted_claim("n3", "owner-n", "sibling"))
        .await
        .expect_err("an ungrouped minting row is corruption, not a parent");
    assert!(
        error.to_string().contains("no group child"),
        "the refusal names the ungrouped minting row: {error}"
    );
    let error = store
        .claim(&minted_claim("n3b", "owner-n", "missing"))
        .await
        .expect_err("a missing minting row is corruption, not a parent");
    assert!(
        error.to_string().contains("does not exist"),
        "the refusal names the missing minting row: {error}"
    );

    // No minting reference at all.
    let mut plain = claim("n4", "owner-n");
    plain.group_key = None;
    assert!(matches!(
        store.claim(&plain).await.expect("claim with no parent"),
        EffectClaimObservation::Claimed { .. }
    ));
}

/// An admitted command outlives its minting child's cancel decision.
///
/// §4 forbids *new* admission under the cancelled invocation; what was already
/// journaled beneath it is the protected half of the same rule. The takeover
/// claim below lands on the row the earlier admission wrote, so the minting
/// check never runs — and that ordering is the law: a replay that consulted
/// the fence would strand every in-flight descendant of a cancelled child.
#[tokio::test]
async fn an_admitted_descendants_row_survives_its_minting_childs_cancel() {
    let store = row_store().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a")]).await;

    // The descendant's lease is minted already expired, so the second claim is
    // a takeover of an existing row — the replay-of-admitted-work path, not a
    // fresh insert.
    let mut admitted = minted_claim("n1", "owner-n", "k1");
    admitted.lease_ttl_ms = 0;
    assert!(matches!(
        store.claim(&admitted).await.expect("admit the descendant"),
        EffectClaimObservation::Claimed { .. }
    ));
    cancel_child(&store, "k1").await;

    let mut takeover = minted_claim("n1", "owner-t", "k1");
    takeover.lease_ttl_ms = 0;
    let observation = store
        .claim(&takeover)
        .await
        .expect("take over the admitted descendant");
    assert!(
        matches!(observation, EffectClaimObservation::Claimed { .. }),
        "a takeover of admitted work is protected, not fenced: {observation:?}"
    );

    // And the cancelled child's own late final is the typed §4 refusal, not a
    // fence miss: the arbitration row, not the lease, answers it.
    let outcome = store
        .finalize(&fences[0], &terminal("k1"))
        .await
        .expect("a late finalize resolves through the journal");
    assert!(
        matches!(outcome, EffectFinalizeOutcome::CancelDecided),
        "a final arriving after the cancel decision is W17's typed refusal: {outcome:?}"
    );
}

/// W19: a committed-but-undrained child — the crash window between the §4
/// decision write and the §5 rank write — is finished by the drain, in commit
/// order, not re-executed.
///
/// The row store stages the window directly: `finalize` writes `Committed`
/// with a commit position, and the `discharge` the driver would have run next
/// is the write the crash took. A drain pass over the same journal owes the
/// rank behind the commit-order barrier, so the child declared second but
/// committed first settles at rank one.
#[tokio::test]
async fn the_drain_finishes_committed_undrained_children_in_commit_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drain.db");
    // The host supplies the drain; a second connection stages the crash state.
    // Both share one file because the window under test is the file surviving
    // the writer that committed and never discharged.
    let host = SqliteEffectHost::open_with_options(&path, SqliteEffectReplayOptions::default())
        .await
        .expect("open the drain host");
    let store = SqliteEffectReplayRowStore {
        completion_keys: CompletionKeys::Unsupported,
        conn: SqliteConnection::open(&path)
            .await
            .expect("open the staging connection"),
        clock: Arc::new(lash_core::facade_support::SystemClock),
        registry: Arc::new(crate::scope_fence::RegistryAttachment::default()),
        settlement_key: super::settlement_notify::SettlementNotifierKey::for_file(&path),
    };
    store
        .open_group(&group_record(), &membership())
        .await
        .expect("open the group row");

    // `k2` (position 1) commits *before* `k1`: the commit order is the durable
    // fact the drain must honor, and it is not the declaration order.
    for replay_key in ["k2", "k1"] {
        let request = claim(replay_key, "owner-1");
        store.claim(&request).await.expect("claim the child");
        let outcome = store
            .finalize(&fence(&request), &terminal(replay_key))
            .await
            .expect("commit the child");
        allocated_commit_seq(outcome);
        // Deliberately no discharge — that write is what the crash took.
    }

    let report = host
        .group_drain()
        .drain_group(GROUP, &tokio_util::sync::CancellationToken::new())
        .await
        .expect("drain the group");

    assert_eq!(report.children.len(), 2);
    for child in &report.children {
        assert_eq!(
            child.outcome,
            ChildDrainOutcome::Decided,
            "a committed child is discharged, never re-executed: {child:?}"
        );
    }
    // Ranks follow the commit order the journal recorded, not positions.
    let first = store
        .read_group_settlement(GROUP, 1)
        .await
        .expect("read rank one")
        .expect("rank one is discharged");
    assert_eq!(first.replay_key, "k2");
    let second = store
        .read_group_settlement(GROUP, 2)
        .await
        .expect("read rank two")
        .expect("rank two is discharged");
    assert_eq!(second.replay_key, "k1");
}

/// FIG-3464: the store-backed replay driver hands every claimed command to
/// the local executor's `execute`, which now runs a trigger command on its
/// trigger target — before, it refused it as a mismatch, so a process-scoped
/// trigger effect on a durable host failed — and a replay answers from the
/// recorded row without touching the trigger store again.
#[tokio::test]
async fn a_trigger_command_runs_on_the_trigger_target_and_replays_from_its_row() {
    let scope = ExecutionScope::process("trigger-driver-process");
    let controller = SqliteRuntimeEffectController::memory(scope.clone())
        .await
        .expect("open the in-memory effect journal");
    let triggers: Arc<dyn lash_core::TriggerStore> = Arc::new(
        crate::SqliteTriggerStore::memory()
            .await
            .expect("open the in-memory trigger store"),
    );
    let owner_scope =
        lash_core::TriggerOwnerScope::host("trigger-driver").expect("trigger owner scope");
    let envelope = || {
        RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(scope.clone(), "trigger-driver:list")
                    .expect("valid trigger address"),
                lash_core::RuntimeAttribution::none(),
                "trigger-driver:list",
            ),
            lash_core::RuntimeEffectCommand::Trigger {
                command: Box::new(lash_core::TriggerCommand::List {
                    owner_scope: owner_scope.clone(),
                    filter: lash_core::TriggerSubscriptionFilter::default(),
                }),
            },
        )
    };

    let recorded = controller
        .execute_effect(envelope(), RuntimeEffectLocalExecutor::triggers(triggers))
        .await
        .expect("the trigger target executes the command");
    assert!(matches!(
        &recorded,
        RuntimeEffectOutcome::Trigger { result }
            if matches!(result.as_ref(), Ok(lash_core::TriggerCommandOutcome::List { records }) if records.is_empty())
    ));

    controller.start_replay();
    let replayed = controller
        .execute_effect(
            envelope(),
            RuntimeEffectLocalExecutor::testing(|_| async {
                panic!("a recorded trigger row must bypass the local executor")
            }),
        )
        .await
        .expect("replay the recorded trigger row");
    assert_eq!(
        serde_json::to_value(replayed).expect("encode the replayed outcome"),
        serde_json::to_value(recorded).expect("encode the recorded outcome")
    );
}
