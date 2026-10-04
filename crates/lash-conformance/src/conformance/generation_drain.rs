//! The build-generation drain's in-flight-turn law (FIG-3884, FIG-3927 N8):
//! a generation's drain work count is exactly its unfinished runs —
//! input-headed and queued-headed alike, each admitted under the generation
//! its shift stamped `admitted_generation` (FIG-3795 S9) — and the composed
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

/// The turn-lane family an admitted run is headed by.
#[derive(Clone, Copy)]
enum Head {
    Input,
    Batch,
}

/// An admitted run's live evidence: its session's store, the lease it was
/// admitted under, and the admission itself.
struct AdmittedRun {
    session_id: SessionId,
    run: TurnId,
    store: Arc<dyn crate::store::RuntimeStore>,
    lease: crate::store::ShiftFence,
    admission: crate::store::RunAdmission,
}

#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
impl AdmittedRun {
    /// Admit a run headed by one fresh row of `head` under `stamp` in a
    /// fresh session of the fixture.
    async fn admit(
        fixture: &GenerationDrainLawFixture,
        name: &str,
        head: Head,
        stamp: &crate::engine::BuildGeneration,
    ) -> Self {
        let session_id = SessionId::fixture(format!("{}-{name}", fixture.prefix));
        let store = fixture
            .stores
            .session_store_factory()
            .admit_view(&crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                config: crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
                .into(),
                head: crate::SessionCreationHead::Config,
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
        let lease = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
            store.store(),
            &session_id,
            &format!("{}-{name}", fixture.prefix),
        )
        .await;
        let admission = store
            .admit_run(&crate::store::AdmitRunRequest {
                unsealed_epoch: None,
                fence: lease.clone(),
                run: TurnId::fixture(name),
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
                admitted_generation: stamp.clone(),
                executor: crate::store::RunExecutor::run(&crate::store::AdmissionId::new(
                    "fixture#0",
                )),
                plugins: Default::default(),
                turn_cancellation: None,
                trace_scopes: std::sync::Arc::new(lash_core::UntracedScopes),
            })
            .await
            .expect("admit the run")
            .expect("the run reaches its head");
        Self {
            session_id,
            run: TurnId::fixture(name),
            store: Arc::clone(store.store()),
            lease,
            admission,
        }
    }

    /// Commit the run's first physical turn owing a follow-on: the rows the
    /// run was admitted with settle, and the run stays unfinished.
    async fn owe_follow_on(&self) -> crate::store::PendingFollowOn {
        let mut state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        state.ensure_agent_frame_initialized();
        let owed = crate::store::PendingFollowOn {
            continuation: None,
            follow_on_turn_id: lash_core::store::PhysicalTurn::derive_turn_id(&self.run, 1),
            frame_id: state
                .current_frame_node_id
                .clone()
                .expect("the initial frame is current"),
            task: "run the rest of the run".to_owned(),
            resolved_run: crate::conformance::helpers::default_resolved_run(),
            chain_depth: 1,
            attempts: 0,
        };
        // A follow-on is written only by a turn's terminal commit.
        let operation =
            crate::OperationId::turn(self.session_id.clone(), self.run.clone(), "final");
        let mut graph = state.pending_graph_commit();
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("derive commit node ids");
        let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
            &state, graph, operation,
        )
        .expect("build the commit");
        commit.shift_fence = Some(Box::new(self.lease.clone()));
        commit.pending_follow_on = Some(owed.clone());
        commit.ingress = Some(super::completing_admission(
            self.run.as_str(),
            &self.admission,
        ));
        self.store
            .commit_runtime_state(commit)
            .await
            .expect("the run's first turn commits owing its follow-on");
        owed
    }

    /// End the run with the commit of its first physical turn, which
    /// settles the rows it was admitted with and writes its terminal.
    async fn end(&self) {
        let mut state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        state.ensure_agent_frame_initialized();
        let run = self.run.clone();
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
        commit.shift_fence = Some(Box::new(self.lease.clone()));
        commit.run_terminal = Some(Box::new(crate::store::RunTerminalWrite {
            commit: crate::store::TurnCommitId::new(run.clone(), 0),
            turn: lash_core::store::PhysicalTurn::derive_turn_id(&run, 0),
            run: run.clone(),
            outcome: crate::store::RunCommittedOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage {
                    text: String::new(),
                },
            ),
        }));
        commit.ingress = Some(super::completing_admission(run.as_str(), &self.admission));
        self.store
            .commit_runtime_state(commit)
            .await
            .expect("the run's final commit lands");
        assert_eq!(
            self.store
                .unfinished_run(&self.session_id)
                .await
                .expect("read the unfinished run"),
            None,
            "the final commit ends the run"
        );
    }
}

/// N8: `a` admits the input-headed `ia` and the queued-headed `qa`, which
/// stay unfinished, and `qb`, which ends before the read; `b` admits `qc`,
/// which stays unfinished; `never` admits nothing. The in-flight count `a`
/// reports is `ia` and `qa` — an ended run and another generation's run are
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

    let ia = AdmittedRun::admit(&fixture, "ia", Head::Input, &a).await;
    let qa = AdmittedRun::admit(&fixture, "qa", Head::Batch, &a).await;
    let qb = AdmittedRun::admit(&fixture, "qb", Head::Batch, &a).await;
    let qc = AdmittedRun::admit(&fixture, "qc", Head::Batch, &b).await;
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

    // The sessions holding those turns, which the drain asks to hand over
    // (FIG-4739): each generation's, in session order, paged; an ended
    // run's session holds none.
    let one = std::num::NonZeroUsize::MIN;
    let mut paged = Vec::new();
    loop {
        let page = drain
            .sessions_in_flight(&a, paged.last(), one)
            .await
            .expect("page a's sessions in flight");
        let Some(last) = page.last().cloned() else {
            break;
        };
        assert_eq!(page.len(), 1, "a page holds at most its bound");
        paged.push(last);
    }
    let mut expected = vec![ia.session_id.clone(), qa.session_id.clone()];
    expected.sort();
    assert_eq!(paged, expected, "a's sessions in flight, in order");
    let whole = std::num::NonZeroUsize::new(16).expect("non-zero");
    assert_eq!(
        drain
            .sessions_in_flight(&b, None, whole)
            .await
            .expect("list b's sessions in flight"),
        vec![qc.session_id.clone()],
    );
    assert!(
        drain
            .sessions_in_flight(&never, None, whole)
            .await
            .expect("list a generation that admitted nothing")
            .is_empty()
    );

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
        &crate::store::fleet_finalize::NoDeployments,
        &a,
        2,
    )
    .await
    .expect("compose a's status");
    assert_eq!(held.in_flight_turns, 2);
    assert!(held.draining_since_ms.is_some());
    assert!(
        !held.drained(),
        "a's unfinished runs hold its drain open: {held:?}"
    );

    ia.end().await;
    let queued_only = drain.generation_work(&a).await.expect("count the work");
    assert_eq!(
        queued_only.in_flight_turns, 1,
        "a's queued-headed run is still in flight"
    );
    qa.end().await;
    let emptied = crate::store::generation_drain::GenerationDrainStatus::collect(
        drain.as_ref(),
        fixture.stores.session_delete_ledger().as_ref(),
        |kind| fixture.stores.obligation_ledger(kind),
        &crate::store::fleet_finalize::NoDeployments,
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

/// FIG-4739: a run whose turn committed owing a follow-on stays its
/// admitting generation's in-flight turn until a shift recovers the
/// follow-on; the recovery's raise moves the run to the generation of the
/// build that recovers it, which holds the run until it ends. A run that
/// owes nothing is not moved by another run's recovery.
#[expect(clippy::expect_used, reason = "conformance law: each step is asserted")]
pub async fn a_recovered_follow_on_moves_its_run_to_the_recovering_generation(
    fixture: GenerationDrainLawFixture,
) {
    let drain = fixture.stores.generation_drain();
    let (old, next) = (
        generation(&fixture.prefix, "handed-over-old"),
        generation(&fixture.prefix, "handed-over-next"),
    );
    let in_flight = |stamp: crate::engine::BuildGeneration| {
        let drain = Arc::clone(&drain);
        async move {
            drain
                .generation_work(&stamp)
                .await
                .expect("count the work")
                .in_flight_turns
        }
    };

    let handed = AdmittedRun::admit(&fixture, "handed", Head::Input, &old).await;
    let _staying = AdmittedRun::admit(&fixture, "staying", Head::Input, &old).await;
    let owed = handed.owe_follow_on().await;
    assert_eq!(
        (in_flight(old.clone()).await, in_flight(next.clone()).await),
        (2, 0),
        "a follow-on no shift has recovered is still its admitting generation's"
    );

    let recovering = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        &handed.store,
        &handed.session_id,
        &format!("{}-handed-recovering", fixture.prefix),
    )
    .await;
    let raised = handed
        .store
        .raise_pending_follow_on_attempts(&recovering, &owed.follow_on_turn_id, &next)
        .await
        .expect("the recovering shift raises the count");
    assert_eq!(raised.attempts, 1);
    assert_eq!(
        (in_flight(old.clone()).await, in_flight(next.clone()).await),
        (1, 1),
        "the recovering generation holds the run it recovers, and no other"
    );
    assert_eq!(
        handed
            .store
            .unfinished_run(&handed.session_id)
            .await
            .expect("read the unfinished run")
            .map(|unfinished| unfinished.run),
        Some(handed.run.clone()),
        "the raise ends no run"
    );
}
