//! Store-side contract tests for the durable effect-group journal.
//!
//! These exercise [`PostgresEffectReplayPersistence`] directly rather than
//! through a host: what is under test is the *allocation* discipline — which
//! transaction may move a group's settlement counter, and what a fenced-out
//! finalize is allowed to leave behind — and a host would only obscure which
//! write did what. Each test namespaces its own scope and group key so a shared
//! database needs no truncation between them.

use super::*;
use crate::postgres_test_support;

use lash_core::facade_support::effect_replay_driver::EffectClaimObservation;

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
        lash_core::RuntimeErrorCode::PostgresEffectReplayStore
    );
    assert!(!error.code.is_retryable());
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

struct GroupFixture {
    /// Held for the test's lifetime: these tests share one database with every
    /// other Postgres suite, and an unlocked fixture would provision or truncate
    /// underneath them.
    _database_lock: postgres_test_support::SharedDatabaseLock,
    store: PostgresEffectReplayPersistence,
    scope_id: String,
    session_id: String,
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
        let session_id = format!("effect-group-{label}-{unique}");
        let fixture = Self {
            _database_lock: database_lock,
            store: PostgresEffectReplayPersistence {
                pool: storage.pool().clone(),
            },
            scope_id: format!("session:{session_id}"),
            group_key: format!("session:{session_id}/group-1"),
            session_id,
        };
        fixture
            .store
            .open_group(&fixture.record())
            .await
            .expect("open the group row");
        Some(fixture)
    }

    fn record(&self) -> EffectGroupRecord {
        EffectGroupRecord {
            group_key: self.group_key.clone(),
            scope_id: self.scope_id.clone(),
            session_id: Some(self.session_id.clone()),
            wake: lash_core::GroupWakePolicy::All,
            loser_disposition: lash_core::LoserPolicy::RunToCompletion,
            children: 2,
            created_at_ms: 1_000,
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
            sleep_duration_ms: None,
            group_key: Some(self.group_key.clone()),
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
}

/// Two siblings finalizing at the same time must take *different* settlement
/// sequences.
///
/// This is the anchor for the group row existing at all: an allocator that reads
/// `MAX(settlement_seq) + 1` and writes the result is a read-then-write that no
/// fence covers, so under `READ COMMITTED` two concurrent finalizes both observe
/// the same maximum and both write the same rank — one settlement silently
/// overwrites the other's position in the queue a consumer reads by rank. The
/// counter lives on a single row so the bump takes that row's write lock and the
/// second finalize blocks until the first commits.
///
/// Two axes, with different strengths, stated rather than implied. The
/// counter-equals-settlements assertion is deterministic: a `MAX`-based
/// allocator never moves the counter, so it fails on every run. The
/// distinct-ranks assertion is a race guard — whether the two finalizes actually
/// overlap inside one `READ COMMITTED` window depends on scheduling — so it is a
/// probabilistic net over a defect the deterministic assertion already catches,
/// backed by `UNIQUE (group_key, settlement_seq)` failing the write closed. This
/// is the concurrent half of the pair; the SQLite sibling serializes on one
/// writer and can only hold the deterministic axis.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_finalize_allocates_distinct_settlement_sequences() {
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
    assert_eq!(
        fixture.next_seq().await,
        2,
        "the counter counts settlements"
    );
}

/// A finalize that loses the lease fence must change nothing at all — including
/// the group's counter.
///
/// Bumping first and writing the child second would burn a rank on a write that
/// never lands: the group would count a settlement no row carries, and a consumer
/// waiting on that rank would wait forever. This is why the fenced `UPDATE` runs
/// first, returns the child's own `group_key`, and the counter moves only when it
/// returned a row.
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

    let removed = fixture
        .store
        .retire_journal(&lash_core::EffectJournalRetirement::Session {
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
}

/// The unsettled read is the exact complement of the rank read: every child of
/// the group is in one answer or the other, never both and never neither.
///
/// It is what makes "this group is complete" a single question instead of a walk
/// up the ranks, and it is the drain queue FIG-1536 reads, which is why the row
/// carries the child's journal identity, its recorded envelope, and its lease
/// boundary rather than only its key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsettled_children_are_exactly_the_children_without_a_rank() {
    let Some(fixture) = GroupFixture::open("unsettled").await else {
        eprintln!("skipping effect-group unsettled read: database URL is not set");
        return;
    };
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
    assert_eq!(unsettled[0].scope_id, fixture.scope_id);
    assert_eq!(
        unsettled[0].state,
        effect_replay_driver::EffectRowState::InProgress
    );
    assert_eq!(
        unsettled[0].envelope_json, r#"{"json":"k1","hash":"hash-k1"}"#,
        "the row carries the recorded canonical envelope a drain re-executes from"
    );
    assert!(
        unsettled[0].lease_expires_at_ms > 0,
        "the row carries the lease boundary a drain decides takeover against"
    );

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
        vec!["k2"],
        "a child that took a rank leaves the unsettled set in the same write"
    );

    fixture
        .store
        .finalize(&second_fence, &GroupFixture::terminal("k2"))
        .await
        .expect("finalize the second child");
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
    shrunk.children = 1;
    shrunk.loser_disposition = lash_core::LoserPolicy::Cancel;
    shrunk.created_at_ms = 9_999;
    let reopened = fixture
        .store
        .open_group(&shrunk)
        .await
        .expect("reopening an existing group is idempotent at the store seam");
    assert_eq!(
        reopened, recorded,
        "the recorded row wins: a reopen may not restate a group's children or \
         its declared disposition, and the store is what says so"
    );
}
