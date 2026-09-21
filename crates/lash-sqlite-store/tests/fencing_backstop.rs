//! FIG-3381: the fencing backstop's two obligations, asserted together.
//!
//! When a conditional write loses after its shared verdict said yes, the store
//! must do exactly two things:
//!
//! 1. **Fail closed with the site's own domain refusal.** A lost lease still
//!    reads as a lost lease to its caller, so the runtime's stand-down handling
//!    is unchanged. ADR 0053's conformance law owns that half and passes
//!    unedited.
//! 2. **Record the disagreement as evidence** — an error-level event naming the
//!    decision, the backend, the row and the rows affected — because the locked
//!    read and the statement's predicate disagreeing about one locked row is a
//!    store defect that must not vanish into a routine refusal.
//!
//! This binary owns the second half and re-asserts the first beside it, so
//! neither can be removed without a failure. It is its own test target because
//! the evidence is emitted on the `tokio-rusqlite` worker thread, which a
//! thread-local capture subscriber never sees; a process-global dispatcher is
//! the only way to observe it, and a global dispatcher may be installed once
//! per process.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use lash_core::store_backend_support::{FENCED_WRITE_DISAGREEMENT_EVENT, FENCING_TRACE_TARGET};
use lash_core::{LeaseOwnerIdentity, QueuedWorkStore, SessionExecutionLeaseStore, StoreError};
use lash_sansio::SessionId;
use lash_sqlite_store::Store;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

// ---------------------------------------------------------------------------
// A process-global capture of the fencing target.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct CapturedEvent {
    level: String,
    target: String,
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> &str {
        self.fields
            .get(name)
            .unwrap_or_else(|| panic!("event is missing field `{name}`: {self:?}"))
    }
}

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl EventCapture {
    /// Every captured disagreement recorded against one row identity.
    ///
    /// Keyed by row so tests in this binary, which share one global
    /// dispatcher, never read each other's evidence.
    fn disagreements_for(&self, row_identity: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|event| {
                event.fields.get("event").map(String::as_str)
                    == Some(FENCED_WRITE_DISAGREEMENT_EVENT)
                    && event.fields.get("row_identity").map(String::as_str) == Some(row_identity)
            })
            .cloned()
            .collect()
    }
}

struct FieldVisitor(BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S: tracing::Subscriber> Layer<S> for EventCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        if event.metadata().target() != FENCING_TRACE_TARGET {
            return;
        }
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedEvent {
                level: event.metadata().level().to_string(),
                target: event.metadata().target().to_string(),
                fields: visitor.0,
            });
    }
}

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn capture() -> EventCapture {
    static CAPTURE: OnceLock<EventCapture> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = EventCapture::default();
            tracing::subscriber::set_global_default(Registry::default().with(capture.clone()))
                .expect("install the fencing capture dispatcher once");
            capture
        })
        .clone()
}

// ---------------------------------------------------------------------------
// The seam: a trigger that silently drops the renewal write.
// ---------------------------------------------------------------------------

/// Suppress the renewal `UPDATE` for one session, so it affects no row after
/// the shared verdict has already authorized it.
///
/// This is the same seam the ADR 0053 conformance law uses, spelled here so
/// this binary can assert the evidence half without touching that law.
#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn suppress_lease_renewal(path: &Path, session_id: &str) {
    rusqlite::Connection::open(path)
        .expect("open the renewal-suppression connection")
        .execute_batch(&format!(
            "CREATE TRIGGER lash_test_fencing_backstop
             BEFORE UPDATE OF lease_expires_at_ms ON session_execution_leases
             WHEN OLD.session_id = '{session_id}'
             BEGIN
                 SELECT RAISE(IGNORE);
             END;"
        ))
        .expect("arm the renewal-suppression trigger");
}

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn restore_lease_renewal(path: &Path) {
    rusqlite::Connection::open(path)
        .expect("open the renewal-restore connection")
        .execute_batch("DROP TRIGGER lash_test_fencing_backstop;")
        .expect("disarm the renewal-suppression trigger");
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_fenced_write_fails_closed_and_records_the_disagreement() {
    let capture = capture();
    let dir = tempfile::tempdir().expect("fencing backstop tempdir");
    let path = dir.path().join("fencing-backstop.db");
    let store = Store::open(&path)
        .await
        .expect("open fencing backstop store");
    let session_id = SessionId::from("fencing-backstop-lost-write");
    let owner = LeaseOwnerIdentity::opaque("backstop-owner", "backstop-incarnation");
    let held = store
        .try_claim_session_execution_lease(&session_id, &owner, "backstop-executor", 120_000)
        .await
        .expect("claim the backstop lease")
        .acquired()
        .expect("the backstop lease is acquired");

    suppress_lease_renewal(&path, session_id.as_str());
    let refusal = store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await;
    restore_lease_renewal(&path);

    // Obligation one: the caller receives exactly what it always received.
    assert!(
        matches!(
            refusal,
            Err(StoreError::SessionExecutionLeaseRenewalRefused { ref session_id })
                if session_id == "fencing-backstop-lost-write"
        ),
        "a lost fenced write must fail closed with this site's own refusal, got {refusal:?}"
    );

    // Obligation two: the disagreement is recorded, with the evidence an
    // operator needs to locate it.
    let recorded = capture.disagreements_for(session_id.as_str());
    assert_eq!(
        recorded.len(),
        1,
        "exactly one disagreement must be recorded, got {recorded:?}"
    );
    let event = &recorded[0];
    assert_eq!(event.level, "ERROR", "a store defect is not a warning");
    assert_eq!(event.target, FENCING_TRACE_TARGET);
    assert_eq!(event.field("fenced_write"), "session_execution_lease.renew");
    assert_eq!(event.field("backend"), "sqlite");
    assert_eq!(event.field("row_identity"), session_id.as_str());
    assert_eq!(event.field("rows_affected"), "0");
    assert_eq!(event.field("outcome"), "fenced_write_lost");

    // Failing closed means nothing was published: the lease is untouched.
    let durable = store
        .get_session_execution_lease(&session_id)
        .await
        .expect("read the lease after the refused renewal")
        .lease
        .expect("a refused renewal preserves the current lease");
    assert_eq!(durable.lease_token, held.lease_token);
    assert_eq!(durable.fencing_token, held.fencing_token);
    assert_eq!(durable.expires_at_epoch_ms, held.expires_at_epoch_ms);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewal_the_verdict_refuses_never_reaches_the_write() {
    // The companion to the law above, and the reason the backstop is a
    // backstop: when the verdict refuses, no write is attempted, so no
    // disagreement exists to record. An empty capture here is what proves the
    // verdict — not rows-affected — decided the refusal.
    let capture = capture();
    let dir = tempfile::tempdir().expect("verdict-first tempdir");
    let path = dir.path().join("verdict-first.db");
    let store = Store::open(&path).await.expect("open verdict-first store");
    let session_id = SessionId::from("fencing-backstop-verdict-first");
    let owner = LeaseOwnerIdentity::opaque("verdict-owner", "verdict-incarnation");
    let held = store
        .try_claim_session_execution_lease(&session_id, &owner, "verdict-executor", 120_000)
        .await
        .expect("claim the verdict-first lease")
        .acquired()
        .expect("the verdict-first lease is acquired");

    let mut stale = held.fence();
    stale.lease_token = "a-token-this-row-never-carried".to_string();
    let refusal = store.renew_session_execution_lease(&stale, 120_000).await;

    assert!(
        matches!(
            refusal,
            Err(StoreError::SessionExecutionLeaseRenewalRefused { ref session_id })
                if session_id == "fencing-backstop-verdict-first"
        ),
        "a stale lease token must be refused by the verdict, got {refusal:?}"
    );
    assert!(
        capture.disagreements_for(session_id.as_str()).is_empty(),
        "a verdict-refused renewal must never reach the write, so nothing disagrees",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewing_a_lapsed_lease_is_refused_as_expired_not_renewed() {
    // The renewal statement's five-column predicate names owner, executor and
    // lease token and says nothing about expiry, so this answer exists only
    // because the shared verdict compares `expires_at` against the `now` the
    // store sampled. Remove the verdict call and a lapsed holder renews itself
    // back to life; that is the whole point of deciding in one place.
    let dir = tempfile::tempdir().expect("lapsed renewal tempdir");
    let path = dir.path().join("lapsed-renewal.db");
    let store = Store::open(&path).await.expect("open lapsed renewal store");
    let session_id = SessionId::from("fencing-backstop-lapsed-renewal");
    let owner = LeaseOwnerIdentity::opaque("lapsed-owner", "lapsed-incarnation");
    let held = store
        .try_claim_session_execution_lease(&session_id, &owner, "lapsed-executor", 1)
        .await
        .expect("claim the lapsing lease")
        .acquired()
        .expect("the lapsing lease is acquired");

    // One-sided wait: a slow machine only lapses the lease harder.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let refusal = store
        .renew_session_execution_lease(&held.fence(), 120_000)
        .await;
    assert!(
        matches!(
            refusal,
            Err(StoreError::SessionExecutionLeaseExpired { ref session_id })
                if session_id == "fencing-backstop-lapsed-renewal"
        ),
        "a lapsed holder must be told its lease expired, not handed a fresh term, got {refusal:?}"
    );
}

// ---------------------------------------------------------------------------
// FIG-3383: the same two obligations over the queued-work claim.
// ---------------------------------------------------------------------------

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn suppress_queued_work_claim(path: &Path, batch_id: &str) {
    let batch_id = batch_id.replace('\'', "''");
    rusqlite::Connection::open(path)
        .expect("open the claim-suppression connection")
        .execute_batch(&format!(
            "CREATE TRIGGER lash_test_queued_claim_backstop
             BEFORE UPDATE OF claim_token ON queued_work_batches
             WHEN OLD.batch_id = '{batch_id}'
             BEGIN
                 SELECT RAISE(IGNORE);
             END;"
        ))
        .expect("arm the claim-suppression trigger");
}

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn restore_queued_work_claim(path: &Path) {
    rusqlite::Connection::open(path)
        .expect("open the claim-restore connection")
        .execute_batch("DROP TRIGGER lash_test_queued_claim_backstop;")
        .expect("disarm the claim-suppression trigger");
}

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
async fn enqueue_one(store: &Store, session_id: &SessionId) -> lash_core::BatchId {
    store
        .enqueue_queued_work(
            lash_core::runtime::QueuedWorkBatchDraft::new(
                session_id.as_str(),
                lash_core::runtime::DeliveryPolicy::EarliestSafeBoundary,
                lash_core::runtime::QueuedWorkBatchPayloads::from(
                    lash_core::runtime::TurnWorkPayload::agent_frame_task(
                        lash_core::facade_support::frame_node_id(session_id, "frame"),
                        "task",
                        None,
                    ),
                ),
            )
            .with_merge_key("backstop"),
        )
        .await
        .expect("enqueue the backstop batch")
        .batch_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_queued_work_claim_fails_closed_and_records_the_disagreement() {
    // The queued-work claim's half of the same contract (D5). The verdict
    // authorized this row over the locked read, so a write that changes no row
    // is a disagreement between the locked read and the statement's own
    // predicate: it is recorded as evidence, and the claim still reports
    // nothing claimed, which is what a lost claim race has always reported.
    let capture = capture();
    let dir = tempfile::tempdir().expect("queued claim backstop tempdir");
    let path = dir.path().join("queued-claim-backstop.db");
    let store = Store::open(&path)
        .await
        .expect("open queued claim backstop store");
    let session_id = SessionId::from("queued-claim-backstop-lost-write");
    let owner = LeaseOwnerIdentity::opaque("queued-owner", "queued-incarnation");
    let batch_id = enqueue_one(&store, &session_id).await;
    let lease = store
        .try_claim_session_execution_lease(&session_id, &owner, "queued-executor", 120_000)
        .await
        .expect("claim the queued backstop lease")
        .acquired()
        .expect("the queued backstop lease is acquired");

    suppress_queued_work_claim(&path, batch_id.as_str());
    let outcome = store
        .claim_ready_queued_work(
            &session_id,
            &lease.fence(),
            &owner,
            lash_core::runtime::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("the claim call itself succeeds");
    restore_queued_work_claim(&path);

    // Obligation one: the caller receives exactly what it always received —
    // a lost race, not an error.
    assert!(
        matches!(
            outcome,
            lash_core::QueuedWorkClaimOutcome::Refused(_)
        ),
        "a lost fenced claim must report no claim, got {outcome:?}"
    );

    // Obligation two: the disagreement is recorded against the row.
    let recorded = capture.disagreements_for(batch_id.as_str());
    assert_eq!(
        recorded.len(),
        1,
        "exactly one disagreement must be recorded, got {recorded:?}"
    );
    let event = &recorded[0];
    assert_eq!(event.level, "ERROR", "a store defect is not a warning");
    assert_eq!(event.target, FENCING_TRACE_TARGET);
    assert_eq!(event.field("fenced_write"), "queued_work_claim.acquire");
    assert_eq!(event.field("backend"), "sqlite");
    assert_eq!(event.field("row_identity"), batch_id.as_str());
    assert_eq!(event.field("rows_affected"), "0");
    assert_eq!(event.field("outcome"), "fenced_write_lost");

    // Failing closed means nothing was published: the row is still claimable.
    let claimed = store
        .claim_ready_queued_work(
            &session_id,
            &lease.fence(),
            &owner,
            lash_core::runtime::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("the retried claim succeeds")
        .claim()
        .expect("the rolled-back row is still claimable");
    assert_eq!(claimed.data.batches.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_work_claim_the_verdict_refuses_never_reaches_the_write() {
    // The companion law: a generation that already holds a row may not claim
    // it again (ADR 0029), and refusing that is not a disagreement.
    //
    // Unlike the lease renewal above, the verdict and the statement's own
    // predicate ask exactly the same question here — the generation predicate
    // is also the candidate scan's `ORDER BY … LIMIT` filter, so it cannot
    // move into shared code — and a red-side mutation that deletes the verdict
    // call leaves this passing. What it does pin is that the refusal comes
    // from the scan and the verdict agreeing *before* the write, so an empty
    // capture is the evidence that no conditional write was attempted at all.
    let capture = capture();
    let dir = tempfile::tempdir().expect("queued verdict-first tempdir");
    let path = dir.path().join("queued-verdict-first.db");
    let store = Store::open(&path)
        .await
        .expect("open queued verdict-first store");
    let session_id = SessionId::from("queued-claim-backstop-verdict-first");
    let owner = LeaseOwnerIdentity::opaque("queued-verdict-owner", "queued-verdict-incarnation");
    let batch_id = enqueue_one(&store, &session_id).await;
    let lease = store
        .try_claim_session_execution_lease(&session_id, &owner, "queued-verdict-executor", 120_000)
        .await
        .expect("claim the queued verdict-first lease")
        .acquired()
        .expect("the queued verdict-first lease is acquired");

    assert!(
        store
            .claim_ready_queued_work(
                &session_id,
                &lease.fence(),
                &owner,
                lash_core::runtime::QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("the first claim succeeds")
            .claim()
            .is_some(),
        "the first claim of this generation takes the row",
    );
    let second = store
        .claim_ready_queued_work(
            &session_id,
            &lease.fence(),
            &owner,
            lash_core::runtime::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("the second claim call itself succeeds");

    assert!(
        matches!(
            second,
            lash_core::QueuedWorkClaimOutcome::Refused(_)
        ),
        "a generation must not claim a row it already holds, got {second:?}"
    );
    assert!(
        capture.disagreements_for(batch_id.as_str()).is_empty(),
        "a verdict-refused claim must never reach the write, so nothing disagrees",
    );
}
