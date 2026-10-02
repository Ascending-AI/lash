//! FIG-4200: a run overtaken by another writer ends typed on every shift
//! path.
//!
//! The bound turn owns the session head (FIG-4202): the store refuses every
//! lane-less head write while a run is bound, so no host write overtakes
//! it. The race that remains is between writers presenting the run's own
//! shift fence, such as a second execution of the same run. Here a run's
//! pressure hook holds its journaled summary while such a writer moves the
//! head under the run's fence. The pressure frame's commit then meets the
//! moved head. The run can never commit on the base it was admitted on, so
//! it ends `Refused(StoreCommitSuperseded)` whichever shift path ran it: the
//! shift loop (a queued drain) or an engine's own run attempt. It never
//! parks, its input is answered with the refusal, and the next shift admits
//! a new run. That typed loss is the ownership rule's backstop.
//!
//! The end holds across a crash before it is written: the redrive replays the
//! journal, meets the same moved head and writes it. A shift that resumes
//! the unfinished run on a fresh journal finds the head moved only under
//! the run's own fence, which is the run's own writing, and continues from
//! it (FIG-4202); [`a_run_resumed_on_a_fresh_journal_continues_from_its_own_frame`]
//! holds that.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;

use super::{
    LawParts, LawSession, ModelScript, PRESSURE_THRESHOLD_TOKENS, StandardFrameLawProtocol,
    SummaryHold, active_path, build_runtime, frame_chain, law_model,
};
use crate::admit;

/// Which shift path runs the overtaken run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupersededRunPath {
    /// The shift loop, through a queued drain.
    ShiftLoop,
    /// An engine's own run attempt: an admission step, then the admitted
    /// run's execution.
    Engine,
}

/// How the overtaken run's execution ends before its end is durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupersededRunRecovery {
    /// Nothing interrupts it: the execution that met the refusal ends the run.
    None,
    /// The execution dies right before it writes the end; the tier redrives
    /// it on the journal it left.
    CrashBeforeEnd,
}

/// The note a writer holding the run's fence appends while the run's
/// pressure hook holds.
const OVERTAKING_NOTE: &str = "a note another writer appended mid-run";

/// How one shift of the law ended: `Ok` for a run that committed.
type ShiftEnd = Result<(), crate::RuntimeError>;
type ShiftEndTx = tokio::sync::mpsc::UnboundedSender<ShiftEnd>;

/// One attempt at a shift on `path`. Every run reports how its shift ended
/// and settles, so the tier never retries a refused run on its own.
fn path_attempt(
    parts: &LawParts,
    path: SupersededRunPath,
    result_tx: ShiftEndTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(&parts, None).await;
            let end = match path {
                SupersededRunPath::ShiftLoop => Box::pin(runtime.execute_next_queued_run(
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                ))
                .await
                .and_then(|drain| {
                    drain.ran().map(|_| ()).ok_or_else(|| {
                        crate::RuntimeError::new(
                            crate::RuntimeErrorCode::QueuedWork,
                            "the queued drain ran no run",
                        )
                    })
                }),
                SupersededRunPath::Engine => Box::pin(engine_run(&mut runtime, &scope)).await,
            };
            let _ = result_tx.send(end);
            crate::ConformanceTurnEnd::Settled
        })
    })
}

/// An engine's shift of the next run: the admission step, then the run's
/// run, as an engine that splits the shift over its own handlers runs them.
async fn engine_run(
    runtime: &mut crate::LashRuntime,
    scope: &crate::ScopedEffectController<'_>,
) -> ShiftEnd {
    let request = lash_core::engine::ShiftRequest {
        session: runtime.export_state().session_id.clone(),
        request: lash_core::engine::ShiftRequestId::new(scope.scope_id()),
        intended_lane: None,
    };
    let admitted = match lash_core::shift::admit_shift(runtime, scope, &request, 0, None)
        .await
        .map_err(lash_core::engine::ShiftAbort::into_error)?
    {
        lash_core::engine::AdmitVerdict::Admit(admitted) => admitted,
        other => {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::QueuedWork,
                format!("the engine's admission admitted no run: {other:?}"),
            ));
        }
    };
    match lash_core::shift::execute_admitted_run(runtime, scope, admitted)
        .await
        .map_err(lash_core::engine::ShiftAbort::into_error)?
    {
        lash_core::engine::RunOutcome::Committed { .. } => Ok(()),
        other => Err(crate::RuntimeError::new(
            crate::RuntimeErrorCode::QueuedWork,
            format!("the engine's run did not commit: {other:?}"),
        )),
    }
}

impl LawSession {
    /// Runs one shift of the next run on `path` under the shift name
    /// `shift`, and answers how it ended.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn work_on(&self, shift: &str, path: SupersededRunPath) -> ShiftEnd {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner
                .run_turn(self.shift_scope(shift), path_attempt(&self.parts, path, tx)),
        )
        .await
        .expect("the shift ends");
        rx.recv().await.expect("the tier's runner ran the shift")
    }

    /// The scope a shift named `shift` runs under.
    fn shift_scope(&self, shift: &str) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::turn(
            &self.session_id,
            crate::TurnId::fixture(format!("{}-{shift}", self.prefix)),
        ))
    }

    /// Appends [`OVERTAKING_NOTE`] through another runtime: a lane-less
    /// head write, run in a runtime operation of its own on the tier.
    /// Moves the head under the bound run's own shift fence, as a second
    /// execution of the run would: a lane-less write is refused while the
    /// run is bound (FIG-4202), so the fence is what lets this one land.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn overtake_under_the_runs_fence(&self) {
        let fence = crate::store::current_shift_fence(self.store.as_ref(), &self.session_id)
            .await
            .expect("read the session's shift fence")
            .expect("the bound run sealed the session");
        let mut state =
            crate::conformance::helpers::load_window_state(&self.store, &self.session_id)
                .await
                .expect("read the session head")
                .expect("the session committed");
        state.append_active_conversation_messages(&[crate::Message {
            id: "superseded-run-overtaking-note".to_string(),
            role: crate::MessageRole::Assistant,
            parts: vec![crate::Part::text(
                "superseded-run-overtaking-note.p0".to_string(),
                OVERTAKING_NOTE.to_string(),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        }]);
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
        commit.shift_fence = Some(Box::new(fence));
        self.store
            .commit_runtime_state(commit)
            .await
            .expect("a writer holding the run's fence moves the head");
    }
}

/// A run whose held pressure frame a writer holding its own shift fence
/// overtakes ends `Refused(StoreCommitSuperseded)` on `path`, recovered as
/// `recovery` says: one terminal, its input answered, no park, the summary
/// requested once, and the next shift admits a new run that commits.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_superseded_run_ends_typed_on_every_shift_path(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    path: SupersededRunPath,
    recovery: SupersededRunRecovery,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            // The first run's usage crosses the pressure hook's threshold.
            (protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("superseded-{path:?}-{recovery:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    let recording = Arc::new(
        lash_core::testing::runtime_helpers::RecordingStore::over_session(
            Arc::clone(&law.store),
            law.session_id.clone(),
        ),
    );
    law.parts.store = Arc::clone(&recording) as Arc<dyn crate::RuntimeStore>;
    law.parts.compaction.pressure_hold = Some(SummaryHold::default());
    let hold = law
        .parts
        .compaction
        .pressure_hold
        .clone()
        .expect("the law holds its pressure summary");

    law.enqueue("first question").await;
    law.work_on("run-1", path)
        .await
        .expect("the first run commits below the pressure threshold");
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");

    let overtaken_input = law.enqueue("second question").await;
    match recovery {
        SupersededRunRecovery::None => {}
        SupersededRunRecovery::CrashBeforeEnd => {
            recording.before_next_end_refused_run(Arc::new(|| {
                Box::pin(async { panic!("injected crash before the refused run's end") })
            }))
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let overtaken = async {
        match recovery {
            SupersededRunRecovery::None => {
                law.runner
                    .run_turn(
                        law.shift_scope("run-2"),
                        path_attempt(&law.parts, path, tx.clone()),
                    )
                    .await;
            }
            SupersededRunRecovery::CrashBeforeEnd => {
                let (crashed_tx, _crashed_rx) = tokio::sync::mpsc::unbounded_channel();
                law.runner
                    .run_crashed_then_redriven_turn(
                        law.shift_scope("run-2"),
                        path_attempt(&law.parts, path, crashed_tx),
                        path_attempt(&law.parts, path, tx.clone()),
                    )
                    .await;
            }
        }
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(
            overtaken,
            hold.while_held(law.overtake_under_the_runs_fence()),
        ),
    )
    .await
    .expect("the overtaken run's shift ends");
    let first_end = rx.recv().await.expect("the tier ran the overtaken shift");
    let refused = first_end.expect_err("the overtaken run is refused");
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::StoreCommitSuperseded,
        "{refused:?}"
    );

    let run = law
        .store
        .run_of_input(&law.session_id, &overtaken_input)
        .await
        .expect("read the input's run")
        .expect("the overtaken input was admitted to a run");
    let terminal = law
        .store
        .run_terminal(&law.session_id, &run)
        .await
        .expect("read the run's terminal")
        .expect("the overtaken run ended");
    assert!(
        matches!(
            &terminal.cause,
            crate::store::RunTerminalCause::Refused { code, .. }
                if *code == crate::RuntimeErrorCode::StoreCommitSuperseded
        ),
        "{terminal:?}"
    );
    assert_eq!(
        law.store
            .load_turn_park(&law.session_id)
            .await
            .expect("read the session's park"),
        None,
        "an overtaken run never parks"
    );
    assert!(
        law.store
            .unfinished_run(&law.session_id)
            .await
            .expect("read the unfinished run")
            .is_none(),
        "the overtaken run no longer holds the session"
    );
    assert!(
        law.store
            .list_pending_turn_inputs(&law.session_id)
            .await
            .expect("read pending input")
            .iter()
            .all(|row| row.input.input_id != overtaken_input),
        "the overtaken run's input is answered"
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        1,
        "the journaled summary is never requested again"
    );
    assert_eq!(
        model.turn_calls.load(Ordering::SeqCst),
        1,
        "the overtaken run made no model call"
    );
    let head = law.head().await;
    assert_eq!(
        frame_chain(&head, &law.session_id).len(),
        1,
        "the overtaken pressure frame never opened"
    );
    let path_texts = active_path(&head.graph);
    assert!(
        path_texts.iter().any(|text| text == OVERTAKING_NOTE),
        "the overtaking append stands: {path_texts:?}"
    );

    law.enqueue("third question").await;
    law.work_on("run-3", path)
        .await
        .expect("the next shift admits a new run, which commits");
    assert_eq!(
        law.store
            .load_turn_park(&law.session_id)
            .await
            .expect("read the session's park"),
        None
    );
    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(
        chain.len(),
        2,
        "the new run's pressure frame opens: {chain:?}"
    );
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    let path_texts = active_path(&head.graph);
    assert!(
        path_texts.iter().any(|text| text == "answer 2"),
        "{path_texts:?}"
    );
}

/// A run whose own pressure frame committed, and whose turn then failed at
/// its terminal commit, is resumed by a new shift on a fresh journal
/// (FIG-4201). The head moved from its admission's base, but its own fenced
/// commit moved it: the bound turn owns the head, so the run is not
/// overtaken. It continues from its own pressure frame on `path`, without a
/// second summary, and commits: no refusal, no park, its input answered.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_resumed_on_a_fresh_journal_continues_from_its_own_frame(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    path: SupersededRunPath,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            // The first run's usage crosses the pressure hook's threshold.
            (protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            // The resumed run's first execution, whose terminal commit fails.
            (protocol.answer("answer 2"), 1),
            // Its execution on the fresh journal.
            (protocol.answer("answer 2"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("own-frame-{path:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    let recording = Arc::new(
        lash_core::testing::runtime_helpers::RecordingStore::over_session(
            Arc::clone(&law.store),
            law.session_id.clone(),
        ),
    );
    law.parts.store = Arc::clone(&recording) as Arc<dyn crate::RuntimeStore>;

    law.enqueue("first question").await;
    law.work_on("run-1", path)
        .await
        .expect("the first run commits below the pressure threshold");
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");

    let input = law.enqueue("second question").await;
    recording.fail_next_turn_terminal_commit(crate::StoreError::Backend(
        "injected store fault on the turn's terminal commit".to_string(),
    ));
    let fault = law
        .work_on("run-2", path)
        .await
        .expect_err("the run's terminal commit fails");
    assert_ne!(
        fault.code,
        crate::RuntimeErrorCode::StoreCommitSuperseded,
        "a store fault on the terminal commit is the attempt's live fault: {fault:?}"
    );
    let moved = law.head().await;
    let chain = frame_chain(&moved, &law.session_id);
    assert_eq!(
        chain.len(),
        2,
        "the run's own pressure frame committed: {chain:?}"
    );
    assert!(
        law.store
            .unfinished_run(&law.session_id)
            .await
            .expect("read the unfinished run")
            .is_some(),
        "the run is still bound"
    );

    law.work_on("run-2-fresh-journal", path)
        .await
        .expect("the resumed run continues from its own pressure frame and commits");

    let run = law
        .store
        .run_of_input(&law.session_id, &input)
        .await
        .expect("read the input's run")
        .expect("the input was admitted to a run");
    let terminal = law
        .store
        .run_terminal(&law.session_id, &run)
        .await
        .expect("read the run's terminal")
        .expect("the resumed run ended");
    assert!(
        matches!(
            terminal.cause,
            crate::store::RunTerminalCause::Committed { .. }
        ),
        "the resumed run committed: {terminal:?}"
    );
    assert_eq!(
        law.store
            .load_turn_park(&law.session_id)
            .await
            .expect("read the session's park"),
        None,
        "a run its own commits moved never parks"
    );
    assert!(
        law.store
            .list_pending_turn_inputs(&law.session_id)
            .await
            .expect("read pending input")
            .iter()
            .all(|row| row.input.input_id != input),
        "the run's input is answered"
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        1,
        "the run continues from its own pressure frame and never summarizes again"
    );
    let head = law.head().await;
    assert_eq!(
        head.head_revision,
        moved.head_revision + 1,
        "the resumed run commits its turn over its own pressure frame"
    );
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "one pressure frame: {chain:?}");
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    let path_texts = active_path(&head.graph);
    let seed_at = path_texts
        .iter()
        .position(|text| text == super::SUMMARY_TEXT)
        .expect("the pressure frame's seed is on the path");
    assert_eq!(
        path_texts[seed_at + 1..],
        ["second question", "answer 2"],
        "the resumed run executes in its own pressure frame"
    );
}
