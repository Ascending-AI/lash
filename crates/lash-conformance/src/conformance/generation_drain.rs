//! The build-generation drain's in-flight-turn law (FIG-3884, FIG-3927 N8):
//! a generation's drain work count is exactly its unfinished roots —
//! input-headed and queued-headed alike, each admitted under the generation
//! its drive stamped `admitted_generation` (FIG-3795 S9) — and the composed
//! drain status cannot report drained while one stands.

use crate::conformance::DeploymentViewExt as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use lash_sansio::{SessionId, TurnId};

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

/// The turn-lane family an admitted root is headed by.
#[derive(Clone, Copy)]
enum Head {
    Input,
    Batch,
}

/// An admitted root's live evidence: its session's store, the lease it was
/// admitted under, and the admission itself.
struct AdmittedRoot {
    session_id: SessionId,
    root: TurnId,
    store: Arc<dyn crate::store::RuntimeStore>,
    lease: crate::store::DriveFence,
    admission: crate::store::RootAdmission,
}

#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
impl AdmittedRoot {
    /// Admit a root headed by one fresh row of `head` under `stamp` in a
    /// fresh session of the fixture.
    async fn admit(
        fixture: &GenerationDrainLawFixture,
        name: &str,
        head: Head,
        stamp: &crate::engine::BuildGeneration,
    ) -> Self {
        let session_id = SessionId::from(format!("{}-{name}", fixture.prefix));
        let store = fixture
            .stores
            .session_store_factory()
            .admit_view(&crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                config: crate::SessionPolicy::new(crate::TurnBudget::Unbounded).into(),
                head: crate::SessionCreationHead::CommittedByCreator,
            })
            .await
            .expect("create the law's session");
        let head = match head {
            Head::Input => crate::store::AdmittedHead::Input(
                store
                    .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                        &session_id,
                        crate::TurnInputIngress::NextTurn,
                        crate::TurnInput::text(name),
                    ))
                    .await
                    .expect("enqueue the head input")
                    .input_id,
            ),
            Head::Batch => crate::store::AdmittedHead::Batch(
                store
                    .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
                        &session_id,
                        name,
                        1,
                        name,
                        crate::DeliveryPolicy::EarliestSafeBoundary,
                    ))
                    .await
                    .expect("enqueue the head batch")
                    .batch_id,
            ),
        };
        let lease = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
            store.store(),
            &session_id,
            &format!("{}-{name}", fixture.prefix),
        )
        .await;
        let admission = store
            .admit_root(&crate::store::AdmitRootRequest {
                fence: lease.clone(),
                root: TurnId::from(name),
                head,
                max_inputs: 64,
                policy: lash_core::testing::queued_work_admission_policy(64),
                base: crate::store::SessionHeadRef {
                    generation: 0,
                    revision: 0,
                    leaf: None,
                    checkpoint: None,
                },
                turn_index: 1,
                generation: None,
                admitted_generation: stamp.clone(),
            })
            .await
            .expect("admit the root")
            .expect("the root reaches its head");
        Self {
            session_id,
            root: TurnId::from(name),
            store: Arc::clone(store.store()),
            lease,
            admission,
        }
    }

    /// End the root with the commit of its first physical turn, which
    /// settles the rows it was admitted with and writes its terminal.
    async fn end(&self) {
        let mut state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        let root = self.root.clone();
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.drive_fence = Some(Box::new(self.lease.clone()));
        commit.root_terminal = Some(Box::new(crate::store::RootTerminalWrite {
            commit: crate::store::TurnCommitId::new(root.clone(), 0),
            turn: lash_core::store::PhysicalTurn::derive_turn_id(&root, 0),
            root: root.clone(),
            stop: None,
        }));
        commit.ingress = Some(super::completing_admission(root.as_str(), &self.admission));
        self.store
            .commit_runtime_state(commit)
            .await
            .expect("the root's final commit lands");
        assert_eq!(
            self.store
                .unfinished_root(&self.session_id)
                .await
                .expect("read the unfinished root"),
            None,
            "the final commit ends the root"
        );
    }
}

/// N8: `a` admits the input-headed `ia` and the queued-headed `qa`, which
/// stay unfinished, and `qb`, which ends before the read; `b` admits `qc`,
/// which stays unfinished; `never` admits nothing. The in-flight count `a`
/// reports is `ia` and `qa` — an ended root and another generation's root are
/// not `a`'s in-flight turns — and `a`'s composed drain status holds until
/// both end.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn in_flight_turns_follow_their_admitting_generation(fixture: GenerationDrainLawFixture) {
    let drain = fixture.stores.generation_drain();
    let (a, b, never) = (
        generation(&fixture.prefix, "a"),
        generation(&fixture.prefix, "b"),
        generation(&fixture.prefix, "never-admitted"),
    );

    let ia = AdmittedRoot::admit(&fixture, "ia", Head::Input, &a).await;
    let qa = AdmittedRoot::admit(&fixture, "qa", Head::Batch, &a).await;
    let qb = AdmittedRoot::admit(&fixture, "qb", Head::Batch, &a).await;
    let _qc = AdmittedRoot::admit(&fixture, "qc", Head::Batch, &b).await;
    qb.end().await;

    for (stamp, expected) in [(&a, 2), (&b, 1), (&never, 0)] {
        let work = drain.generation_work(stamp).await.expect("count the work");
        assert_eq!(
            work.in_flight_turns,
            expected,
            "generation {} holds {expected} in-flight turns",
            stamp.as_str(),
        );
    }

    // The composed status carries the count and holds the drain open for it:
    // marked and otherwise empty, `a` is not drained while `ia` or `qa`
    // stands.
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
    assert_eq!(held.in_flight_turns, 2);
    assert!(held.draining_since_ms.is_some());
    assert!(
        !held.drained(),
        "a's unfinished roots hold its drain open: {held:?}"
    );

    ia.end().await;
    let queued_only = drain.generation_work(&a).await.expect("count the work");
    assert_eq!(
        queued_only.in_flight_turns, 1,
        "a's queued-headed root is still in flight"
    );
    qa.end().await;
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
        "nothing of a's stands once ia and qa end: {emptied:?}"
    );
}
