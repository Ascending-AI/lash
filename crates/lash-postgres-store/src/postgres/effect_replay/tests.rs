//! Store-side contract tests for the durable effect-group journal.
//!
//! These exercise [`PostgresEffectReplayRowStore`] directly rather than
//! through a host: what is under test is the *allocation* discipline — which
//! transaction may move a group's settlement counter, and what a fenced-out
//! finalize is allowed to leave behind — and a host would only obscure which
//! write did what. Each test namespaces its own scope and group key so a shared
//! database needs no truncation between them.

use super::*;
use crate::postgres_test_support;

use lash_core_execution::ChildDrainOutcome;
use lash_core_execution::facade_support::effect_replay_driver::{
    EffectClaimObservation, EffectCommitState, EffectGroupChildCommitOutcome,
    EffectGroupChildCommitRequest, EffectGroupLifecycle, EffectGroupLifecyclePhase,
    MintingEffectRef,
};

#[test]
fn stored_effect_corruption_is_non_retryable() {
    let error = effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectReplay",
            message: "lease_expires_at_ms must be non-negative, got -1".to_string(),
        }
        .to_string(),
    );
    assert_eq!(
        error.code,
        lash_core_execution::RuntimeErrorCode::PostgresEffectReplayStore
    );
    assert!(!error.code.is_retryable());
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

struct GroupFixture {
    /// Held for the test's lifetime: these tests share one database with every
    /// other Postgres suite, and an unlocked fixture would provision or truncate
    /// underneath them.
    _database_lock: postgres_test_support::SharedDatabaseLock,
    storage: PostgresStorage,
    store: PostgresEffectReplayRowStore,
    scope_id: String,
    session_id: SessionId,
    group_key: String,
}

impl GroupFixture {
    async fn open(label: &str) -> Option<Self> {
        let database_url = postgres_test_support::database_url()?;
        let database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect effect-group contract storage");
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let session_id = SessionId::from(format!("effect-group-{label}-{unique}"));
        let scope_id = ExecutionScope::turn(session_id.clone(), format!("effect-group-{label}"))
            .journal_identity()
            .expect("valid effect-group scope")
            .key()
            .to_string();
        let fixture = Self {
            _database_lock: database_lock,
            store: PostgresEffectReplayRowStore {
                pool: storage.pool().clone(),
                notify_hub: group_notify::GroupNotifyHub::spawn(storage.pool()),
            },
            storage,
            scope_id,
            group_key: format!("session:{session_id}/group-1"),
            session_id,
        };
        fixture
            .store
            .open_group(&fixture.record(), &fixture.membership())
            .await
            .expect("open the group row");
        Some(fixture)
    }

    /// The accepted membership the fixture's two children carry.
    ///
    /// `envelope_json` is opaque to the store — the retained accepted envelope
    /// is the raw child request, not the canonical replay form — so the same
    /// bytes the claim request writes stand in for it; what the tests assert
    /// is which row's copy a read reports.
    fn membership(&self) -> Vec<AcceptedGroupChild> {
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

    fn record(&self) -> EffectGroupRecord {
        EffectGroupRecord {
            group_key: self.group_key.clone(),
            scope_id: self.scope_id.clone(),
            session_id: Some(self.session_id.clone()),
            wake: lash_core_execution::GroupWakePolicy::All,
            loser_disposition: lash_core_execution::LoserPolicy::RunToCompletion,
            expected_children: 2,
            created_at_ms: 1_000,
            lifecycle:
                lash_core_execution::runtime::effect_replay_driver::EffectGroupLifecycle::Live,
        }
    }

    fn claim_request(&self, replay_key: &str, owner: &str) -> EffectClaimRequest {
        EffectClaimRequest {
            scope_id: self.scope_id.clone(),
            session_id: Some(self.session_id.clone()),
            replay_key: replay_key.to_string(),
            envelope_hash: format!("hash-{replay_key}"),
            envelope_json: format!(r#"{{"json":"{replay_key}","hash":"hash-{replay_key}"}}"#),
            owner_id: owner.to_string(),
            lease_token: format!("token-{owner}"),
            lease_ttl_ms: 30_000,
            sleep: None,
            group_key: Some(self.group_key.clone()),
            minting_effect: None,
            strict_replay: false,
        }
    }

    async fn claim(&self, replay_key: &str, owner: &str) -> EffectLeaseFence {
        let request = self.claim_request(replay_key, owner);
        let observation = self.store.claim(&request).await.expect("claim the child");
        assert!(
            matches!(observation, EffectClaimObservation::Claimed { .. }),
            "the child must be freshly claimed"
        );
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

    async fn next_seq(&self) -> i64 {
        sqlx::query_scalar("SELECT next_seq FROM lash_runtime_effect_group WHERE group_key = $1")
            .bind(&self.group_key)
            .fetch_one(&self.store.pool)
            .await
            .expect("read the group counter")
    }

    async fn next_commit_seq(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT next_commit_seq FROM lash_runtime_effect_group WHERE group_key = $1",
        )
        .bind(&self.group_key)
        .fetch_one(&self.store.pool)
        .await
        .expect("read the commit counter")
    }

    /// Discharge one committed child, insisting the barrier admits it.
    async fn discharge(&self, replay_key: &str) -> u64 {
        match self
            .store
            .discharge_child(&EffectDischargeRequest {
                group_key: self.group_key.clone(),
                scope_id: self.scope_id.clone(),
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
}

/// Two siblings finalizing at the same time must take *different* commit
/// positions, and only their discharges allocate settlement ranks.
///
/// This is the anchor for the group row carrying both counters: an allocator
/// that reads `MAX(commit_seq) + 1` and writes the result is a read-then-write
/// that no fence covers, so under `READ COMMITTED` two concurrent finalizes
/// both observe the same maximum and both write the same commit position — one
/// child's final silently loses its place in the commit order a drain replays.
/// Each counter lives on a single row so the bump takes that row's write lock
/// and the second writer blocks until the first commits.
///
/// Two axes, with different strengths, stated rather than implied. The
/// counter-equals-commits assertion is deterministic: a `MAX`-based allocator
/// never moves the counter, so it fails on every run. The distinct-positions
/// assertion is a race guard — whether the two finalizes actually overlap
/// inside one `READ COMMITTED` window depends on scheduling — so it is a
/// probabilistic net over a defect the deterministic assertion already
/// catches, backed by the partial `UNIQUE (commit_seq, group_key)` failing the
/// write closed. The `next_seq` assertion is the §5 law: finalize moves no
/// settlement rank, so a consumer cannot observe a child as settled before
/// its drain discharged. This is the concurrent half of the pair; the SQLite
/// sibling serializes on one writer and can only hold the deterministic axis.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_finalize_allocates_distinct_commit_positions() {
    let Some(fixture) = GroupFixture::open("concurrent").await else {
        eprintln!("skipping effect-group concurrent finalize: database URL is not set");
        return;
    };
    let first = fixture.claim("k1", "owner-a").await;
    let second = fixture.claim("k2", "owner-b").await;

    let (first_terminal, second_terminal) =
        (GroupFixture::terminal("k1"), GroupFixture::terminal("k2"));
    let (left, right) = tokio::join!(
        fixture.store.finalize(&first, &first_terminal),
        fixture.store.finalize(&second, &second_terminal),
    );
    let mut children = vec![
        (
            "k1",
            allocated_commit_seq(left.expect("finalize the first child")),
        ),
        (
            "k2",
            allocated_commit_seq(right.expect("finalize the second child")),
        ),
    ];
    children.sort_by_key(|(_, commit_seq)| *commit_seq);
    assert_eq!(
        children
            .iter()
            .map(|(_, commit_seq)| *commit_seq)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "each committing child must take its own durable commit position"
    );
    assert_eq!(
        fixture.next_commit_seq().await,
        2,
        "the commit counter counts finals"
    );
    assert_eq!(
        fixture.next_seq().await,
        0,
        "finalize allocates no settlement rank — the discharge does"
    );

    // The barrier holds a later commit behind an undrained earlier one.
    let (later_key, _) = children[1];
    let outcome = fixture
        .store
        .discharge_child(&EffectDischargeRequest {
            group_key: fixture.group_key.clone(),
            scope_id: fixture.scope_id.clone(),
            replay_key: later_key.to_string(),
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
    let mut ranks = Vec::new();
    for (replay_key, _) in &children {
        ranks.push(fixture.discharge(replay_key).await);
    }
    assert_eq!(
        ranks,
        vec![1, 2],
        "settlement order agrees with commit order"
    );
    assert_eq!(
        fixture.next_seq().await,
        2,
        "the counter counts settlements"
    );
}

/// A boundary-committed row is `committed` but still `in_progress`: its drain
/// is owed. A terminal-less discharge against it must report `Blocked` and
/// roll its rank bump back — never seat a rank on a child whose projected
/// terminal does not exist yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_child_still_draining_cannot_take_a_rank_without_its_terminal() {
    let Some(fixture) = GroupFixture::open("mid-drain").await else {
        eprintln!("skipping mid-drain discharge anchor: database URL is not set");
        return;
    };
    fixture.claim("k1", "owner-a").await;

    let outcome = fixture
        .store
        .commit_group_child(&EffectGroupChildCommitRequest {
            group_key: Some(fixture.group_key.clone()),
            scope_id: fixture.scope_id.clone(),
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

    let outcome = fixture
        .store
        .discharge_child(&EffectDischargeRequest {
            group_key: fixture.group_key.clone(),
            scope_id: fixture.scope_id.clone(),
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

    let (commit_state, settlement_seq): (String, Option<i64>) = sqlx::query_as(
        "SELECT commit_state, settlement_seq FROM lash_runtime_effect_replay
         WHERE scope_id = $1 AND replay_key = 'k1'",
    )
    .bind(&fixture.scope_id)
    .fetch_one(&fixture.store.pool)
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
        fixture.next_seq().await,
        0,
        "the rank bump rolled back with the blocked discharge"
    );

    // The finishing write carries the projected terminal and discharges.
    let outcome = fixture
        .store
        .discharge_child(&EffectDischargeRequest {
            group_key: fixture.group_key.clone(),
            scope_id: fixture.scope_id.clone(),
            replay_key: "k1".to_string(),
            terminal: Some(GroupFixture::terminal("k1")),
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
/// This is why the fenced `UPDATE` runs first, returns the child's own
/// `group_key`, and the commit counter moves only when it returned a row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fence_miss_allocates_nothing() {
    let Some(fixture) = GroupFixture::open("fence-miss").await else {
        eprintln!("skipping effect-group fence-miss anchor: database URL is not set");
        return;
    };
    let fence = fixture.claim("k1", "owner-a").await;
    let stale = EffectLeaseFence {
        lease_token: "token-superseded".to_string(),
        ..fence
    };

    let outcome = fixture
        .store
        .finalize(&stale, &GroupFixture::terminal("k1"))
        .await
        .expect("a fenced-out finalize is not an error");
    assert!(
        matches!(outcome, EffectFinalizeOutcome::FenceMoved),
        "a stale lease token must report the fence moved"
    );
    assert_eq!(
        fixture.next_commit_seq().await,
        0,
        "a finalize that wrote no child row must not consume a commit position"
    );
    assert_eq!(
        fixture.next_seq().await,
        0,
        "a finalize that wrote no child row must not consume a settlement rank"
    );
    assert!(
        fixture
            .store
            .read_group_settlement(&fixture.group_key, 1)
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_commit_seq_check_holds_grouped_rows_to_their_position() {
    let Some(fixture) = GroupFixture::open("commit-check").await else {
        eprintln!("skipping commit-seq CHECK anchor: database URL is not set");
        return;
    };
    fixture.claim("k1", "owner-a").await;

    for commit_state in ["committed", "drained"] {
        let error = sqlx::query(
            "UPDATE lash_runtime_effect_replay
             SET commit_state = $2, commit_seq = NULL
             WHERE scope_id = $1 AND replay_key = 'k1'",
        )
        .bind(&fixture.scope_id)
        .bind(commit_state)
        .execute(&fixture.store.pool)
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
        ..fixture.claim_request("u1", "owner-u")
    };
    let observation = fixture
        .store
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
        sqlx::query(&format!(
            "UPDATE lash_runtime_effect_replay
             SET commit_state = $2, commit_seq = NULL{settlement}
             WHERE scope_id = $1 AND replay_key = 'u1'"
        ))
        .bind(&fixture.scope_id)
        .bind(commit_state)
        .execute(&fixture.store.pool)
        .await
        .unwrap_or_else(|error| {
            panic!("an ungrouped {commit_state} row may carry no commit position: {error}")
        });
    }
}

/// `drain_input` is a committed-row fact: it is unwritable on a pending or
/// ungrouped row, where nothing could legitimately have staged a resumable
/// drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_drain_input_check_confines_it_to_decided_rows() {
    let Some(fixture) = GroupFixture::open("drain-input-check").await else {
        eprintln!("skipping drain-input CHECK anchor: database URL is not set");
        return;
    };
    fixture.claim("k1", "owner-a").await;

    let error = sqlx::query(
        "UPDATE lash_runtime_effect_replay SET drain_input = '{\"x\":1}'
         WHERE scope_id = $1 AND replay_key = 'k1'",
    )
    .bind(&fixture.scope_id)
    .execute(&fixture.store.pool)
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
        ..fixture.claim_request("u1", "owner-u")
    };
    fixture
        .store
        .claim(&ungrouped)
        .await
        .expect("claim the ungrouped row");
    let error = sqlx::query(
        "UPDATE lash_runtime_effect_replay SET drain_input = '{\"x\":1}'
         WHERE scope_id = $1 AND replay_key = 'u1'",
    )
    .bind(&fixture.scope_id)
    .execute(&fixture.store.pool)
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settlements_are_read_by_rank_not_by_sequence_value() {
    let Some(fixture) = GroupFixture::open("rank").await else {
        eprintln!("skipping effect-group rank stability: database URL is not set");
        return;
    };
    for (replay_key, owner) in [("k1", "owner-a"), ("k2", "owner-b")] {
        let fence = fixture.claim(replay_key, owner).await;
        fixture
            .store
            .finalize(&fence, &GroupFixture::terminal(replay_key))
            .await
            .expect("finalize the child");
        fixture.discharge(replay_key).await;
    }
    // Open a gap the way a retried allocation would: the ranks a consumer reads
    // must not move.
    sqlx::query(
        "UPDATE lash_runtime_effect_replay SET settlement_seq = 9
         WHERE scope_id = $1 AND replay_key = 'k2'",
    )
    .bind(&fixture.scope_id)
    .execute(&fixture.store.pool)
    .await
    .expect("widen the settlement gap");

    let first = fixture
        .store
        .read_group_settlement(&fixture.group_key, 1)
        .await
        .expect("read rank 1")
        .expect("rank 1 is settled");
    let second = fixture
        .store
        .read_group_settlement(&fixture.group_key, 2)
        .await
        .expect("read rank 2")
        .expect("rank 2 is settled");
    assert_eq!(first.replay_key, "k1");
    assert_eq!(second.replay_key, "k2");
    assert_eq!(second.sequence, 9, "the gap is real, the rank is unchanged");
    assert!(
        fixture
            .store
            .read_group_settlement(&fixture.group_key, 3)
            .await
            .expect("read rank 3")
            .is_none(),
        "a rank past the settled count must read as absent, never as a wrapped rank"
    );
}

/// Retirement takes a group and its children together: a group row outliving its
/// children keeps a counter no settlement can be matched to, and children
/// outliving their group settle against a missing allocator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retirement_removes_a_group_and_its_children_together() {
    let Some(fixture) = GroupFixture::open("retire").await else {
        eprintln!("skipping effect-group retirement atomicity: database URL is not set");
        return;
    };
    let fence = fixture.claim("k1", "owner-a").await;
    fixture.claim("k2", "owner-b").await;
    fixture
        .store
        .finalize(&fence, &GroupFixture::terminal("k1"))
        .await
        .expect("finalize the first child");
    fixture.discharge("k1").await;

    // Session retirement refuses while the group is live (ADR 0099 §7);
    // settle the lifecycle first so the delete is the thing under test.
    fixture
        .store
        .transition_group_lifecycle(
            &fixture.group_key,
            &[EffectGroupLifecyclePhase::Live],
            &EffectGroupLifecycle::Settled {
                disposition: lash_core_execution::LoserPolicy::RunToCompletion,
            },
        )
        .await
        .expect("settle the group lifecycle");

    let removed = fixture
        .store
        .retire_journal(&lash_core_execution::EffectJournalRetirement::Session {
            session_id: fixture.session_id.clone(),
        })
        .await
        .expect("retire the session journal");
    assert_eq!(removed, 2, "retirement reports the children it removed");

    let groups: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lash_runtime_effect_group WHERE group_key = $1")
            .bind(&fixture.group_key)
            .fetch_one(&fixture.store.pool)
            .await
            .expect("count group rows");
    assert_eq!(groups, 0, "the group row goes with its children");
    let pending = fixture
        .store
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
    fixture
        .store
        .complete_artifact_owner_retirement(identity.key())
        .await
        .expect("ack artifact owner cleanup");
    assert!(
        fixture
            .store
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsettled_children_are_exactly_the_children_without_a_rank() {
    let Some(fixture) = GroupFixture::open("unsettled").await else {
        eprintln!("skipping effect-group unsettled read: database URL is not set");
        return;
    };

    // Before any claim the whole membership is already the unsettled set: an
    // accepted child with no replay row is work the drain must still see.
    let unsettled = fixture
        .store
        .read_unsettled_group_children(&fixture.group_key)
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
    assert_eq!(unsettled[0].scope_id, fixture.scope_id);
    assert_eq!(
        unsettled[0].state, None,
        "a never-claimed child has no replay row state"
    );
    assert_eq!(unsettled[0].commit_state, None);
    assert_eq!(
        unsettled[0].envelope_json, r#"{"json":"k1","hash":"hash-k1"}"#,
        "the row carries the retained envelope a drain re-executes from"
    );

    let first_fence = fixture.claim("k1", "owner-a").await;
    let second_fence = fixture.claim("k2", "owner-b").await;

    let unsettled = fixture
        .store
        .read_unsettled_group_children(&fixture.group_key)
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
    fixture
        .store
        .finalize(&first_fence, &GroupFixture::terminal("k1"))
        .await
        .expect("finalize the first child");
    let unsettled = fixture
        .store
        .read_unsettled_group_children(&fixture.group_key)
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

    fixture.discharge("k1").await;
    let unsettled = fixture
        .store
        .read_unsettled_group_children(&fixture.group_key)
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

    fixture
        .store
        .finalize(&second_fence, &GroupFixture::terminal("k2"))
        .await
        .expect("finalize the second child");
    fixture.discharge("k2").await;
    assert!(
        fixture
            .store
            .read_unsettled_group_children(&fixture.group_key)
            .await
            .expect("read the unsettled children a third time")
            .is_empty(),
        "an empty unsettled set is what completeness reads as"
    );
}

/// `open_group` reports the row **as it stands durably**, which is what lets a
/// reopen be fenced against the journal rather than against one process's
/// memory: the reopen below gets the first open's record back, not its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopening_a_group_reports_the_recorded_row_rather_than_the_one_offered() {
    let Some(fixture) = GroupFixture::open("reopen").await else {
        eprintln!("skipping effect-group reopen record: database URL is not set");
        return;
    };
    // The fixture already opened the group once, with the record below.
    let recorded = fixture.record();

    let mut shrunk = recorded.clone();
    shrunk.expected_children = 1;
    shrunk.loser_disposition = lash_core_execution::LoserPolicy::Cancel;
    shrunk.created_at_ms = 9_999;
    let reopened = fixture
        .store
        .open_group(&shrunk, &[])
        .await
        .expect("reopening an existing group is idempotent at the store seam");
    assert_eq!(
        reopened, recorded,
        "the recorded row wins: a reopen may not restate a group's children or \
         its declared disposition, and the store is what says so"
    );
}

// ============================================================================
// The §4 admission fence (ADR 0099)
// ============================================================================
//
// These tests pin the claim-level fence: a *fresh* effect admission minted
// under a cancel-decided group child is refused before a row exists for it,
// while the admitted commands the decision protects — replay rows already
// journaled — claim, take over, and finalize exactly as before. On this
// backend the fence read runs `FOR NO KEY UPDATE` inside the claim
// transaction, so it serializes against `decide_cancel`'s membership write.

impl GroupFixture {
    /// Journals the cancel decision for `replay_key`, insisting it wins, and
    /// returns the settlement rank the decision seated the child at.
    async fn cancel_child(&self, replay_key: &str) -> u64 {
        let outcome = self
            .store
            .decide_cancel(&EffectCancelRequest {
                group_key: self.group_key.clone(),
                replay_key: replay_key.to_string(),
                terminal: EffectTerminal::Failed {
                    error_json: r#"{"code":"cancelled"}"#.to_string(),
                },
                envelope_json: format!(r#"{{"json":"{replay_key}","hash":"hash-{replay_key}"}}"#),
                envelope_hash: format!("hash-{replay_key}"),
                completion_fence: None,
            })
            .await
            .expect("journal the cancel decision");
        match outcome {
            EffectCancelOutcome::Decided { settlement_seq } => settlement_seq,
            other => panic!("the child must be decided by this call, got {other:?}"),
        }
    }

    /// A claim on an ungrouped effect minted by `minting_replay_key`.
    fn minted_claim(
        &self,
        replay_key: &str,
        owner: &str,
        minting_replay_key: &str,
    ) -> EffectClaimRequest {
        EffectClaimRequest {
            replay_key: replay_key.to_string(),
            group_key: None,
            minting_effect: Some(MintingEffectRef {
                scope_id: self.scope_id.clone(),
                replay_key: minting_replay_key.to_string(),
            }),
            ..self.claim_request(replay_key, owner)
        }
    }
}

/// A late §4 final commit against a cancel-decided child reports the typed
/// `CancelDecided` outcome carrying the rank the decision seated — the same
/// rank `decide_cancel` returned — never a store error: the schema CHECK
/// forbids `commit_seq` on a `cancel_decided` row, so the rank must come from
/// `settlement_seq`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_final_commit_on_a_cancel_decided_child_reports_the_seated_rank() {
    let Some(fixture) = GroupFixture::open("late-final-rank").await else {
        eprintln!("skipping late-final seated-rank report: database URL is not set");
        return;
    };
    fixture.claim("k1", "owner-a").await;
    let rank = fixture.cancel_child("k1").await;

    let outcome = fixture
        .store
        .commit_group_child(&EffectGroupChildCommitRequest {
            group_key: Some(fixture.group_key.clone()),
            scope_id: fixture.scope_id.clone(),
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

/// §4: a new admission beneath a cancel-decided child is refused with no write.
///
/// The fence lives inside the claim transaction, on the `Insert` arm only:
/// what it must produce is the typed observation *and* the absence of the row
/// — a refusal that had inserted first would leave an orphan the next claim
/// could take over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_minted_by_a_cancel_decided_child_is_refused_without_a_row() {
    let Some(fixture) = GroupFixture::open("mint-fence").await else {
        eprintln!("skipping effect-group minting fence: database URL is not set");
        return;
    };
    fixture.cancel_child("k1").await;

    let observation = fixture
        .store
        .claim(&fixture.minted_claim("n1", "owner-n", "k1"))
        .await
        .expect("the fence refuses through the observation vocabulary");
    assert_eq!(
        observation,
        EffectClaimObservation::MintingChildCancelled,
        "a fresh admission under a cancelled minting child must be refused"
    );
    assert!(
        !fixture
            .store
            .replay_row_exists(&fixture.scope_id, "n1")
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fence_refuses_only_a_cancelled_minting_child() {
    let Some(fixture) = GroupFixture::open("mint-fence-arms").await else {
        eprintln!("skipping effect-group minting fence arms: database URL is not set");
        return;
    };
    let parent = fixture.claim("k1", "owner-a").await;

    // Undecided parent: the child is claimed and running, so a minted
    // admission is ordinary work.
    let observation = fixture
        .store
        .claim(&fixture.minted_claim("n1", "owner-n", "k1"))
        .await
        .expect("claim under an undecided parent");
    assert!(
        matches!(observation, EffectClaimObservation::Claimed { .. }),
        "an undecided minting child must not fence admissions: {observation:?}"
    );

    // Committed parent: the decision went the other way, so its declared
    // intents are exactly the work §4 protects.
    fixture
        .store
        .finalize(&parent, &GroupFixture::terminal("k1"))
        .await
        .expect("finalize the parent");
    fixture.discharge("k1").await;
    let observation = fixture
        .store
        .claim(&fixture.minted_claim("n2", "owner-n", "k1"))
        .await
        .expect("claim under a committed parent");
    assert!(
        matches!(observation, EffectClaimObservation::Claimed { .. }),
        "a committed minting child must not fence admissions: {observation:?}"
    );

    // A minting key that resolves to no group child — an ungrouped effect's
    // replay key, or a key no row holds — is corruption the binding cannot
    // have produced, so the claim fails closed rather than admitting.
    let mut ungrouped = fixture.claim_request("sibling", "owner-s");
    ungrouped.group_key = None;
    assert!(matches!(
        fixture
            .store
            .claim(&ungrouped)
            .await
            .expect("claim the sibling"),
        EffectClaimObservation::Claimed { .. }
    ));
    let error = fixture
        .store
        .claim(&fixture.minted_claim("n3", "owner-n", "sibling"))
        .await
        .expect_err("an ungrouped minting row is corruption, not a parent");
    assert!(
        error.to_string().contains("no group child"),
        "the refusal names the ungrouped minting row: {error}"
    );
    let error = fixture
        .store
        .claim(&fixture.minted_claim("n3b", "owner-n", "missing"))
        .await
        .expect_err("a missing minting row is corruption, not a parent");
    assert!(
        error.to_string().contains("does not exist"),
        "the refusal names the missing minting row: {error}"
    );

    // No minting reference at all.
    let mut plain = fixture.claim_request("n4", "owner-n");
    plain.group_key = None;
    assert!(matches!(
        fixture
            .store
            .claim(&plain)
            .await
            .expect("claim with no parent"),
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admitted_descendants_row_survives_its_minting_childs_cancel() {
    let Some(fixture) = GroupFixture::open("mint-fence-replay").await else {
        eprintln!("skipping effect-group minting replay fence: database URL is not set");
        return;
    };
    let parent = fixture.claim("k1", "owner-a").await;

    // The descendant's lease is minted already expired, so the second claim is
    // a takeover of an existing row — the replay-of-admitted-work path, not a
    // fresh insert.
    let mut admitted = fixture.minted_claim("n1", "owner-n", "k1");
    admitted.lease_ttl_ms = 0;
    assert!(matches!(
        fixture
            .store
            .claim(&admitted)
            .await
            .expect("admit the descendant"),
        EffectClaimObservation::Claimed { .. }
    ));
    fixture.cancel_child("k1").await;

    let mut takeover = fixture.minted_claim("n1", "owner-t", "k1");
    takeover.lease_ttl_ms = 0;
    let observation = fixture
        .store
        .claim(&takeover)
        .await
        .expect("take over the admitted descendant");
    assert!(
        matches!(observation, EffectClaimObservation::Claimed { .. }),
        "a takeover of admitted work is protected, not fenced: {observation:?}"
    );

    // And the cancelled child's own late final is the typed §4 refusal, not a
    // fence miss: the arbitration row, not the lease, answers it.
    let outcome = fixture
        .store
        .finalize(&parent, &GroupFixture::terminal("k1"))
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
    let Some(fixture) = GroupFixture::open("committed-undrained-drain").await else {
        return;
    };

    // `k2` (position 1) commits *before* `k1`: the commit order is the durable
    // fact the drain must honor, and it is not the declaration order.
    for replay_key in ["k2", "k1"] {
        let fence = fixture.claim(replay_key, "owner-1").await;
        let outcome = fixture
            .store
            .finalize(&fence, &GroupFixture::terminal(replay_key))
            .await
            .expect("commit the child");
        allocated_commit_seq(outcome);
        // Deliberately no discharge — that write is what the crash took.
    }

    let drain = std::sync::Arc::new(build_effect_replay_driver(
        &fixture.storage,
        PostgresEffectReplayOptions::default(),
        std::sync::Arc::new(lash_core_execution::facade_support::SystemClock),
    ))
    .into_group_drain();
    let report = drain
        .drain_group(
            &fixture.group_key,
            &tokio_util::sync::CancellationToken::new(),
        )
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
    let first = fixture
        .store
        .read_group_settlement(&fixture.group_key, 1)
        .await
        .expect("read rank one")
        .expect("rank one is discharged");
    assert_eq!(first.replay_key, "k2");
    let second = fixture
        .store
        .read_group_settlement(&fixture.group_key, 2)
        .await
        .expect("read rank two")
        .expect("rank two is discharged");
    assert_eq!(second.replay_key, "k1");
}
