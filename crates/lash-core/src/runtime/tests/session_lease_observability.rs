//! The four decisive session-execution-lease transitions must each emit one
//! structured trace event carrying the session id, the lease generation, and the
//! holder identity: enough for a log timeline to reconstruct takeover order.
//!
//! These are trace events, never durable session events (lease churn is
//! per-attempt telemetry, not session history), so the oracle here is a capture
//! layer over the `tracing` dispatcher rather than an event sink.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime::session_execution_lease::{
    SessionExecutionLeaseCommitEvidence, SessionExecutionLeaseGuard, trace_commit_cas_rejected,
};
use crate::store::StoreError;
use crate::{LeaseOwnerIdentity, LeaseTimings, SystemClock};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CapturedEvent {
    level: String,
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    pub(super) fn field(&self, name: &str) -> &str {
        self.fields
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("event is missing field `{name}`: {self:?}"))
    }
}

#[derive(Clone, Default)]
pub(super) struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl EventCapture {
    /// Every captured event whose `event` field names this lease transition.
    pub(super) fn named(&self, event: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("lock captured events")
            .iter()
            .filter(|captured| captured.fields.get("event").map(String::as_str) == Some(event))
            .cloned()
            .collect()
    }

    pub(super) fn exactly_one(&self, event: &str) -> CapturedEvent {
        let matched = self.named(event);
        assert_eq!(
            matched.len(),
            1,
            "expected exactly one `{event}` trace event, captured: {:?}",
            self.events.lock().expect("lock captured events")
        );
        matched.into_iter().next().expect("checked one event")
    }
}

struct FieldVisitor(BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // `Option` fields render as `Some("x")` / `None`; unwrap the payload so
        // an assertion reads the value rather than the wrapper.
        let rendered = format!("{value:?}");
        let unwrapped = rendered
            .strip_prefix("Some(")
            .and_then(|rest| rest.strip_suffix(')'))
            .map(|inner| inner.trim_matches('"').to_string())
            .unwrap_or_else(|| rendered.trim_matches('"').to_string());
        self.0.insert(field.name().to_string(), unwrapped);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S: tracing::Subscriber> Layer<S> for EventCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("lock captured events")
            .push(CapturedEvent {
                level: event.metadata().level().to_string(),
                fields: visitor.0,
            });
    }
}

/// Run `body` with a capture layer installed as the thread-local dispatcher.
///
/// `#[tokio::test]` builds a current-thread runtime, so the lease renewal task
/// is polled on this same thread and its events land in the same capture.
pub(super) async fn capturing<F, Fut, T>(body: F) -> (T, EventCapture)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let capture = EventCapture::default();
    let subscriber = Registry::default().with(capture.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    let value = body().await;
    drop(guard);
    (value, capture)
}

/// Several renewal intervals, so the loop has provably run and failed.
const TRANSIENT_SETTLE: Duration = Duration::from_millis(400);

fn owner(owner_id: &str, incarnation: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, incarnation)
}

fn new_store() -> Arc<dyn crate::store::RuntimePersistence> {
    Arc::new(crate::runtime::InMemorySessionStore::new())
}

fn short_timings() -> LeaseTimings {
    LeaseTimings::new(Duration::from_millis(90), Duration::from_millis(30))
        .expect("ttl >= 3 * renew_interval")
}

#[tokio::test]
async fn claiming_the_lane_traces_the_session_generation_and_holder() {
    let store = new_store();
    let claimant = owner("worker-a", "worker-a:boot-1");
    let (guard, capture) = capturing(|| async {
        SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            "lease-observability",
            &claimant,
            LeaseTimings::default(),
            Arc::new(SystemClock),
        )
        .await
        .expect("claim the session execution lane")
        .expect("an unheld lane is acquirable")
    })
    .await;

    let claimed = capture.exactly_one("session_execution_lease.acquired");
    assert_eq!(claimed.level, "INFO");
    assert_eq!(claimed.field("session_id"), "lease-observability");
    assert_eq!(claimed.field("owner_id"), "worker-a");
    assert_eq!(claimed.field("incarnation_id"), "worker-a:boot-1");
    assert_eq!(
        claimed.field("fencing_token"),
        guard.fence().fencing_token.to_string(),
        "the traced fencing token must be the fence the claim returned"
    );
    guard
        .release_if_live()
        .await
        .expect("release the claimed lane");
}

/// The flagship production case: worker A stalls or dies, so its renewal task is
/// not running when its lease lapses and worker B sweeps the lane.
///
/// Nothing A would have logged happens, so a takeover reported from A's
/// renewal-failure path would be silently absent here. The event has to come from
/// B, atomically with the claim that displaced A. This is the regression for the
/// review probe that found zero `taken_over` events in exactly this sequence.
#[tokio::test]
async fn a_dead_holder_is_still_reported_as_taken_over_by_the_winner() {
    let session_id = "lease-dead-holder";
    let store = new_store();
    let dead = owner("worker-a", "worker-a:boot-1");
    let sweeper = owner("worker-b", "worker-b:boot-1");

    // A row left behind by a worker that is already gone: held by a named owner,
    // already lapsed, and with no renewal task anywhere in this process. Claiming
    // it through the store rather than a guard is the point, because a dead
    // holder has no guard and emits nothing at all.
    let dead_generation = store
        .try_claim_session_execution_lease(session_id, &dead, 0)
        .await
        .expect("seed the abandoned row")
        .acquired()
        .expect("an unheld lane is acquirable")
        .fencing_token;

    let ((), capture) = capturing(|| async {
        let sweeper_guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            session_id,
            &sweeper,
            LeaseTimings::default(),
            Arc::new(SystemClock),
        )
        .await
        .expect("sweep the lapsed lane")
        .expect("a lapsed lane is acquirable");
        assert!(sweeper_guard.fence().fencing_token > dead_generation);
        sweeper_guard
            .release_if_live()
            .await
            .expect("release the swept lane");
    })
    .await;

    assert!(
        capture.named("session_execution_lease.lost").is_empty(),
        "a dead holder runs no renewal, so it reports nothing: {:?}",
        capture.events.lock().expect("lock captured events")
    );
    let taken_over = capture.exactly_one("session_execution_lease.taken_over");
    assert_eq!(taken_over.level, "INFO");
    assert_eq!(taken_over.field("session_id"), session_id);
    assert_eq!(
        taken_over.field("owner_id"),
        "worker-b",
        "the winner is the emitter, so the event's own identity fields are its"
    );
    assert_eq!(taken_over.field("incarnation_id"), "worker-b:boot-1");
    assert_eq!(taken_over.field("displaced_owner_id"), "worker-a");
    assert_eq!(
        taken_over.field("displaced_incarnation_id"),
        "worker-a:boot-1"
    );
    let winner: u64 = taken_over
        .field("fencing_token")
        .parse()
        .expect("winner generation is numeric");
    let displaced: u64 = taken_over
        .field("displaced_fencing_token")
        .parse()
        .expect("displaced generation is numeric");
    assert!(
        winner > displaced,
        "the takeover must order the two generations: {displaced} -> {winner}"
    );
}

/// A holder that releases its lane cleanly has not been taken over, so the next
/// claimant must stay silent about it. Otherwise every ordinary commit-and-reclaim
/// would look like a handoff and the event would be worthless for triage.
#[tokio::test]
async fn claiming_a_released_lane_reports_no_takeover() {
    let session_id = "lease-released-lane";
    let store = new_store();

    let ((), capture) = capturing(|| async {
        let first = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            session_id,
            &owner("worker-a", "worker-a:boot-1"),
            LeaseTimings::default(),
            Arc::new(SystemClock),
        )
        .await
        .expect("claim the lane")
        .expect("an unheld lane is acquirable");
        first.release_if_live().await.expect("release the lane");

        let second = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            session_id,
            &owner("worker-b", "worker-b:boot-1"),
            LeaseTimings::default(),
            Arc::new(SystemClock),
        )
        .await
        .expect("claim the released lane")
        .expect("a released lane is acquirable");
        second.release_if_live().await.expect("release again");
    })
    .await;

    assert_eq!(
        capture.named("session_execution_lease.acquired").len(),
        2,
        "both claims report themselves"
    );
    assert!(
        capture
            .named("session_execution_lease.taken_over")
            .is_empty(),
        "a cleanly released lane is handed over, not taken over: {:?}",
        capture.events.lock().expect("lock captured events")
    );
}

#[tokio::test]
async fn a_live_holder_that_is_swept_reports_only_its_own_renewal_failure() {
    let session_id = "lease-takeover";
    let store = new_store();
    let holder = owner("worker-a", "worker-a:boot-1");
    let successor = owner("worker-b", "worker-b:boot-1");

    let ((), capture) = capturing(|| async {
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            session_id,
            &holder,
            short_timings(),
            Arc::new(SystemClock),
        )
        .await
        .expect("claim the lane")
        .expect("an unheld lane is acquirable");
        let held_generation = guard.fence().fencing_token;

        // Move the durable row on behind the holder's back, the shape a stalled
        // holder sees when its TTL lapsed and a peer swept the lane. Releasing
        // through the store (not the guard) leaves the holder's renewal loop
        // running against a generation it no longer owns.
        store
            .release_session_execution_lease(&guard.completion())
            .await
            .expect("clear the row without notifying the holder");
        // The peer claims through a guard, because the winner is what emits the
        // takeover. Its claim displaced nobody: the row was released above, so
        // this deliberately produces `claimed` without `taken_over`.
        let successor_guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            session_id,
            &successor,
            short_timings(),
            Arc::new(SystemClock),
        )
        .await
        .expect("peer claim after the lane was swept")
        .expect("a swept lane is claimable");
        assert!(successor_guard.fence().fencing_token > held_generation);

        // Drive the holder's renewal loop far enough to observe the handoff.
        for _ in 0..40 {
            if guard.is_lost() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            guard.is_lost(),
            "the displaced holder must observe its own lease loss"
        );
        // Keep both guards alive until the assertions above have run.
        drop(guard);
        successor_guard
            .release_if_live()
            .await
            .expect("release the successor lane");
    })
    .await;

    // The loser's event is purely local: this runner lost the lane. It names no
    // successor, because at renewal-failure time it cannot know who took it, and
    // guessing is what produced the wrong-successor defect this test guards.
    let lease_lost = capture.exactly_one("session_execution_lease.lost");
    assert_eq!(lease_lost.level, "WARN");
    assert_eq!(lease_lost.field("session_id"), session_id);
    assert_eq!(lease_lost.field("owner_id"), "worker-a");
    assert_eq!(lease_lost.field("incarnation_id"), "worker-a:boot-1");
    assert_eq!(
        lease_lost.field("fencing_token"),
        held_generation_of(&capture),
        "the lost event names the fencing token this runner held"
    );
    for absent in ["superseding_owner_id", "displaced_owner_id"] {
        assert!(
            !lease_lost.fields.contains_key(absent),
            "the lost event must not claim to know who took the lane: {lease_lost:?}"
        );
    }
    assert!(
        capture
            .named("session_execution_lease.taken_over")
            .is_empty(),
        "the successor claimed a released row, so it displaced nobody: {:?}",
        capture.events.lock().expect("lock captured events")
    );
}

/// The generation reported by the first `claimed` event in a capture.
fn held_generation_of(capture: &EventCapture) -> String {
    capture
        .named("session_execution_lease.acquired")
        .first()
        .expect("a claim was captured")
        .field("fencing_token")
        .to_string()
}

#[tokio::test]
async fn a_transient_renewal_error_neither_loses_the_lane_nor_reports_a_takeover() {
    let session_id = "lease-transient";
    let store = Arc::new(crate::runtime::InMemorySessionStore::new());
    let holder = owner("worker-a", "worker-a:boot-1");

    let ((), capture) = capturing(|| async {
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store) as Arc<dyn crate::store::RuntimePersistence>,
            session_id,
            &holder,
            short_timings(),
            Arc::new(SystemClock),
        )
        .await
        .expect("claim the lane")
        .expect("an unheld lane is acquirable");
        store.fail_next_session_execution_lease_renewal();
        // The injected failure is a backend error, not a fence rejection, so the
        // renewal loop stops but the lease is still ours to release. Give the loop
        // enough intervals to have run and failed.
        tokio::time::sleep(TRANSIENT_SETTLE).await;
        assert!(
            !guard.is_lost(),
            "a transient renewal error must not mark the lane lost"
        );
        guard.release_if_live().await.expect("release the lane");
    })
    .await;

    assert_eq!(
        capture
            .named("session_execution_lease.renewal_failed")
            .len(),
        1,
        "a transient renewal error reports itself once"
    );
    assert!(
        capture.named("session_execution_lease.lost").is_empty(),
        "a transient error is not a lease loss: {:?}",
        capture.events.lock().expect("lock captured events")
    );
    assert!(
        capture
            .named("session_execution_lease.taken_over")
            .is_empty(),
        "a transient renewal error is not a handoff, and no claim happened: {:?}",
        capture.events.lock().expect("lock captured events")
    );
}

/// A writer that met a busy lane and proceeded under the commit CAS anyway is a
/// normal, documented path (ADR 0029 makes the CAS the authority). When such a
/// writer loses the CAS, its rejection must still be attributable: an anonymous
/// WARN naming only head revisions cannot tell an operator who was writing.
///
/// Regression for the review probe that reached this branch and found
/// `generation`, `owner_id`, and `incarnation_id` all absent.
#[tokio::test]
async fn a_lane_less_writer_that_loses_the_cas_is_still_attributable() {
    let session_id = "lease-busy-advisory";
    let store = new_store();
    let holder = owner("worker-a", "worker-a:boot-1");
    let claimant = owner("worker-b", "worker-b:boot-1");

    // A live foreign holder, so the claimant below has no lane of its own.
    let _held = store
        .try_claim_session_execution_lease(session_id, &holder, 60_000)
        .await
        .expect("claim the lane")
        .acquired()
        .expect("an unheld lane is acquirable");

    let ((), capture) = capturing(|| async {
        // The lane is busy, so this writer has no guard at all.
        assert!(
            SessionExecutionLeaseGuard::try_acquire(
                Arc::clone(&store),
                session_id,
                &claimant,
                LeaseTimings::default(),
                Arc::new(SystemClock),
            )
            .await
            .expect("probe the busy lane")
            .is_none(),
            "a live foreign holder must reject the claim"
        );
        trace_commit_cas_rejected(
            session_id,
            None,
            &claimant,
            &StoreError::HeadRevisionConflict {
                expected: 3,
                actual: 4,
            },
        );
    })
    .await;

    let rejected = capture.exactly_one("session_execution_lease.commit_cas_rejected");
    assert_eq!(rejected.level, "WARN");
    assert_eq!(rejected.field("session_id"), session_id);
    assert_eq!(
        rejected.field("owner_id"),
        "worker-b",
        "the event must name the writer, not the holder it raced"
    );
    assert_eq!(rejected.field("incarnation_id"), "worker-b:boot-1");
    assert!(
        !rejected.fields.contains_key("fencing_token"),
        "a lane-less writer held no generation, so it must not claim one: {rejected:?}"
    );
    assert_eq!(
        rejected.field("lane_held"),
        "false",
        "lane_held is what says the generation is not this writer's own"
    );
    assert_eq!(
        rejected.field("lease_lost"),
        "false",
        "a writer that never held the lane cannot have lost it"
    );
    assert_eq!(rejected.field("expected_head_revision"), "3");
    assert_eq!(rejected.field("actual_head_revision"), "4");
}

#[tokio::test]
async fn a_rejected_commit_cas_traces_the_losing_generation_and_head_revisions() {
    let session_id = "lease-commit-cas";
    let store = new_store();
    let holder = owner("worker-a", "worker-a:boot-1");

    let ((), capture) = capturing(|| async {
        let guard = SessionExecutionLeaseGuard::try_acquire(
            Arc::clone(&store),
            session_id,
            &holder,
            LeaseTimings::default(),
            Arc::new(SystemClock),
        )
        .await
        .expect("claim the lane")
        .expect("an unheld lane is acquirable");
        // A live holder whose commit still loses the head CAS: the advisory
        // lease says nothing about publication, and this is the livelock shape.
        trace_commit_cas_rejected(
            session_id,
            Some(&guard.commit_evidence()),
            &holder,
            &StoreError::HeadRevisionConflict {
                expected: 7,
                actual: 9,
            },
        );
        // A non-CAS store failure has its own error path and must stay silent
        // on this event.
        trace_commit_cas_rejected(
            session_id,
            Some(&guard.commit_evidence()),
            &holder,
            &StoreError::Backend("unrelated backend failure".to_string()),
        );
        guard.release_if_live().await.expect("release the lane");
    })
    .await;

    let rejected = capture.exactly_one("session_execution_lease.commit_cas_rejected");
    assert_eq!(rejected.level, "WARN");
    assert_eq!(rejected.field("session_id"), session_id);
    assert_eq!(rejected.field("owner_id"), "worker-a");
    assert_eq!(rejected.field("incarnation_id"), "worker-a:boot-1");
    assert_eq!(rejected.field("expected_head_revision"), "7");
    assert_eq!(rejected.field("actual_head_revision"), "9");
    assert_eq!(
        rejected.field("lease_lost"),
        "false",
        "a rejection while the lane is still held is livelock, not a handoff"
    );
    assert_eq!(
        rejected.field("lane_held"),
        "true",
        "this writer held the lane, so the generation is its own"
    );
    assert!(!rejected.field("fencing_token").is_empty());
}

#[test]
fn probe_runtime_size() {
    eprintln!(
        "LashRuntime={} Guard={} Evidence={} Acquisition={}",
        std::mem::size_of::<crate::runtime::LashRuntime>(),
        std::mem::size_of::<SessionExecutionLeaseGuard>(),
        std::mem::size_of::<SessionExecutionLeaseCommitEvidence>(),
        std::mem::size_of::<crate::store::SessionExecutionLeaseAcquisition>(),
    );
}
