//! The four decisive session-execution-lease transitions must each emit one
//! structured trace event carrying the session id, the lease generation, and the
//! holder identity — enough for a log timeline to reconstruct takeover order.
//!
//! These are trace events, never durable session events (lease churn is
//! per-attempt telemetry, not session history), so the oracle here is a capture
//! layer over the `tracing` dispatcher rather than an event sink.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime::session_execution_lease::{
    SessionExecutionLeaseGuard, trace_commit_cas_rejected,
};
use crate::store::StoreError;
use crate::{LeaseOwnerIdentity, LeaseTimings, SystemClock};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedEvent {
    level: String,
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> &str {
        self.fields
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("event is missing field `{name}`: {self:?}"))
    }
}

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl EventCapture {
    /// Every captured event whose `event` field names this lease transition.
    fn named(&self, event: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("lock captured events")
            .iter()
            .filter(|captured| captured.fields.get("event").map(String::as_str) == Some(event))
            .cloned()
            .collect()
    }

    fn exactly_one(&self, event: &str) -> CapturedEvent {
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
async fn capturing<F, Fut, T>(body: F) -> (T, EventCapture)
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

    let claimed = capture.exactly_one("session_execution_lease.claimed");
    assert_eq!(claimed.level, "INFO");
    assert_eq!(claimed.field("session_id"), "lease-observability");
    assert_eq!(claimed.field("owner_id"), "worker-a");
    assert_eq!(claimed.field("incarnation_id"), "worker-a:boot-1");
    assert_eq!(
        claimed.field("generation"),
        guard.fence().fencing_token.to_string(),
        "the traced generation must be the fence the claim returned"
    );
    guard
        .release_if_live()
        .await
        .expect("release the claimed lane");
}

#[tokio::test]
async fn a_takeover_traces_renew_failure_then_the_superseding_holder() {
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

        // Move the durable row on behind the holder's back — the shape a stalled
        // holder sees when its TTL lapsed and a peer swept the lane. Releasing
        // through the store (not the guard) leaves the holder's renewal loop
        // running against a generation it no longer owns.
        store
            .release_session_execution_lease(&guard.completion())
            .await
            .expect("clear the row without notifying the holder");
        let taken = store
            .try_claim_session_execution_lease(session_id, &successor, 60_000)
            .await
            .expect("peer claim after the lane was swept")
            .acquired()
            .expect("a swept lane is claimable");
        assert!(taken.fencing_token > held_generation);

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
        // Keep the guard alive until the assertions above have run.
        drop(guard);
    })
    .await;

    let renew_failed = capture.exactly_one("session_execution_lease.renew_failed");
    assert_eq!(renew_failed.level, "WARN");
    assert_eq!(renew_failed.field("session_id"), session_id);
    assert_eq!(renew_failed.field("owner_id"), "worker-a");
    assert_eq!(renew_failed.field("incarnation_id"), "worker-a:boot-1");
    assert!(!renew_failed.field("generation").is_empty());

    let taken_over = capture.exactly_one("session_execution_lease.taken_over");
    assert_eq!(taken_over.level, "INFO");
    assert_eq!(taken_over.field("session_id"), session_id);
    assert_eq!(taken_over.field("owner_id"), "worker-a");
    assert_eq!(taken_over.field("superseding_owner_id"), "worker-b");
    assert_eq!(
        taken_over.field("superseding_incarnation_id"),
        "worker-b:boot-1"
    );
    let displaced: u64 = taken_over
        .field("generation")
        .parse()
        .expect("displaced generation is numeric");
    let superseding: u64 = taken_over
        .field("superseding_generation")
        .parse()
        .expect("superseding generation is numeric");
    assert!(
        superseding > displaced,
        "the takeover event must order the two generations: {displaced} -> {superseding}"
    );
    assert_eq!(
        renew_failed.field("generation"),
        displaced.to_string(),
        "renew_failed and taken_over must name the same displaced generation"
    );
}

#[tokio::test]
async fn a_transient_renewal_error_is_not_reported_as_a_takeover() {
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
        for _ in 0..40 {
            if guard.is_lost() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            guard.is_lost(),
            "an injected renewal failure loses the lane"
        );
        drop(guard);
    })
    .await;

    assert_eq!(
        capture.named("session_execution_lease.renew_failed").len(),
        1,
        "a rejected renewal always reports itself"
    );
    assert!(
        capture
            .named("session_execution_lease.taken_over")
            .is_empty(),
        "a renewal that failed while this owner still holds the row is not a takeover: {:?}",
        capture.events.lock().expect("lock captured events")
    );
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
    assert!(!rejected.field("generation").is_empty());
}
