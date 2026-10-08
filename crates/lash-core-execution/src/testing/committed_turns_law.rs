//! The committed-turn read's cursor law, run by every store (FIG-5297).
//!
//! A reader that polls one session's `load_committed_turns` from where its
//! last page left off, while other sessions commit turns concurrently, reads
//! each of the session's turns exactly once, in commit order. A store whose
//! writers can commit out of order supplies a late committer: the session's
//! turn held right before its `COMMIT` while the other sessions commit and
//! the reader polls on.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::turn_feed_law::ArmLateCommit;
use crate::store::{
    CommittedTurnCursor, OperationId, RuntimeCommit, RuntimeTurnCommitStamp, SessionCommitStore,
    SessionHistoryStore, TurnCommitOutcome,
};
use crate::{DeploymentStore, SessionCatalogStore, SessionId, TurnId};

/// How long the law lets the other sessions commit while the late turn is
/// held.
const HELD_WINDOW: Duration = Duration::from_secs(2);

/// How often the reader polls.
const POLL_EVERY: Duration = Duration::from_millis(2);

/// The turns each session commits.
const TURNS: usize = 4;

/// The commit of `session_id`'s turn `turn` over head revision
/// `expected_head_revision`: a completed turn that appends no node.
fn completed_turn(
    session_id: &SessionId,
    expected_head_revision: u64,
    turn: usize,
) -> RuntimeCommit {
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state);
    commit.expected_head_revision = expected_head_revision;
    commit.turn_commit = RuntimeTurnCommitStamp::new(OperationId::new(
        crate::ExecutionScope::turn(session_id.clone(), turn_id(turn)),
        "final",
    ));
    commit.outcome = Some(TurnCommitOutcome::Completed);
    commit
}

fn turn_id(turn: usize) -> TurnId {
    TurnId::parse(format!("committed-turn-{turn}")).expect("a turn id")
}

/// Commit `session_id`'s turns `turns`, one after another, each over the
/// head revision the one before published.
async fn commit_turns<S>(store: &S, session_id: &SessionId, turns: std::ops::Range<usize>)
where
    S: SessionCommitStore + ?Sized,
{
    for turn in turns {
        let head = store
            .load_session_head_meta(session_id)
            .await
            .expect("read the session's head")
            .map_or(0, |head| head.head_revision);
        store
            .commit_runtime_state(completed_turn(session_id, head, turn))
            .await
            .expect("the session's turn commits");
    }
}

/// A reader that follows one session's committed turns from its start,
/// checking the law on every page.
struct Follower {
    session_id: SessionId,
    cursor: Option<CommittedTurnCursor>,
    seen: Vec<TurnId>,
}

impl Follower {
    /// Read pages of one turn until one comes back empty.
    async fn poll<S: SessionHistoryStore + ?Sized>(&mut self, store: &S) {
        loop {
            let page = store
                .load_committed_turns(&self.session_id, self.cursor.as_ref(), NonZeroU32::MIN)
                .await
                .expect("the session's committed turns read from the reader's own cursor");
            assert!(page.turns.len() <= 1, "a page holds at most its limit");
            let after = self
                .cursor
                .as_ref()
                .map_or(0, CommittedTurnCursor::head_revision);
            for turn in &page.turns {
                assert!(
                    turn.cursor.head_revision() > after,
                    "turn {} came at revision {}, not after the reader's {after}",
                    turn.turn_id,
                    turn.cursor.head_revision()
                );
                assert_eq!(turn.outcome, TurnCommitOutcome::Completed);
                assert!(
                    !self.seen.contains(&turn.turn_id),
                    "the reader saw {} twice",
                    turn.turn_id
                );
                self.seen.push(turn.turn_id.clone());
            }
            assert_eq!(page.next.session_id(), &self.session_id);
            assert!(
                page.next.head_revision() >= after,
                "the page's next cursor {:?} is behind the reader's {after}",
                page.next
            );
            self.cursor = Some(page.next);
            if page.turns.is_empty() {
                return;
            }
        }
    }
}

/// `writers` other sessions commit turns concurrently while a reader polls
/// the followed session, which commits its turns too; with `late`, the
/// followed session's middle turn is held before its `COMMIT` while the
/// others commit and the reader polls. The reader reads each of the
/// session's turns exactly once, in the order they committed, and a cursor
/// of another session is refused.
pub async fn a_reader_polling_one_session_never_misses_or_repeats_a_turn<S>(
    store: Arc<S>,
    writers: usize,
    late: Option<ArmLateCommit>,
) where
    S: DeploymentStore + SessionCatalogStore + Send + Sync + 'static,
{
    let followed = SessionId::from("committed-turns-followed");
    let sessions: Vec<SessionId> = (0..writers)
        .map(|writer| {
            SessionId::parse(format!("committed-turns-writer-{writer}")).expect("a session id")
        })
        .collect();
    for session_id in sessions.iter().chain([&followed]) {
        store
            .admit_session(&super::store_fixtures::root_session_request(session_id))
            .await
            .expect("admit the session");
    }
    let middle = TURNS / 2;
    commit_turns(store.as_ref(), &followed, 0..middle).await;

    let stop = Arc::new(AtomicBool::new(false));
    let read = Arc::new(AtomicUsize::new(0));
    let reader = crate::task::spawn({
        let store = Arc::clone(&store);
        let stop = Arc::clone(&stop);
        let read = Arc::clone(&read);
        let session_id = followed.clone();
        async move {
            let mut follower = Follower {
                session_id,
                cursor: None,
                seen: Vec::new(),
            };
            while !stop.load(Ordering::Acquire) {
                follower.poll(store.as_ref()).await;
                read.store(follower.seen.len(), Ordering::Release);
                tokio::time::sleep(POLL_EVERY).await;
            }
            follower
        }
    });

    let held = match late {
        Some(arm) => {
            let hold = arm();
            let task = crate::task::spawn({
                let store = Arc::clone(&store);
                let session_id = followed.clone();
                async move { commit_turns(store.as_ref(), &session_id, middle..middle + 1).await }
            });
            hold.reached.await;
            Some((task, hold.release))
        }
        None => None,
    };
    let tasks: Vec<_> = sessions
        .iter()
        .cloned()
        .map(|session_id| {
            let store = Arc::clone(&store);
            crate::task::spawn(
                async move { commit_turns(store.as_ref(), &session_id, 0..TURNS).await },
            )
        })
        .collect();
    let late_task = match held {
        Some((task, release)) => {
            // The reader reads the turns before the held one and polls on
            // while the other sessions commit.
            let _ = tokio::time::timeout(HELD_WINDOW, async {
                while read.load(Ordering::Acquire) < middle {
                    tokio::time::sleep(POLL_EVERY).await;
                }
            })
            .await;
            release();
            task
        }
        None => crate::task::spawn({
            let store = Arc::clone(&store);
            let session_id = followed.clone();
            async move { commit_turns(store.as_ref(), &session_id, middle..middle + 1).await }
        }),
    };
    for task in tasks {
        task.await.expect("join a writer");
    }
    late_task.await.expect("join the late turn");
    commit_turns(store.as_ref(), &followed, middle + 1..TURNS).await;

    stop.store(true, Ordering::Release);
    let mut follower = reader.await.expect("join the reader");
    follower.poll(store.as_ref()).await;
    assert_eq!(
        follower.seen,
        (0..TURNS).map(turn_id).collect::<Vec<_>>(),
        "the reader read every committed turn of the session once, in commit order"
    );

    let foreign = store
        .load_committed_turns(&sessions[0], follower.cursor.as_ref(), NonZeroU32::MIN)
        .await
        .expect_err("a cursor of another session is refused");
    assert!(
        matches!(foreign, crate::StoreError::CursorForeignSession { .. }),
        "the refusal is typed: {foreign:?}"
    );
}
