//! How the replay driver waits on a SQLite journal (FIG-3579).
//!
//! A claim queued behind another owner's live lease and a discharge held by
//! the commit-order barrier park on the journal's change notifications, raced
//! against the driver clock's deadline. These tests hold each wait to that
//! under the clocks that broke the old fixed poll: a frozen clock whose sleeps
//! return at once (the poll became a hot spin) and a clock nobody advances
//! (the poll never came back). The lease still expires from the clock, and a
//! file journal still notices a writer in another process on its bounded
//! fallback poll.

use super::*;
use lash_core_execution::{
    RuntimeEffectController, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const EPOCH_MS: u64 = 1_700_000_000_000;
const GROUP: &str = "session:s1/wait-group";

/// Wall and monotonic faces that never move, and sleeps that return at once,
/// counting every face read: the test-local frozen clock `direct.rs` uses.
#[derive(Debug)]
struct FrozenClock {
    instant: Instant,
    monotonic_reads: AtomicU64,
    reads: AtomicU64,
}

impl FrozenClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            instant: Instant::now(),
            monotonic_reads: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        })
    }

    /// Every face read and sleep so far.
    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    /// Monotonic reads so far: the driver takes one per claim attempt.
    fn claim_attempts(&self) -> u64 {
        self.monotonic_reads.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::Clock for FrozenClock {
    fn now(&self) -> Instant {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.monotonic_reads.fetch_add(1, Ordering::SeqCst);
        self.instant
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        chrono::DateTime::from(std::time::UNIX_EPOCH + Duration::from_millis(EPOCH_MS))
    }

    async fn sleep(&self, _duration: Duration) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }

    async fn sleep_until(&self, _deadline: Instant) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

/// Virtual time that moves only when the test advances it, and sleeps that
/// park until it reaches their deadline: `lash-sim`'s `SimClock` contract. A
/// clock never advanced is the simulator between scheduler steps.
#[derive(Debug)]
struct SteppedClock {
    origin: Instant,
    elapsed_ms: tokio::sync::watch::Sender<u64>,
}

impl SteppedClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: Instant::now(),
            elapsed_ms: tokio::sync::watch::channel(0).0,
        })
    }

    fn advance(&self, ms: u64) {
        self.elapsed_ms.send_modify(|elapsed| *elapsed += ms);
    }

    async fn park_until(&self, target_ms: u64) {
        let mut elapsed = self.elapsed_ms.subscribe();
        let _ = elapsed.wait_for(|elapsed| *elapsed >= target_ms).await;
    }
}

#[async_trait::async_trait]
impl lash_core_execution::Clock for SteppedClock {
    fn now(&self) -> Instant {
        self.origin + Duration::from_millis(*self.elapsed_ms.borrow())
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let ms = EPOCH_MS + *self.elapsed_ms.borrow();
        chrono::DateTime::from(std::time::UNIX_EPOCH + Duration::from_millis(ms))
    }

    async fn sleep(&self, duration: Duration) {
        let target = *self.elapsed_ms.borrow() + duration.as_millis() as u64;
        self.park_until(target).await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        let target = deadline.saturating_duration_since(self.origin).as_millis() as u64;
        self.park_until(target).await;
    }
}

fn envelope(scope: &ExecutionScope, replay_key: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core_execution::RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope.clone(), replay_key)
                .expect("valid effect address"),
            lash_core_execution::RuntimeAttribution::none(),
            replay_key,
        ),
        lash_core_execution::RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "wait-witness".to_string(),
        },
    )
}

fn value(text: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::LanguageRuntimeValue {
        value: serde_json::json!(text),
    }
}

fn encoded(outcome: &RuntimeEffectOutcome) -> serde_json::Value {
    serde_json::to_value(outcome).expect("encode the outcome")
}

/// Run `envelope` on `controller` in its own task with an executor that
/// reports its start and then holds the claim until released.
fn hold_claim(
    controller: SqliteRuntimeEffectController,
    envelope: RuntimeEffectEnvelope,
    outcome: RuntimeEffectOutcome,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
) {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let holding = tokio::spawn(async move {
        controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    started_tx.send(()).expect("report the claim held");
                    let _ = release_rx.await;
                    Ok(outcome)
                }),
            )
            .await
    });
    (started_rx, release_tx, holding)
}

/// Run `envelope` on `controller` in its own task with an executor that must
/// never run: the queued claim is answered from the holder's row.
fn queue_for_replay(
    controller: SqliteRuntimeEffectController,
    envelope: RuntimeEffectEnvelope,
) -> tokio::task::JoinHandle<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>> {
    tokio::spawn(async move {
        controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async {
                    panic!("a claim queued behind a finalizing holder must replay its row")
                }),
            )
            .await
    })
}

/// Under a frozen clock the lease never expires and every sleep returns at
/// once. A claim queued behind that live lease must park on the journal, not
/// re-claim in a hot loop, and the holder's finalize must wake it. The claim
/// count is exact because a memory journal has no cross-process writer to
/// poll for: the queued claim is read once, re-read once with its wake armed,
/// and then left alone until something changes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_queued_behind_a_live_lease_parks_under_a_frozen_clock() {
    // The holder's clock never moves either, but parks its sleeps, so its
    // lease renewal stays asleep: what this test counts is the waiter alone,
    // on a clock of its own with the same frozen wall face.
    let holder_clock = SteppedClock::new();
    let waiter_clock = FrozenClock::new();
    let deployment = crate::SqliteDeployment::memory_with_clock(holder_clock)
        .await
        .expect("open the memory deployment");
    let waiter_deployment = deployment
        .reopen_with_clock(waiter_clock.clone())
        .await
        .expect("reopen the memory deployment on the waiter's clock");
    let scope = ExecutionScope::turn("frozen-session", "frozen-turn");
    let holder = deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the holder's controller");
    let waiter = waiter_deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the waiter's controller");

    let (started, release, holding) =
        hold_claim(holder, envelope(&scope, "frozen-effect"), value("held"));
    started.await.expect("the holder claims the row");

    let reads_before = waiter_clock.reads();
    let queued = queue_for_replay(waiter, envelope(&scope, "frozen-effect"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reads = waiter_clock.reads() - reads_before;
    let claims = waiter_clock.claim_attempts();
    assert!(
        !queued.is_finished(),
        "the lease is live on the frozen clock, so the claim stays queued"
    );
    assert!(
        claims <= 2,
        "a queued claim on a memory journal is read, re-read with its wake armed, \
         and then parked; it was attempted {claims} times in 300ms"
    );
    assert!(
        reads <= 64,
        "a queued claim must park, not spin on a clock whose sleeps return at once; \
         it read or slept on the clock {reads} times in 300ms"
    );

    release.send(()).expect("release the holder");
    let held = holding
        .await
        .expect("holder task")
        .expect("the holder finalizes");
    let replayed = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("the holder's finalize wakes the queued claim")
        .expect("queued task")
        .expect("the queued claim replays the holder's row");
    assert_eq!(encoded(&replayed), encoded(&held));
}

/// A grouped child whose discharge the commit-order barrier holds behind a
/// lower-committed sibling resumes when that sibling drains — on the drain's
/// notification, with the clock never moving. The old fixed poll slept on the
/// driver clock, so under a clock nobody advances it never came back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discharge_held_by_the_commit_order_barrier_resumes_when_its_blocker_drains() {
    let clock = SteppedClock::new();
    let deployment = crate::SqliteDeployment::memory_with_clock(clock.clone())
        .await
        .expect("open the memory deployment");
    let host = deployment.effect_host();
    let store = SqliteEffectReplayRowStore {
        conn: SqliteConnection::open(host.journal.target())
            .await
            .expect("open the staging connection"),
        clock: clock.clone(),
        registry: Arc::new(crate::scope_fence::RegistryAttachment::default()),
        wake: JournalWakeKey::for_journal(&host.journal),
    };
    let scope = ExecutionScope::turn("s1", "wait-group");
    let scope_id = scope
        .journal_identity()
        .expect("turn journal identity")
        .key()
        .to_string();
    let membership = |replay_key: &str, position: usize| AcceptedGroupChild {
        position,
        replay_key: replay_key.to_string(),
        envelope_json: format!(r#"{{"json":"{replay_key}"}}"#),
        command_version: 2,
    };
    store
        .open_group(
            &EffectGroupRecord {
                group_key: GROUP.to_string(),
                scope_id: scope_id.clone(),
                session_id: Some(SessionId::from("s1")),
                wake: lash_core_execution::GroupWakePolicy::All,
                loser_disposition: lash_core_execution::LoserPolicy::RunToCompletion,
                expected_children: 2,
                lifecycle: EffectGroupLifecycle::Live,
                created_at_ms: EPOCH_MS,
            },
            &[membership("k1", 0), membership("k2", 1)],
        )
        .await
        .expect("open the group row");

    // `k1` commits first and still owes its drain: the barrier every later
    // commit waits behind.
    let blocker = EffectClaimRequest {
        scope_id: scope_id.clone(),
        session_id: Some(SessionId::from("s1")),
        replay_key: "k1".to_string(),
        envelope_hash: "hash-k1".to_string(),
        envelope_json: r#"{"json":"k1"}"#.to_string(),
        owner_id: "blocker".to_string(),
        lease_token: "blocker-token".to_string(),
        lease_ttl_ms: 30_000,
        sleep: None,
        group_key: Some(GROUP.to_string()),
        minting_effect: None,
        strict_replay: false,
    };
    assert!(matches!(
        store.claim(&blocker).await.expect("claim k1"),
        EffectClaimObservation::Claimed { .. }
    ));
    let committed = store
        .finalize(
            &EffectLeaseFence {
                scope_id: scope_id.clone(),
                replay_key: "k1".to_string(),
                envelope_hash: blocker.envelope_hash.clone(),
                owner_id: blocker.owner_id.clone(),
                lease_token: blocker.lease_token.clone(),
            },
            &EffectTerminal::Completed {
                outcome_json: encoded(&value("k1")).to_string(),
            },
        )
        .await
        .expect("commit k1");
    assert!(matches!(
        committed,
        EffectFinalizeOutcome::Written {
            commit_seq: Some(1)
        }
    ));

    let mut k2 = envelope(&scope, "k2");
    k2.group = Some(Box::new(lash_core_execution::EffectGroupMembership {
        group_key: GROUP.to_string(),
        position: 1,
        wake: lash_core_execution::GroupWakePolicy::All,
        loser_disposition: lash_core_execution::LoserPolicy::RunToCompletion,
    }));
    let controller = deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the child's controller");
    let settling = tokio::spawn(async move {
        controller
            .execute_effect(
                k2,
                RuntimeEffectLocalExecutor::testing(|_| async { Ok(value("k2")) }),
            )
            .await
    });

    // `k2` commits behind `k1`; its discharge is now held by the barrier.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let arbitration = store
                .read_group_child_arbitration(&scope_id, "k2")
                .await
                .expect("read k2's arbitration");
            if arbitration.is_some_and(|arbitration| {
                matches!(arbitration.commit_state, EffectCommitState::Committed)
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("k2 commits behind k1");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !settling.is_finished(),
        "k2's discharge waits behind k1's owed drain"
    );

    assert!(matches!(
        store
            .discharge_child(&EffectDischargeRequest {
                group_key: GROUP.to_string(),
                scope_id: scope_id.clone(),
                replay_key: "k1".to_string(),
                terminal: None,
            })
            .await
            .expect("drain k1"),
        EffectDischargeOutcome::Discharged { settlement_seq: 1 }
    ));
    let settled = tokio::time::timeout(Duration::from_secs(5), settling)
        .await
        .expect("k1's drain wakes k2's held discharge without the clock advancing")
        .expect("child task")
        .expect("k2 settles");
    assert_eq!(encoded(&settled), encoded(&value("k2")));
    let second = store
        .read_group_settlement(GROUP, 2)
        .await
        .expect("read rank two")
        .expect("k2 took rank two");
    assert_eq!(second.replay_key, "k2");
}

/// Notifications do not replace the clock: a holder that stops renewing
/// without finalizing wakes nobody, and the queued claim takes the row over
/// when the clock reaches the lease's expiry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_that_expires_on_the_clock_hands_the_queued_claim_the_row() {
    let clock = SteppedClock::new();
    let deployment = crate::SqliteDeployment::memory_with_clock(clock.clone())
        .await
        .expect("open the memory deployment");
    let scope = ExecutionScope::turn("expiry-session", "expiry-turn");
    let holder = deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the holder's controller");
    let successor = deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the successor's controller");

    let (started, _release, abandoned) =
        hold_claim(holder, envelope(&scope, "expiry-effect"), value("never"));
    started.await.expect("the holder claims the row");
    abandoned.abort();
    assert!(
        abandoned
            .await
            .expect_err("the holder is abandoned without finalizing")
            .is_cancelled()
    );

    let reclaiming = tokio::spawn(async move {
        successor
            .execute_effect(
                envelope(&scope, "expiry-effect"),
                RuntimeEffectLocalExecutor::testing(|_| async { Ok(value("reclaimed")) }),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !reclaiming.is_finished(),
        "the abandoned lease is live until the clock reaches its expiry"
    );

    clock.advance(LeaseTimings::default().ttl_ms());
    let reclaimed = tokio::time::timeout(Duration::from_secs(5), reclaiming)
        .await
        .expect("the clock reaching the lease expiry wakes the queued claim")
        .expect("successor task")
        .expect("the successor takes the expired row over");
    assert_eq!(encoded(&reclaimed), encoded(&value("reclaimed")));
}

/// A file journal can be written by another process, whose commits wake
/// nothing here. A claim queued behind such a writer still sees it finalize,
/// on the bounded fallback poll, long before the lease would expire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_journal_notices_a_foreign_process_finalize_on_its_fallback_poll() {
    let dir = tempfile::tempdir().expect("deployment root");
    let deployment = crate::SqliteDeployment::open(dir.path())
        .await
        .expect("open the file deployment");
    let scope = ExecutionScope::turn("foreign-session", "foreign-turn");
    let waiter = deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the waiter's controller");

    // The other process: its own connection and a wake identity nothing in
    // this process listens on.
    let journal = DatabaseTarget::File(dir.path().join(SqliteDatabase::EffectReplay.file_name()));
    let foreign = Arc::new(build_effect_replay_driver(
        SqliteConnection::open(&journal)
            .await
            .expect("open the foreign connection"),
        SqliteEffectReplayOptions::default(),
        Arc::new(lash_core_execution::facade_support::SystemClock),
        vec![0; 32],
        Arc::new(crate::scope_fence::RegistryAttachment::default()),
        JournalWakeKey {
            identity: Arc::from("sqlite:foreign-process"),
            writers: EffectJournalWriters::Unannounced,
        },
    ));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let foreign_scope = scope.clone();
    let holding = tokio::spawn(async move {
        foreign
            .execute_effect(
                &foreign_scope,
                envelope(&foreign_scope, "foreign-effect"),
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    started_tx.send(()).expect("report the claim held");
                    let _ = release_rx.await;
                    Ok(value("foreign"))
                }),
                None,
            )
            .await
    });
    started_rx
        .await
        .expect("the foreign process claims the row");

    let queued = queue_for_replay(waiter, envelope(&scope, "foreign-effect"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!queued.is_finished(), "the foreign lease is live");
    release_tx.send(()).expect("release the foreign holder");
    holding
        .await
        .expect("foreign task")
        .expect("the foreign holder finalizes");
    let replayed = tokio::time::timeout(Duration::from_secs(3), queued)
        .await
        .expect("the fallback poll sees the foreign finalize well before the lease expires")
        .expect("queued task")
        .expect("the queued claim replays the foreign row");
    assert_eq!(encoded(&replayed), encoded(&value("foreign")));
}

/// A resolver that runs each staged child's executor once, by replay key.
#[derive(Default)]
struct StagedChildren(std::sync::Mutex<std::collections::HashMap<String, Executor>>);

type Executor = RuntimeEffectLocalExecutor<'static>;

impl lash_core_execution::GroupExecutors for StagedChildren {
    fn executor_for(&self, envelope: &RuntimeEffectEnvelope) -> Option<Executor> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(envelope.invocation.replay_key())
    }
}

/// A child body that runs until `release` fires, counting its returns.
fn parked_child(
    release: tokio_util::sync::CancellationToken,
    started: Arc<AtomicU64>,
    finished: Arc<AtomicU64>,
    text: &'static str,
) -> Executor {
    RuntimeEffectLocalExecutor::testing(move |_| async move {
        started.fetch_add(1, Ordering::SeqCst);
        release.cancelled().await;
        finished.fetch_add(1, Ordering::SeqCst);
        Ok(value(text))
    })
}

/// Closing a `RunToCompletion` group hands its still-running children to the
/// close-time finalizer, which waits for each to return. After the first of
/// two returns, the finalizer must park again for the second — not re-await a
/// wake that already fired, which returns at once forever and never yields.
///
/// The scenario runs on its own current-thread runtime so such a spin freezes
/// that runtime outright; the watchdog turns the freeze into a failure rather
/// than a hung test.
#[test]
fn a_close_time_finalizer_parks_between_its_running_childrens_returns() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the scenario runtime");
        runtime.block_on(close_with_two_running_children());
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(20))
        .expect("the finalizer parks between returns; a spinning one never yields its runtime");
}

async fn close_with_two_running_children() {
    let deployment = crate::SqliteDeployment::memory()
        .await
        .expect("open the memory deployment");
    let prefix = format!("close-wait-{}", uuid::Uuid::new_v4().simple());
    let scope = ExecutionScope::runtime_operation(format!("{prefix}-op"));
    let group_key = format!("{prefix}:group:g:0");
    let controller = deployment
        .open_effect_controller(scope.clone())
        .await
        .expect("open the group's controller");
    let resolver = Arc::new(StagedChildren::default());
    controller
        .register_group_executors(resolver.clone())
        .expect("register the resolver");

    let children: Vec<RuntimeEffectEnvelope> = (0..3)
        .map(|position| envelope(&scope, &format!("{group_key}:child:{position}")))
        .collect();
    let (first, second) = (
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    );
    let started = Arc::new(AtomicU64::new(0));
    let returned = Arc::new(AtomicU64::new(0));
    {
        let mut staged = resolver
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        staged.insert(
            children[0].invocation.replay_key().to_string(),
            RuntimeEffectLocalExecutor::testing(|_| async { Ok(value("winner")) }),
        );
        for (child, release) in children[1..].iter().zip([&first, &second]) {
            staged.insert(
                child.invocation.replay_key().to_string(),
                parked_child(release.clone(), started.clone(), returned.clone(), "loser"),
            );
        }
    }
    let group = lash_core_execution::RuntimeEffectGroup::try_new(
        lash_core_execution::RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            lash_core_execution::RuntimeAttribution::none(),
            "group",
        ),
        group_key.clone(),
        children,
        lash_core_execution::GroupWakePolicy::All,
        lash_core_execution::LoserPolicy::RunToCompletion,
    )
    .expect("assemble the group");
    let mut handle = controller
        .open_effect_group(group)
        .await
        .expect("open the group");
    while started.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    controller
        .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new())
        .await
        .expect("the winner takes rank one");
    controller
        .close_effect_group(handle, lash_core_execution::LoserPolicy::RunToCompletion)
        .await
        .expect("the close records `closing` and hands the losers to the finalizer");

    // The first loser's return wakes the finalizer, which still owes the
    // second: it must park again rather than spin.
    first.cancel();
    while returned.load(Ordering::SeqCst) < 1 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    second.cancel();
    while returned.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
}
