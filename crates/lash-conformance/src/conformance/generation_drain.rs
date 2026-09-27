//! The build-generation drain's in-flight-turn law (FIG-3884): a
//! generation's drain work count is exactly its pending queued runs — each
//! admitted under the generation its drive stamped `admitted_generation`
//! (FIG-3795 S9) — and the composed drain status cannot report drained while
//! one stands.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use lash_sansio::SessionId;

/// What one drain law runs over: a backend's store set, fresh per law.
pub struct GenerationDrainLawFixture {
    pub stores: Arc<dyn crate::StoreSet>,
    /// Distinguishes this law's rows from every other law's on a shared
    /// database.
    pub prefix: String,
}

/// `alias` under `prefix` as a generation distinct from every other law's: a
/// durable catalog outlives the test that wrote to it.
fn generation(prefix: &str, alias: &str) -> crate::engine::BuildGeneration {
    let mut hasher = DefaultHasher::new();
    ("fig-3884-in-flight", prefix, alias).hash(&mut hasher);
    let bytes = hasher.finish().to_be_bytes();
    let mut digest = [0_u8; 6];
    digest.copy_from_slice(&bytes[..6]);
    crate::engine::BuildGeneration::from_digest(digest)
}

/// The persisted configuration a queued-run admission carries.
fn queued_run_configuration(session_id: &SessionId) -> crate::PersistedSessionConfig {
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    crate::RuntimeCommit::persisted_state_for_test(&state, &[]).config
}

/// A queued run's live admission evidence: its session's store, the lease it
/// began under, its scope, and the admission itself.
struct QueuedRun {
    store: Arc<dyn crate::store::RuntimePersistence>,
    lease: crate::SessionExecutionLease,
    scope: crate::ExecutionScope,
    admission: crate::store::QueuedRunAdmission,
}

#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
impl QueuedRun {
    /// Admit a queued run under `stamp` in a fresh session of the fixture.
    async fn begin(
        fixture: &GenerationDrainLawFixture,
        name: &str,
        stamp: &crate::engine::BuildGeneration,
    ) -> Self {
        let session_id = SessionId::from(format!("{}-{name}", fixture.prefix));
        let store = fixture
            .stores
            .session_store_factory()
            .create_store(&crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            })
            .await
            .expect("create the law's session");
        let lease = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
            &store,
            &session_id,
            &format!("{}-{name}", fixture.prefix),
        )
        .await;
        let scope = crate::ExecutionScope::queue_drain(
            session_id.clone(),
            format!("{}-{name}", fixture.prefix),
        );
        let admission = store
            .begin_or_resume_queued_run(
                &lease.authority(),
                crate::store::BeginQueuedRun {
                    session_id: session_id.clone(),
                    identity: Some(scope.clone()),
                    request: crate::store::QueuedRunRequest::Automatic,
                    configuration: queued_run_configuration(&session_id),
                    expected_head_revision: 0,
                    initial_turn_index: 1,
                    generation: None,
                    admitted_generation: stamp.clone(),
                },
            )
            .await
            .expect("begin the queued run");
        Self {
            store,
            lease,
            scope,
            admission,
        }
    }

    /// Settle the run with an empty terminal through its own session's store
    /// and lease authority: the empty-selection freeze is the read that makes
    /// `Empty` a legal terminal.
    async fn settle(&self) {
        let selected = self
            .store
            .select_queued_run(
                &self.lease.authority(),
                &self.scope,
                &self.lease.owner,
                64,
                &self.admission.configuration,
                lash_core::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("freeze the empty selection");
        self.store
            .settle_queued_run(
                &self.lease.authority(),
                crate::store::QueuedRunCommit {
                    scope: self.scope.clone(),
                    expected_revision: selected.admission.revision,
                    progress: crate::store::QueuedRunProgress::Settle {
                        terminal: crate::store::QueuedRunTerminal::Empty,
                    },
                },
            )
            .await
            .expect("settle the queued run");
    }
}

/// `a` admits `qa`, which stays pending, and `qb`, which settles before the
/// read; `b` admits `qc`, which stays pending; `never` admits nothing. The
/// in-flight count `a` reports is `qa` alone — a settled run and another
/// generation's run are not `a`'s in-flight turns — and `a`'s composed drain
/// status holds until `qa` settles.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn in_flight_turns_follow_their_admitting_generation(fixture: GenerationDrainLawFixture) {
    let drain = fixture.stores.generation_drain();
    let (a, b, never) = (
        generation(&fixture.prefix, "a"),
        generation(&fixture.prefix, "b"),
        generation(&fixture.prefix, "never-admitted"),
    );

    let qa = QueuedRun::begin(&fixture, "qa", &a).await;
    let qb = QueuedRun::begin(&fixture, "qb", &a).await;
    let _qc = QueuedRun::begin(&fixture, "qc", &b).await;
    qb.settle().await;

    for (stamp, expected) in [(&a, 1), (&b, 1), (&never, 0)] {
        let work = drain.generation_work(stamp).await.expect("count the work");
        assert_eq!(
            work.in_flight_turns,
            expected,
            "generation {} holds {expected} in-flight turns",
            stamp.as_str(),
        );
    }

    // The composed status carries the count and holds the drain open for it:
    // marked and otherwise empty, `a` is not drained while `qa` stands.
    assert!(
        drain.mark_draining(&a, 1).await.expect("mark a draining"),
        "a was not marked before"
    );
    let held = crate::store::generation_drain::GenerationDrainStatus::collect(
        drain.as_ref(),
        fixture.stores.session_delete_ledger().as_ref(),
        |kind| fixture.stores.obligation_ledger(kind),
        &a,
        2,
    )
    .await
    .expect("compose a's status");
    assert_eq!(held.in_flight_turns, 1);
    assert!(held.draining_since_ms.is_some());
    assert!(
        !held.drained(),
        "a's pending run holds its drain open: {held:?}"
    );

    qa.settle().await;
    let emptied = crate::store::generation_drain::GenerationDrainStatus::collect(
        drain.as_ref(),
        fixture.stores.session_delete_ledger().as_ref(),
        |kind| fixture.stores.obligation_ledger(kind),
        &a,
        3,
    )
    .await
    .expect("compose a's emptied status");
    assert_eq!(emptied.in_flight_turns, 0);
    assert!(
        emptied.drained(),
        "nothing of a's stands once qa settles: {emptied:?}"
    );
}
