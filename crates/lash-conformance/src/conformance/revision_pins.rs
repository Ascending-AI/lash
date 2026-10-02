//! FIG-4731: a state is named `(session, head revision)`, every head
//! publication is a retained revision until the host collects, and a pin is a
//! name (an input, a turn or a revision) that only host collection reads.
//!
//! Every law executes real runs through the store: an accepted input, the
//! run's admission, and its final commit, which writes the terminal that
//! names the revision the turn published. A fork "is" a turn when the fork's
//! head reads back the window, checkpoint, frame and model the turn's commit
//! published.

use super::session_store_factory::session_store_request;
use super::*;
use lash_core::store::{AdmittedHead, ConformanceDeployment, RunAdmission, ShiftFence};
use lash_core::testing::store_fixtures::seal_shift_fence_for_test;
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

/// One turn a law committed.
#[derive(Clone, Debug)]
struct CommittedTurn {
    /// The revision the turn's final commit published.
    revision: u64,
    /// What the session's head read back right after that commit.
    state: serde_json::Value,
}

/// One session a law executes turns on.
struct PinLaw {
    factory: Arc<dyn ConformanceDeployment>,
    request: crate::SessionStoreCreateRequest,
    view: crate::store::SessionStore,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
impl PinLaw {
    async fn new(factory: &Arc<dyn ConformanceDeployment>, session: &str) -> Self {
        let request = session_store_request(
            &SessionId::fixture(session),
            "revision-pin-model",
            crate::SessionRelation::Root,
        );
        let view = factory
            .admit_view(&request)
            .await
            .expect("create the law's session");
        Self {
            factory: Arc::clone(factory),
            request,
            view,
        }
    }

    fn id(&self) -> &SessionId {
        &self.request.session_id
    }

    fn store(&self) -> &Arc<dyn crate::RuntimeStore> {
        self.view.store()
    }

    /// Accept a next-turn input, pinned in its acceptance when `pin`.
    async fn accept(&self, text: &str, pin: bool) -> crate::InputId {
        let mut draft = crate::PendingTurnInputDraft::new(
            self.id().clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text(text),
        );
        if pin {
            draft = draft.pinned();
        }
        self.store()
            .enqueue_pending_turn_input(draft)
            .await
            .expect("accept the input")
            .input_id
    }

    /// Admit `run` headed by `head` under a fresh shift fence.
    async fn admit(&self, run: &str, head: &crate::InputId) -> (ShiftFence, RunAdmission) {
        let fence = seal_shift_fence_for_test(self.store(), self.id(), run).await;
        let admission =
            admitted_run(self.store(), &fence, run, AdmittedHead::Input(head.clone())).await;
        (fence, admission)
    }

    /// Land `run`'s final commit: one new node, a checkpoint component only
    /// this turn wrote, and the terminal that names the published revision.
    async fn commit(
        &self,
        run: &str,
        fence: &ShiftFence,
        settlement: crate::store::IngressSettlement,
    ) -> CommittedTurn {
        let mut state = crate::conformance::helpers::load_window_state(self.store(), self.id())
            .await
            .expect("load the session's state")
            .unwrap_or_else(|| crate::RuntimeSessionState {
                session_id: self.id().clone(),
                ..crate::RuntimeSessionState::new(self.request.config.session_policy())
            });
        state.ensure_agent_frame_initialized();
        append_conformance_event_node(&mut state, &format!("{}:{run}", self.id()), run);
        state.set_execution_state_snapshot(Some(run.as_bytes().to_vec().into()));
        let operation =
            crate::OperationId::turn(self.id(), TurnId::fixture(run.to_string()), "final");
        let (commit, _) =
            crate::RuntimeCommit::persisted_state_with_operation(&mut state, operation)
                .expect("build the turn's commit");
        let receipt = self
            .store()
            .commit_runtime_state(final_commit(commit, fence, settlement))
            .await
            .expect("the turn's final commit lands");
        CommittedTurn {
            revision: receipt.head_revision,
            state: self.head_state(self.id()).await,
        }
    }

    /// Accept an input, admit `run` with it and commit the run.
    async fn turn(&self, run: &str) -> CommittedTurn {
        let input = self.accept(run, false).await;
        let (fence, admission) = self.admit(run, &input).await;
        self.commit(run, &fence, completing_admission(run, &admission))
            .await
    }

    /// What `session`'s head reads back: the resume closure a fork must
    /// reproduce.
    async fn head_state(&self, session: &SessionId) -> serde_json::Value {
        let read = self
            .store()
            .load_session_window(session, crate::store::WindowSelector::Current)
            .await
            .expect("read the head")
            .expect("the session has a head");
        serde_json::json!({
            "window": read.window,
            "checkpoint_ref": read.checkpoint_ref,
            "checkpoint": read.checkpoint,
            "current_frame_node_id": read.current_frame_node_id,
            "model": read.config.model,
        })
    }

    /// Host collection: the one place the default policy releases.
    async fn collect(&self) -> crate::store::GcReport {
        self.store()
            .gc_unreachable()
            .await
            .expect("the host collection completes")
    }

    async fn retained(&self) -> Vec<u64> {
        self.factory
            .revisions(self.id())
            .await
            .expect("list the retained revisions")
            .into_iter()
            .map(|revision| revision.head_revision)
            .collect()
    }

    fn branch(&self, name: &str) -> SessionId {
        SessionId::fixture(format!("{}-{name}", self.id()))
    }

    /// Fork `target` into the branch `name`, as the facade does: resolve the
    /// target, then fork the revision it names.
    async fn fork(
        &self,
        target: &crate::Target,
        name: &str,
    ) -> Result<SessionId, crate::StoreError> {
        let revision = self.factory.resolve_target(self.id(), target).await?;
        let session_id = self.branch(name);
        self.factory
            .fork_session(&crate::ForkSessionRequest {
                session_id: session_id.clone(),
                source_session_id: self.id().clone(),
                head_revision: revision.head_revision,
                relation: crate::SessionRelation::Fork {
                    source_session_id: self.id().clone(),
                    source_node_id: revision.leaf_node_id.clone(),
                },
                pending_observer_intents: Vec::new(),
                config: revision.fork_config(),
            })
            .await?;
        Ok(session_id)
    }

    /// A fork of `target` is `turn`'s committed state exactly.
    async fn assert_fork_is(&self, target: &crate::Target, turn: &CommittedTurn, name: &str) {
        let fork = self
            .fork(target, name)
            .await
            .unwrap_or_else(|error| panic!("fork `{name}` of {target}: {error}"));
        assert_eq!(
            self.head_state(&fork).await,
            turn.state,
            "fork `{name}` of {target} must be the state revision {} published",
            turn.revision
        );
    }

    /// `target` resolves to `turn`'s revision.
    async fn assert_resolves(&self, target: &crate::Target, turn: &CommittedTurn) {
        let resolved = self
            .factory
            .resolve_target(self.id(), target)
            .await
            .unwrap_or_else(|error| panic!("resolve {target}: {error}"));
        assert_eq!(
            resolved.head_revision, turn.revision,
            "{target} names the revision its turn published"
        );
    }

    /// A fork of `target` refuses with the cause `refusal` recognises, names
    /// the target it was asked for, and creates no session.
    async fn assert_refused(
        &self,
        target: &crate::Target,
        name: &str,
        refusal: fn(&crate::StoreError) -> Option<(&SessionId, &crate::Target)>,
    ) {
        let error = match self.fork(target, name).await {
            Ok(fork) => panic!("{target} must refuse, forked `{fork}` instead"),
            Err(error) => error,
        };
        assert_eq!(
            refusal(&error),
            Some((self.id(), target)),
            "fork `{name}` of {target} refused the wrong way: {error:?}"
        );
        assert!(
            self.factory
                .live_view(&self.branch(name))
                .await
                .expect("look up the refused fork")
                .is_none(),
            "a refused fork of {target} must create no session"
        );
    }
}

fn pending(error: &crate::StoreError) -> Option<(&SessionId, &crate::Target)> {
    match error {
        crate::StoreError::ForkTargetPending { session_id, target } => Some((session_id, target)),
        _ => None,
    }
}

fn unavailable(error: &crate::StoreError) -> Option<(&SessionId, &crate::Target)> {
    match error {
        crate::StoreError::ForkTargetUnavailable { session_id, target } => {
            Some((session_id, target))
        }
        _ => None,
    }
}

fn pruned(error: &crate::StoreError) -> Option<(&SessionId, &crate::Target)> {
    match error {
        crate::StoreError::ForkTargetPruned { session_id, target } => Some((session_id, target)),
        _ => None,
    }
}

/// Law 1, pin timing. T2 is pinned (a) through its input before it starts,
/// (b) out of band through the unfinished run while it runs, or (c) by its
/// revision after T3 committed. In each case a host collection keeps T2 and
/// the head, releases T1, and a fork of the pinned target is T2's committed
/// state exactly.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_pin_written_before_during_or_after_its_turn_keeps_that_turn_through_collection(
    factory: Arc<dyn ConformanceDeployment>,
) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Timing {
        BeforeByInput,
        DuringByTurn,
        AfterByRevision,
    }
    for (case, timing) in [
        ("before", Timing::BeforeByInput),
        ("during", Timing::DuringByTurn),
        ("after", Timing::AfterByRevision),
    ] {
        let law = PinLaw::new(&factory, &format!("pin-timing-{case}")).await;
        let first = law.turn("t1").await;

        let input = law.accept("t2", timing == Timing::BeforeByInput).await;
        let mut target = crate::Target::Input(input.clone());
        if timing == Timing::BeforeByInput {
            law.assert_refused(&target, "too-early", pending).await;
        }
        let (fence, admission) = law.admit("t2", &input).await;
        if timing == Timing::DuringByTurn {
            let running = law
                .store()
                .unfinished_run(law.id())
                .await
                .expect("read the unfinished run")
                .expect("T2 is running")
                .run;
            assert_eq!(running, TurnId::from("t2"));
            target = crate::Target::Turn(running);
            law.factory
                .pin(law.id(), &target)
                .await
                .expect("pin the running turn out of band");
            law.assert_refused(&target, "too-early", pending).await;
        }
        let second = law
            .commit("t2", &fence, completing_admission("t2", &admission))
            .await;
        let third = law.turn("t3").await;
        if timing == Timing::AfterByRevision {
            target = crate::Target::Revision(second.revision);
            law.factory
                .pin(law.id(), &target)
                .await
                .expect("pin the past turn's revision");
        }

        law.collect().await;
        assert_eq!(
            law.retained().await,
            vec![second.revision, third.revision],
            "{case}: collection keeps the pinned turn and the head, nothing else"
        );
        // Every handle of T2 names the one revision the pin kept.
        for handle in [
            crate::Target::Input(input.clone()),
            crate::Target::Turn(TurnId::from("t2")),
            crate::Target::Revision(second.revision),
        ] {
            law.assert_resolves(&handle, &second).await;
        }
        let kept = law
            .factory
            .resolve_target(law.id(), &target)
            .await
            .expect("resolve the pinned target");
        assert_eq!(kept.pinned_by, vec![target.clone()]);
        assert!(!kept.head, "T3 is the head");
        law.assert_fork_is(&target, &second, "pinned").await;
        law.assert_refused(
            &crate::Target::Revision(first.revision),
            "collected",
            pruned,
        )
        .await;
    }
}

/// Law 1, the race. A pin of the running T2 races T2's final commit and T3's
/// admission and commit; whichever order the store serializes them in, the
/// pin holds T2 through the next collection.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_pin_racing_its_turns_commit_and_the_next_admission_keeps_the_turn(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = Arc::new(PinLaw::new(&factory, "pin-race").await);
    law.turn("t1").await;
    let input = law.accept("t2", false).await;
    let (fence, admission) = law.admit("t2", &input).await;
    let target = crate::Target::Turn(TurnId::from("t2"));

    let pinner = tokio::spawn({
        let law = Arc::clone(&law);
        let target = target.clone();
        async move { law.factory.pin(law.id(), &target).await }
    });
    let driver = tokio::spawn({
        let law = Arc::clone(&law);
        async move {
            let second = law
                .commit("t2", &fence, completing_admission("t2", &admission))
                .await;
            let third = law.turn("t3").await;
            (second, third)
        }
    });
    let (pin, turns) = tokio::join!(pinner, driver);
    pin.expect("the pin task ran")
        .expect("a pin never waits on, or loses to, its turn");
    let (second, third) = turns.expect("the turns committed");

    law.collect().await;
    assert_eq!(
        law.retained().await,
        vec![second.revision, third.revision],
        "the racing pin holds T2 through collection"
    );
    law.assert_fork_is(&target, &second, "raced").await;
}

/// Law 2. An input its first run handed back resolves to the run that
/// later applied it, never to the run that deferred it; inputs merged into
/// one run resolve to that run; and a pin written twice is one pin.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_merged_or_deferred_input_resolves_to_the_run_that_applied_it(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "pin-merge-defer").await;

    // Deferral: `ra` admits the pinned input and hands it back open.
    let deferred = law.accept("deferred", true).await;
    let deferred_target = crate::Target::Input(deferred.clone());
    let (fence, _) = law.admit("ra", &deferred).await;
    let deferring = law
        .commit(
            "ra",
            &fence,
            releasing("ra", [crate::store::IngressRowId::Input(deferred.clone())]),
        )
        .await;
    law.assert_refused(&deferred_target, "deferred-early", pending)
        .await;
    let (fence, admission) = law.admit("rb", &deferred).await;
    let applying = law
        .commit("rb", &fence, completing_admission("rb", &admission))
        .await;
    assert_ne!(deferring.revision, applying.revision);
    law.assert_resolves(&deferred_target, &applying).await;

    // Merge: `rc`, headed by the second input, takes the first with it.
    let merged = law.accept("merged", false).await;
    let merged_target = crate::Target::Input(merged.clone());
    for _ in 0..2 {
        law.factory
            .pin(law.id(), &merged_target)
            .await
            .expect("a retried pin is the first one");
    }
    let head = law.accept("head", false).await;
    let (fence, admission) = law.admit("rc", &head).await;
    assert_eq!(
        admission
            .inputs
            .as_ref()
            .expect("the run admitted inputs")
            .input_ids(),
        vec![merged.clone(), head.clone()],
        "the run headed by the second input takes the first with it"
    );
    let merging = law
        .commit("rc", &fence, completing_admission("rc", &admission))
        .await;
    law.assert_resolves(&merged_target, &merging).await;
    law.assert_resolves(&crate::Target::Input(head), &merging)
        .await;
    assert_eq!(
        law.factory
            .resolve_target(law.id(), &merged_target)
            .await
            .expect("resolve the merged input")
            .pinned_by,
        vec![merged_target.clone()],
        "a pin written twice is one pin"
    );

    let last = law.turn("rd").await;
    law.collect().await;
    assert_eq!(
        law.retained().await,
        vec![applying.revision, merging.revision, last.revision],
        "each pinned input keeps the revision of the run that applied it"
    );
    law.assert_fork_is(&deferred_target, &applying, "deferred")
        .await;
    law.assert_fork_is(&merged_target, &merging, "merged").await;
}

/// Law 3. With no pin, under the default policy, every past turn and the
/// creation revision fork until the host collects; after the collection each
/// refuses as collected and only the head forks.
pub async fn every_turn_forks_until_the_host_collects_and_refuses_pruned_after(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "retained-until-gc").await;
    let created = CommittedTurn {
        revision: 0,
        state: law.head_state(law.id()).await,
    };
    let mut turns = Vec::new();
    for run in ["t1", "t2", "t3"] {
        turns.push((run, law.turn(run).await));
    }
    assert_eq!(
        law.retained().await,
        (0..=3).collect::<Vec<_>>(),
        "every head publication is retained until the host collects"
    );
    law.assert_fork_is(&crate::Target::Revision(0), &created, "created")
        .await;
    for (run, turn) in &turns {
        law.assert_fork_is(
            &crate::Target::Turn(TurnId::from(*run)),
            turn,
            &format!("{run}-by-turn"),
        )
        .await;
        law.assert_fork_is(
            &crate::Target::Revision(turn.revision),
            turn,
            &format!("{run}-by-revision"),
        )
        .await;
    }

    law.collect().await;
    let (_, head) = turns
        .pop()
        .unwrap_or_else(|| unreachable!("three turns ran"));
    assert_eq!(law.retained().await, vec![head.revision]);
    law.assert_refused(&crate::Target::Revision(0), "created-late", pruned)
        .await;
    for (run, turn) in &turns {
        law.assert_refused(
            &crate::Target::Turn(TurnId::from(*run)),
            &format!("{run}-by-turn-late"),
            pruned,
        )
        .await;
        law.assert_refused(
            &crate::Target::Revision(turn.revision),
            &format!("{run}-by-revision-late"),
            pruned,
        )
        .await;
    }
    law.assert_fork_is(&crate::Target::Turn(TurnId::from("t3")), &head, "head")
        .await;
}

/// D4: a session that never ran a turn forks at its creation revision. The
/// fork copies the recorded configuration and records its lineage with no
/// source node.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_session_that_never_ran_a_turn_forks_at_its_creation_revision(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "fork-empty").await;
    let created = law
        .factory
        .resolve_target(law.id(), &crate::Target::Revision(0))
        .await
        .expect("the creation revision is retained");
    assert!(created.head);
    assert_eq!(created.leaf_node_id, None, "nothing has been appended");
    assert_eq!(created.config.model, law.request.config.model);

    let fork = law
        .fork(&crate::Target::Revision(0), "branch")
        .await
        .expect("an empty session forks at its creation revision");
    assert_eq!(
        law.head_state(&fork).await,
        law.head_state(law.id()).await,
        "the fork copies the empty session's recorded head"
    );
    assert_eq!(
        law.store()
            .load_session_meta(&fork)
            .await
            .expect("read the fork's metadata")
            .expect("the fork has metadata")
            .relation,
        crate::SessionRelation::Fork {
            source_session_id: law.id().clone(),
            source_node_id: None,
        },
        "the fork records its lineage; there is no source node to name"
    );
    // The fork is an ordinary session: its own creation revision is its head.
    assert_eq!(
        factory
            .revisions(&fork)
            .await
            .expect("list the fork's revisions")
            .into_iter()
            .map(|revision| (revision.head_revision, revision.leaf_node_id, revision.head))
            .collect::<Vec<_>>(),
        vec![(0, None, true)]
    );
}

/// Law 4. Every reclaimer honours the one retained-revisions relation: after
/// host collection, vacuum, retained-evidence reclaim and a sibling session's
/// deletion, a reopened store still forks the pinned turn with its whole
/// resume closure. A turn stays while any pin names it; releasing the last
/// pin lets the next collection take it and its checkpoint.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_reclaimer_keeps_a_pinned_turn_until_its_last_pin_is_released(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "pin-reclaimers").await;
    law.turn("t1").await;
    let input = law.accept("t2", true).await;
    let (fence, admission) = law.admit("t2", &input).await;
    let second = law
        .commit("t2", &fence, completing_admission("t2", &admission))
        .await;
    let by_input = crate::Target::Input(input);
    let by_turn = crate::Target::Turn(TurnId::from("t2"));
    law.factory
        .pin(law.id(), &by_turn)
        .await
        .expect("pin the turn a second way");
    // A sibling that shares the pinned prefix, deleted below.
    let sibling = law
        .fork(&by_turn, "sibling")
        .await
        .expect("fork the pinned turn");
    let third = law.turn("t3").await;

    let run_every_reclaimer = || async {
        law.collect().await;
        law.view.vacuum().await.expect("vacuum the session");
        law.factory
            .reclaim_retained_evidence(crate::RetentionBound {
                committed_before_epoch_ms: u64::MAX,
                turn_watermark: lash_core::store::TurnProjectionWatermark::NoProjector,
            })
            .await
            .expect("reclaim retained evidence");
    };
    run_every_reclaimer().await;
    law.factory
        .delete_session(&sibling)
        .await
        .expect("delete the sibling: its blob reclaim and ancestry retirement run");
    run_every_reclaimer().await;
    assert_eq!(law.retained().await, vec![second.revision, third.revision]);

    // Reopen: a fresh view over the same store.
    let reopened = PinLaw {
        factory: Arc::clone(&law.factory),
        request: law.request.clone(),
        view: law
            .factory
            .live_view(law.id())
            .await
            .expect("reopen the session")
            .expect("the session is live"),
    };
    reopened
        .assert_fork_is(&by_input, &second, "after-reclaim-by-input")
        .await;
    reopened
        .assert_fork_is(&by_turn, &second, "after-reclaim-by-turn")
        .await;
    for fork in ["after-reclaim-by-input", "after-reclaim-by-turn"] {
        law.factory
            .delete_session(&law.branch(fork))
            .await
            .expect("delete the probe fork");
    }

    law.factory
        .unpin(law.id(), &by_input)
        .await
        .expect("release one of the two pins");
    law.collect().await;
    assert_eq!(
        law.retained().await,
        vec![second.revision, third.revision],
        "the turn stays while any pin names it"
    );
    law.factory
        .unpin(law.id(), &by_turn)
        .await
        .expect("release the last pin");
    let report = law.collect().await;
    assert_eq!(
        law.retained().await,
        vec![third.revision],
        "releasing the last pin lets the collection take the turn"
    );
    assert!(
        report.deleted_blob_count > 0,
        "the released turn's checkpoint is collected with it: {report:?}"
    );
    law.assert_refused(&by_turn, "released", pruned).await;
    law.assert_fork_is(
        &crate::Target::Revision(third.revision),
        &third,
        "head-after-release",
    )
    .await;
}

/// The session retention policy's window: `LastTurns(n)` keeps the last `n`
/// terminal runs' revisions and `HeadOnly` only the head, each releasing at
/// commit with no host collection; a pin keeps a turn outside either window.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_retention_window_counts_terminal_runs_and_pins_extend_it(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "retention-window").await;
    assert_eq!(
        law.factory
            .retention(law.id())
            .await
            .expect("read the default policy"),
        crate::Retention::UntilGc
    );
    let window = crate::Retention::LastTurns(std::num::NonZeroU32::MIN.saturating_add(1));
    law.factory
        .set_retention(law.id(), window)
        .await
        .expect("set a two-turn window");
    assert_eq!(
        law.factory
            .retention(law.id())
            .await
            .expect("read the policy back"),
        window
    );
    let first = law.turn("t1").await;
    law.factory
        .pin(law.id(), &crate::Target::Turn(TurnId::from("t1")))
        .await
        .expect("pin the first turn");
    let second = law.turn("t2").await;
    let third = law.turn("t3").await;
    let fourth = law.turn("t4").await;
    assert_eq!(
        law.retained().await,
        vec![first.revision, third.revision, fourth.revision],
        "the last two terminal runs and the pinned turn stay; the commits released the rest"
    );
    law.assert_refused(&crate::Target::Revision(second.revision), "outside", pruned)
        .await;
    law.assert_fork_is(&crate::Target::Turn(TurnId::from("t1")), &first, "pinned")
        .await;

    law.factory
        .set_retention(law.id(), crate::Retention::HeadOnly)
        .await
        .expect("keep only the head");
    let fifth = law.turn("t5").await;
    assert_eq!(
        law.retained().await,
        vec![first.revision, fifth.revision],
        "head-only keeps the head and what is pinned"
    );
    law.collect().await;
    assert_eq!(law.retained().await, vec![first.revision, fifth.revision]);
}

/// Law 5. A target whose run has not finished refuses `Pending`; a run
/// that ended without a commit and a withdrawn input refuse `Unavailable`.
/// Neither ever forks the head in the target's place, and a pin on such a
/// target is accepted and keeps nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_and_unavailable_targets_refuse_typed_and_never_fork_the_head(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "fork-refusals").await;
    let first = law.turn("t1").await;
    let head_before = law.head_state(law.id()).await;

    // Accepted, not admitted: no run has taken the input.
    let input = law.accept("t2", true).await;
    let by_input = crate::Target::Input(input.clone());
    let by_turn = crate::Target::Turn(TurnId::from("t2"));
    law.assert_refused(&by_input, "accepted", pending).await;
    law.assert_refused(&by_turn, "unadmitted", pending).await;
    law.assert_refused(
        &crate::Target::Revision(first.revision + 1),
        "unpublished",
        pending,
    )
    .await;

    // Admitted and running.
    let (fence, _) = law.admit("t2", &input).await;
    law.factory
        .pin(law.id(), &by_turn)
        .await
        .expect("a pin is accepted while its turn runs");
    law.assert_refused(&by_input, "running-by-input", pending)
        .await;
    law.assert_refused(&by_turn, "running-by-turn", pending)
        .await;

    // The run ends without a commit.
    let refusal = lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::StoreCommitSuperseded,
        "the run is refused before it commits",
    );
    assert!(
        matches!(
            law.store()
                .end_refused_run(&fence, &TurnId::from("t2"), &refusal, 1)
                .await
                .expect("end the refused run"),
            crate::store::RunEndOutcome::Ended(_)
        ),
        "the refusal ends the run"
    );
    law.assert_refused(&by_turn, "refused-by-turn", unavailable)
        .await;
    law.assert_refused(&by_input, "refused-by-input", unavailable)
        .await;

    // An input withdrawn before any run took it.
    let withdrawn = law.accept("withdrawn", true).await;
    law.store()
        .cancel_pending_turn_input(law.id(), withdrawn.as_str())
        .await
        .expect("withdraw the input");
    law.assert_refused(&crate::Target::Input(withdrawn), "withdrawn", unavailable)
        .await;

    assert_eq!(
        law.head_state(law.id()).await,
        head_before,
        "no refusal moved the head"
    );
    // Pins that name no state keep nothing.
    law.collect().await;
    assert_eq!(law.retained().await, vec![first.revision]);
}
