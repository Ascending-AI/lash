//! FIG-4200: a root overtaken by another writer ends typed on every drive
//! path.
//!
//! A root's pressure hook holds its journaled summary while another runtime
//! appends to the session, a lane-less head write that no drive fence stops
//! (FIG-4010). The pressure frame's commit then meets the moved head. The
//! root can never commit on the base it was admitted on, so it ends
//! `Refused(StoreCommitSuperseded)` whichever drive path ran it: the drive
//! loop (a queued drain) or an engine's own root attempt. It never parks, its
//! input is answered with the refusal, and the next drive admits a new root.
//!
//! The end holds across a crash before it is written (the redrive replays the
//! journal, meets the same moved head and writes it) and on a fresh journal
//! (a new drive resumes the unfinished root, and its live inspection finds
//! the head overtaken).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;

use super::{
    LawParts, LawSession, ModelScript, PRESSURE_THRESHOLD_TOKENS, StandardFrameLawProtocol,
    SummaryHold, active_path, build_runtime, frame_chain, law_model,
};
use crate::admit;

/// Which drive path runs the overtaken root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupersededRootPath {
    /// The drive loop, through a queued drain.
    DriveLoop,
    /// An engine's own root attempt: an admission step, then the admitted
    /// root's run.
    Engine,
}

/// How the overtaken root's execution ends before its end is durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupersededRootRecovery {
    /// Nothing interrupts it: the run that met the refusal ends the root.
    None,
    /// The execution dies right before it writes the end; the tier redrives
    /// it on the journal it left.
    CrashBeforeEnd,
    /// The execution fails to write the end and its journal is gone; a new
    /// drive resumes the unfinished root on a fresh journal.
    FreshJournal,
}

/// The note another runtime appends while the root's pressure hook holds.
const OVERTAKING_NOTE: &str = "a note another runtime appended mid-root";

/// How one drive of the law ended: `Ok` for a root that committed.
type DriveEnd = Result<(), crate::RuntimeError>;
type DriveEndTx = tokio::sync::mpsc::UnboundedSender<DriveEnd>;

/// One attempt at a drive on `path`. Every run reports how its drive ended
/// and settles, so the tier never retries a refused root on its own.
fn path_attempt(
    parts: &LawParts,
    path: SupersededRootPath,
    result_tx: DriveEndTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(&parts, None).await;
            let end = match path {
                SupersededRootPath::DriveLoop => Box::pin(runtime.drive_next_queued_root(
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                ))
                .await
                .and_then(|drain| {
                    drain.ran().map(|_| ()).ok_or_else(|| {
                        crate::RuntimeError::new(
                            crate::RuntimeErrorCode::QueuedWork,
                            "the queued drain ran no root",
                        )
                    })
                }),
                SupersededRootPath::Engine => {
                    let generation = parts.host.backend().build_generation().clone();
                    Box::pin(engine_root(&mut runtime, &scope, generation)).await
                }
            };
            let _ = result_tx.send(end);
            crate::ConformanceTurnEnd::Settled
        })
    })
}

/// An engine's drive of the next root: the admission step, then the root's
/// run, as an engine that splits the drive over its own handlers runs them.
async fn engine_root(
    runtime: &mut crate::LashRuntime,
    scope: &crate::ScopedEffectController<'_>,
    build_generation: lash_core::engine::BuildGeneration,
) -> DriveEnd {
    let request = lash_core::engine::DriveRequest {
        session: runtime.export_state().session_id.clone(),
        request: lash_core::engine::DriveRequestId::new(scope.scope_id()),
        build_generation,
    };
    let admitted = match lash_core::drive::admit_drive(runtime, scope, &request, 0)
        .await
        .map_err(lash_core::engine::DriveAbort::into_error)?
    {
        lash_core::engine::AdmitVerdict::Admit(admitted) => admitted,
        other => {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::QueuedWork,
                format!("the engine's admission admitted no root: {other:?}"),
            ));
        }
    };
    match lash_core::drive::run_admitted_root(runtime, scope, admitted)
        .await
        .map_err(lash_core::engine::DriveAbort::into_error)?
    {
        lash_core::engine::RootOutcome::Committed { .. } => Ok(()),
        other => Err(crate::RuntimeError::new(
            crate::RuntimeErrorCode::QueuedWork,
            format!("the engine's root did not commit: {other:?}"),
        )),
    }
}

impl LawSession {
    /// Runs one drive of the next root on `path` under the drive name
    /// `drive`, and answers how it ended.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn drive_on(&self, drive: &str, path: SupersededRootPath) -> DriveEnd {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner
                .run_turn(self.drive_scope(drive), path_attempt(&self.parts, path, tx)),
        )
        .await
        .expect("the drive ends");
        rx.recv().await.expect("the tier's runner ran the drive")
    }

    /// The scope a drive named `drive` runs under.
    fn drive_scope(&self, drive: &str) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::queue_drain(
            &self.session_id,
            format!("{}-{drive}", self.prefix),
        ))
    }

    /// Appends [`OVERTAKING_NOTE`] through another runtime: a lane-less
    /// head write, run in a runtime operation of its own on the tier.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn append_from_another_runtime(&self) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let parts = self.parts.clone();
        let attempt: crate::ConformanceTurnAttempt = Arc::new(move |_scope| {
            let parts = parts.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut writer = build_runtime(&parts, None).await;
                let appended = Box::pin(writer.append_session_nodes(
                    crate::AppendSessionNodesRequest {
                        operation_id: "superseded-root-overtaking-append".to_string(),
                        nodes: vec![crate::SessionAppendNode::message(
                            crate::PluginMessage::text(
                                crate::MessageRole::Assistant,
                                OVERTAKING_NOTE,
                            ),
                        )],
                        requires_ancestor_node_id: None,
                    },
                ))
                .await
                .map(|_| ());
                let _ = tx.send(appended);
                crate::ConformanceTurnEnd::Settled
            })
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner.run_turn(
                admit(crate::ExecutionScope::runtime_operation(format!(
                    "{}-overtaking-append",
                    self.prefix
                ))),
                attempt,
            ),
        )
        .await
        .expect("the append ends");
        rx.recv()
            .await
            .expect("the tier ran the append")
            .expect("another runtime's lane-less append lands");
    }
}

/// A root whose held pressure frame another runtime's lane-less append
/// overtakes ends `Refused(StoreCommitSuperseded)` on `path`, recovered as
/// `recovery` says: one terminal, its input answered, no park, the summary
/// requested once, and the next drive admits a new root that commits.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_superseded_root_ends_typed_on_every_drive_path(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    path: SupersededRootPath,
    recovery: SupersededRootRecovery,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            // The first root's usage crosses the pressure hook's threshold.
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
    law.drive_on("root-1", path)
        .await
        .expect("the first root commits below the pressure threshold");
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");

    let overtaken_input = law.enqueue("second question").await;
    match recovery {
        SupersededRootRecovery::None => {}
        SupersededRootRecovery::CrashBeforeEnd => {
            recording.before_next_end_refused_root(Arc::new(|| {
                Box::pin(async { panic!("injected crash before the refused root's end") })
            }))
        }
        SupersededRootRecovery::FreshJournal => {
            recording.fail_next_end_refused_root(crate::StoreError::Backend(
                "injected store fault on the refused root's end".to_string(),
            ));
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let overtaken = async {
        match recovery {
            SupersededRootRecovery::None | SupersededRootRecovery::FreshJournal => {
                law.runner
                    .run_turn(
                        law.drive_scope("root-2"),
                        path_attempt(&law.parts, path, tx.clone()),
                    )
                    .await;
            }
            SupersededRootRecovery::CrashBeforeEnd => {
                let (crashed_tx, _crashed_rx) = tokio::sync::mpsc::unbounded_channel();
                law.runner
                    .run_crashed_then_redriven_turn(
                        law.drive_scope("root-2"),
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
            hold.while_held(law.append_from_another_runtime()),
        ),
    )
    .await
    .expect("the overtaken root's drive ends");
    let first_end = rx.recv().await.expect("the tier ran the overtaken drive");
    let refused = match recovery {
        SupersededRootRecovery::None | SupersededRootRecovery::CrashBeforeEnd => {
            first_end.expect_err("the overtaken root is refused")
        }
        SupersededRootRecovery::FreshJournal => {
            let fault = first_end.expect_err("the end's store fault fails the drive");
            assert_ne!(
                fault.code,
                crate::RuntimeErrorCode::StoreCommitSuperseded,
                "a store fault on the end is the attempt's live fault: {fault:?}"
            );
            law.drive_on("root-2-fresh-journal", path)
                .await
                .expect_err("the resumed root is refused on its fresh journal")
        }
    };
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::StoreCommitSuperseded,
        "{refused:?}"
    );

    let root = law
        .store
        .root_of_input(&law.session_id, &overtaken_input)
        .await
        .expect("read the input's root")
        .expect("the overtaken input was admitted to a root");
    let terminal = law
        .store
        .root_terminal(&law.session_id, &root)
        .await
        .expect("read the root's terminal")
        .expect("the overtaken root ended");
    assert!(
        matches!(
            &terminal.cause,
            crate::store::RootTerminalCause::Refused { code, .. }
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
        "an overtaken root never parks"
    );
    assert!(
        law.store
            .unfinished_root(&law.session_id)
            .await
            .expect("read the unfinished root")
            .is_none(),
        "the overtaken root no longer holds the session"
    );
    assert!(
        law.store
            .list_pending_turn_inputs(&law.session_id)
            .await
            .expect("read pending input")
            .iter()
            .all(|row| row.input.input_id != overtaken_input),
        "the overtaken root's input is answered"
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        1,
        "the journaled summary is never requested again"
    );
    assert_eq!(
        model.turn_calls.load(Ordering::SeqCst),
        1,
        "the overtaken root made no model call"
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
    law.drive_on("root-3", path)
        .await
        .expect("the next drive admits a new root, which commits");
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
        "the new root's pressure frame opens: {chain:?}"
    );
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    let path_texts = active_path(&head.graph);
    assert!(
        path_texts.iter().any(|text| text == "answer 2"),
        "{path_texts:?}"
    );
}
