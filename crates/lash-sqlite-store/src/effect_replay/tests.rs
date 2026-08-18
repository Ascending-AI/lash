//! Store-side contract tests for the SQLite effect journal.
//!
//! The group tests exercise [`SqliteEffectReplayPersistence`] directly rather than through
//! a host: what is under test is the *allocation* discipline — which transaction
//! may move a group's settlement counter, and what a fenced-out finalize is
//! allowed to leave behind — and a host would only obscure which write did what.

use super::*;

use lash_core::facade_support::effect_replay_driver::{
    EffectClaimObservation, EffectFinalizeOutcome, EffectGroupRecord, EffectLeaseFence,
    EffectTerminal,
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

const SCOPE: &str = "session:s1";
const GROUP: &str = "session:s1/group-1";

async fn persistence() -> SqliteEffectReplayPersistence {
    let conn = SqliteConnection::open_in_memory()
        .await
        .expect("open the in-memory effect database");
    ensure_effect_schema(&conn)
        .await
        .expect("provision the effect schema");
    SqliteEffectReplayPersistence {
        conn,
        clock: Arc::new(lash_core::facade_support::SystemClock),
    }
}

fn group_record() -> EffectGroupRecord {
    EffectGroupRecord {
        group_key: GROUP.to_string(),
        scope_id: SCOPE.to_string(),
        session_id: Some("s1".to_string()),
        wake: lash_core::GroupWakePolicy::All,
        loser_disposition: lash_core::LoserDisposition::RunToCompletion,
        children: 2,
        created_at_ms: 1_000,
    }
}

fn claim(replay_key: &str, owner: &str) -> EffectClaimRequest {
    EffectClaimRequest {
        scope_id: SCOPE.to_string(),
        session_id: Some("s1".to_string()),
        replay_key: replay_key.to_string(),
        envelope_hash: format!("hash-{replay_key}"),
        envelope_json: format!(r#"{{"json":"{replay_key}","hash":"hash-{replay_key}"}}"#),
        owner_id: owner.to_string(),
        lease_token: format!("token-{owner}"),
        lease_ttl_ms: 30_000,
        sleep_duration_ms: None,
        group_key: Some(GROUP.to_string()),
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

async fn open_and_claim(
    store: &SqliteEffectReplayPersistence,
    keys: &[(&str, &str)],
) -> Vec<EffectLeaseFence> {
    store
        .open_group(&group_record())
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

/// The rank a grouped child's finalize took, insisting it took one.
fn allocated_rank(outcome: EffectFinalizeOutcome) -> u64 {
    match outcome {
        EffectFinalizeOutcome::Written {
            settlement_seq: Some(sequence),
        } => sequence,
        other => panic!("a grouped child's finalize must allocate a rank, got {other:?}"),
    }
}

async fn next_seq(store: &SqliteEffectReplayPersistence) -> i64 {
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

/// Two siblings finalizing must take *different* settlement sequences.
///
/// This is the anchor for the group row existing at all: an allocator that reads
/// `MAX(settlement_seq) + 1` and writes the result is a read-then-write that no
/// fence covers, so two finalizes that interleave both observe the same maximum
/// and both write the same rank — one settlement silently overwrites the other's
/// position in the queue a consumer reads by rank. The counter lives on a single
/// row precisely so the bump is the transaction's own write.
///
/// **On this backend the race is not reachable, and that is stated rather than
/// implied.** SQLite writes through one `BEGIN IMMEDIATE` connection, so two
/// finalizes serialize whatever the caller does; a read-then-write allocator
/// would still hand out distinct ranks here. What this test actually holds is
/// the *invariant* that survives serialization: the group counter equals the
/// number of settlements, which a `MAX`-based allocator fails immediately
/// because it never moves the counter at all. The genuinely concurrent version
/// of this proof is the Postgres sibling, where separate connections can
/// interleave inside one `READ COMMITTED` window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_finalize_allocates_distinct_settlement_sequences() {
    let store = persistence().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;

    let (first, second) = (terminal("k1"), terminal("k2"));
    let (left, right) = tokio::join!(
        store.finalize(&fences[0], &first),
        store.finalize(&fences[1], &second),
    );
    let mut sequences = vec![
        allocated_rank(left.expect("finalize the first child")),
        allocated_rank(right.expect("finalize the second child")),
    ];
    sequences.sort_unstable();
    assert_eq!(
        sequences,
        vec![1, 2],
        "each settling child must take its own rank"
    );
    assert_eq!(next_seq(&store).await, 2, "the counter counts settlements");
}

/// A finalize that loses the lease fence must change nothing at all — including
/// the group's counter.
///
/// Bumping first and writing the child second would burn a rank on a write that
/// never lands: the group would count a settlement no row carries, and a consumer
/// waiting on that rank would wait forever. This is why the fenced `UPDATE` runs
/// first and the counter moves only on rowcount 1.
#[tokio::test]
async fn a_fence_miss_allocates_nothing() {
    let store = persistence().await;
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

/// Ranks are read by position, not by sequence equality: the (n+1)-th smallest
/// sequence is the (n+1)-th settlement even when the numbers are not contiguous.
#[tokio::test]
async fn settlements_are_read_by_rank_not_by_sequence_value() {
    let store = persistence().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;
    for fence in &fences {
        store
            .finalize(fence, &terminal(&fence.replay_key))
            .await
            .expect("finalize the child");
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
    let store = persistence().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;
    store
        .finalize(&fences[0], &terminal("k1"))
        .await
        .expect("finalize the first child");

    let removed = store
        .retire_journal(&lash_core::EffectJournalRetirement::Session {
            session_id: "s1".to_string(),
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
}

/// The unsettled read is the exact complement of the rank read: every child of
/// the group is in one answer or the other, never both and never neither.
///
/// It is what makes "this group is complete" a single question instead of a walk
/// up the ranks, and it is the drain queue FIG-1536 reads, which is why the row
/// carries the child's journal identity, its recorded envelope, and its lease
/// boundary rather than only its key.
#[tokio::test]
async fn unsettled_children_are_exactly_the_children_without_a_rank() {
    let store = persistence().await;
    let fences = open_and_claim(&store, &[("k1", "owner-a"), ("k2", "owner-b")]).await;

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
    assert_eq!(unsettled[0].scope_id, SCOPE);
    assert_eq!(unsettled[0].status, "in_progress");
    assert_eq!(
        unsettled[0].envelope_json, r#"{"json":"k1","hash":"hash-k1"}"#,
        "the row carries the recorded canonical envelope a drain re-executes from"
    );
    assert!(
        unsettled[0].lease_expires_at_ms > 0,
        "the row carries the lease boundary a drain decides takeover against"
    );

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
        vec!["k2"],
        "a child that took a rank leaves the unsettled set in the same write"
    );

    store
        .finalize(&fences[1], &terminal("k2"))
        .await
        .expect("finalize the second child");
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
    let store = persistence().await;
    let recorded = store
        .open_group(&group_record())
        .await
        .expect("open the group row");
    assert_eq!(
        recorded,
        group_record(),
        "a fresh open records what it was given"
    );

    let mut shrunk = group_record();
    shrunk.children = 1;
    shrunk.loser_disposition = lash_core::LoserDisposition::Cancel;
    shrunk.created_at_ms = 9_999;
    let reopened = store
        .open_group(&shrunk)
        .await
        .expect("reopening an existing group is idempotent at the store seam");
    assert_eq!(
        reopened,
        group_record(),
        "the recorded row wins: a reopen may not restate a group's children or \
         its declared disposition, and the store is what says so"
    );
}
