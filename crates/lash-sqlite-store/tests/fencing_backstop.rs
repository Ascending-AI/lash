//! The queued-work admission fencing backstop's two obligations, asserted
//! together.
//!
//! When a conditional write loses after its shared verdict said yes, the store
//! must do exactly two things:
//!
//! 1. **Fail closed with the site's own domain refusal.** A lost admission
//!    bind refuses the admission, and nothing it composed is bound.
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

use lash_core_execution::store::{CheckpointAdmission, RunStore as _};
use lash_core_execution::store_backend_support::{
    FENCED_WRITE_DISAGREEMENT_EVENT, FENCING_TRACE_TARGET,
};
use lash_core_execution::{QueuedWorkStore, StoreError, TurnId};
use lash_sansio::SessionId;
use lash_sqlite_store::SqliteStore;
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
// Queued-work admission fenced-write backstop.
// ---------------------------------------------------------------------------

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn suppress_queued_work_bind(path: &Path, batch_id: &str) {
    let batch_id = batch_id.replace('\'', "''");
    rusqlite::Connection::open(path)
        .expect("open the bind-suppression connection")
        .execute_batch(&format!(
            "CREATE TRIGGER lash_test_queued_bind_backstop
             BEFORE UPDATE OF admitted_run ON queued_work_batches
             WHEN OLD.batch_id = '{batch_id}'
             BEGIN
                 SELECT RAISE(IGNORE);
             END;"
        ))
        .expect("arm the bind-suppression trigger");
}

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
fn restore_queued_work_bind(path: &Path) {
    rusqlite::Connection::open(path)
        .expect("open the bind-restore connection")
        .execute_batch("DROP TRIGGER lash_test_queued_bind_backstop;")
        .expect("disarm the bind-suppression trigger");
}

fn backstop_wake(session_id: &SessionId) -> lash_core_execution::ProcessWakeDelivery {
    let process_id = || lash_core_execution::ProcessId::fixture("backstop-process");
    lash_core_execution::ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: "task".to_string(),
        created_at_ms: 1,
        trace_cause: Default::default(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test fixture wiring: a failure here is a broken fixture, and panicking names it"
)]
async fn enqueue_one(store: &SqliteStore, session_id: &SessionId) -> lash_core_execution::BatchId {
    store
        .enqueue_queued_work(
            lash_core_execution::runtime::process_wake_batch_draft(backstop_wake(session_id))
                .with_merge_key("backstop"),
        )
        .await
        .expect("enqueue the backstop batch")
        .batch_id
}

/// Admit the running run `run`'s rows at its after-work checkpoint.
async fn admit(
    store: &SqliteStore,
    session_id: &SessionId,
    run: &str,
) -> Result<CheckpointAdmission, StoreError> {
    let run = TurnId::fixture(run);
    store
        .admit_at_checkpoint(&lash_core_execution::store::CheckpointAdmissionRequest {
            session_id: session_id.clone(),
            run: run.clone(),
            turn_id: run,
            checkpoint: lash_core_execution::CheckpointKind::AfterWork,
            step: "backstop-checkpoint".to_string(),
            max_inputs: 0,
            policy: lash_core_execution::testing::queued_work_admission_policy(10),
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_admission_bind_fails_closed_and_records_the_disagreement() {
    // The composition read this row open in the admission's own write
    // transaction, so a bind that changes no row is a disagreement between
    // that read and the statement's own predicate: it is recorded as
    // evidence, and the admission is refused whole.
    let capture = capture();
    let dir = tempfile::tempdir().expect("admission backstop tempdir");
    let path = dir.path().join("admission-backstop.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open admission backstop store");
    let session_id = SessionId::from("admission-backstop-lost-write");
    let batch_id = enqueue_one(&store, &session_id).await;

    suppress_queued_work_bind(&path, batch_id.as_str());
    let outcome = admit(&store, &session_id, "backstop-run").await;
    restore_queued_work_bind(&path);

    // Obligation one: the admission fails closed.
    assert!(
        matches!(outcome, Err(StoreError::Contended)),
        "a lost admission bind must refuse the admission, got {outcome:?}"
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
    assert_eq!(event.field("fenced_write"), "ingress.admit");
    assert_eq!(event.field("backend"), "sqlite");
    assert_eq!(event.field("row_identity"), batch_id.as_str());
    assert_eq!(event.field("rows_affected"), "0");
    assert_eq!(event.field("outcome"), "fenced_write_lost");

    // Failing closed means nothing was published: the row is still admissible.
    let admitted = admit(&store, &session_id, "backstop-run")
        .await
        .expect("the retried admission succeeds")
        .queued
        .expect("the rolled-back row is still admissible");
    assert_eq!(admitted.batch_ids(), vec![batch_id]);
}
