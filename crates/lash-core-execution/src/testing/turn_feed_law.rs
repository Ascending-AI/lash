//! The turn-change feed's cursor law, run by every store (FIG-5276).
//!
//! A reader that polls `turns_changed_since` from where its last page left
//! off, while many writers commit concurrently, sees every change exactly
//! once: it never skips a change whose transaction committed after a higher
//! one, and it never reads one twice. A store whose writers can commit out of
//! order supplies a late committer: a change held right before its `COMMIT`
//! while the others commit and the reader moves on.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::store::{
    SessionFaultOrigin, SessionFaultRecord, SessionFaultStore, TurnChangeCursor, TurnChangeKind,
};
use crate::{DeploymentStore, SessionCatalogStore, SessionId};

/// How long the law lets the other writers commit while the late one is
/// held. A store that orders its commits behind the held one cannot commit
/// them until it is released; the law then goes on without them.
const HELD_WINDOW: Duration = Duration::from_secs(2);

/// How often the reader polls.
const POLL_EVERY: Duration = Duration::from_millis(2);

/// A commit the store holds right before its `COMMIT`, armed for the next
/// transaction that commits.
pub struct LateCommit {
    /// Resolves once the held transaction has run every statement.
    pub reached: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Lets the held transaction commit.
    pub release: Box<dyn FnOnce() + Send>,
}

/// Arms the store's [`LateCommit`] hold.
pub type ArmLateCommit = Box<dyn FnOnce() -> LateCommit + Send>;

/// A reader that follows the turn feed from the initial cursor, checking the
/// law on every page.
#[derive(Debug, Default)]
struct Follower {
    cursor: TurnChangeCursor,
    /// Every change read, by session and kind, with the cursor it came at.
    seen: BTreeMap<(SessionId, String), TurnChangeCursor>,
}

impl Follower {
    /// Read pages until one comes back empty.
    async fn poll<S: DeploymentStore + ?Sized>(&mut self, store: &S) {
        loop {
            let page = store
                .turns_changed_since(self.cursor, NonZeroUsize::new(64).expect("nonzero"))
                .await
                .expect("the turn feed reads from the reader's own cursor");
            let empty = page.changes.is_empty();
            for change in page.changes {
                assert!(
                    change.cursor.store_sequence() > self.cursor.store_sequence(),
                    "a change came at {:?}, not after the reader's cursor {:?}",
                    change.cursor,
                    self.cursor
                );
                assert!(
                    matches!(change.kind, TurnChangeKind::SessionFault { .. }),
                    "every change the law makes is a recorded fault: {:?}",
                    change.kind
                );
                self.cursor = change.cursor;
                let key = (change.session_id, format!("{:?}", change.kind));
                if let Some(first) = self.seen.insert(key.clone(), change.cursor) {
                    panic!(
                        "the reader saw {key:?} twice: at {first:?} and at {:?}",
                        change.cursor
                    );
                }
            }
            assert!(
                page.next.store_sequence() >= self.cursor.store_sequence(),
                "the page's next cursor {:?} is behind its last change {:?}",
                page.next,
                self.cursor
            );
            self.cursor = page.next;
            if empty {
                return;
            }
        }
    }

    fn sessions(&self) -> BTreeSet<SessionId> {
        self.seen
            .keys()
            .map(|(session, _)| session.clone())
            .collect()
    }
}

fn fault(message: &str) -> SessionFaultRecord {
    SessionFaultRecord {
        origin: SessionFaultOrigin::DriveAdmission,
        code: crate::RuntimeErrorCode::RuntimeStoreCorrupt,
        message: message.to_owned(),
        cause: None,
    }
}

async fn record_fault<S>(store: &S, session_id: &SessionId)
where
    S: SessionFaultStore + ?Sized,
{
    store
        .record_session_fault(session_id, &fault(session_id.as_str()), 7)
        .await
        .expect("the session's fault commits");
}

/// `writers` sessions each record a fault concurrently while a reader polls
/// the turn feed; with `late`, one more fault is held before its `COMMIT`
/// until the others had their chance to commit and be read. The reader sees
/// each fault exactly once, at a cursor above every change it read before.
pub async fn a_polling_reader_never_skips_or_repeats_a_turn_change<S>(
    store: Arc<S>,
    writers: usize,
    late: Option<ArmLateCommit>,
) where
    S: DeploymentStore + SessionCatalogStore + SessionFaultStore + Send + Sync + 'static,
{
    let late_session = SessionId::from("turn-feed-late-writer");
    let sessions: Vec<SessionId> = (0..writers)
        .map(|writer| SessionId::parse(format!("turn-feed-writer-{writer}")).expect("a session id"))
        .collect();
    for session_id in sessions.iter().chain([&late_session]) {
        store
            .admit_session(&super::store_fixtures::root_session_request(session_id))
            .await
            .expect("admit the session");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let read = Arc::new(AtomicUsize::new(0));
    let reader = crate::task::spawn({
        let store = Arc::clone(&store);
        let stop = Arc::clone(&stop);
        let read = Arc::clone(&read);
        async move {
            let mut follower = Follower::default();
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
                let session_id = late_session.clone();
                async move { record_fault(store.as_ref(), &session_id).await }
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
            crate::task::spawn(async move { record_fault(store.as_ref(), &session_id).await })
        })
        .collect();
    let late_task = match held {
        Some((task, release)) => {
            // The others commit and the reader passes them, or, on a store
            // that orders commits behind the held one, the window ends.
            let _ = tokio::time::timeout(HELD_WINDOW, async {
                while read.load(Ordering::Acquire) < writers {
                    tokio::time::sleep(POLL_EVERY).await;
                }
            })
            .await;
            release();
            task
        }
        None => crate::task::spawn({
            let store = Arc::clone(&store);
            let session_id = late_session.clone();
            async move { record_fault(store.as_ref(), &session_id).await }
        }),
    };
    for task in tasks {
        task.await.expect("join a writer");
    }
    late_task.await.expect("join the late writer");

    stop.store(true, Ordering::Release);
    let mut follower = reader.await.expect("join the reader");
    follower.poll(store.as_ref()).await;
    let expected: BTreeSet<SessionId> = sessions.into_iter().chain([late_session]).collect();
    assert_eq!(
        follower.sessions(),
        expected,
        "the reader saw every committed fault exactly once"
    );
}
