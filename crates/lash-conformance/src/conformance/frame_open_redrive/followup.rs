//! FIG-4201: the administrative compaction is a session command applied at
//! a turn boundary. Its crash matrix, the production compactor's, a
//! compaction queued before an input, and a compaction submitted while a
//! bound turn holds its pressure frame.

use super::*;
use pretty_assertions::assert_eq;

pub(super) struct CompactorCrash {
    pub(super) crash: FrameOpenCrash,
}

impl lash_core::facade_support::TraceSink for CompactorCrash {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        if matches!(
            (&record.event, self.crash),
            (
                crate::TraceEvent::CompactionStarted { .. },
                FrameOpenCrash::BeforeSummary
            ) | (
                crate::TraceEvent::CompactionCompleted { .. },
                FrameOpenCrash::AfterSummary
            )
        ) {
            panic!("injected production compactor crash at {:?}", self.crash);
        }
        Ok(())
    }
}

/// A changed prompt hook after a journaled summary parks the administrative
/// command without settling it or publishing a frame (FIG-4572).
#[expect(
    clippy::expect_used,
    reason = "conformance fixture results are established by setup"
)]
pub async fn a_compaction_redriven_with_a_changed_prompt_parks_without_settling(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            (protocol.answer("answer 1"), 1),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        "compaction-prompt-divergence",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = LawCompactor::Standard;
    for root in ["root-1", "root-2"] {
        law.enqueue(root).await;
        law.run_root(root).await;
    }
    let before = law.head().await;
    let receipt = law.submit_compaction("compact").await;
    let prompt_plugin = |text: &'static str| {
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-compaction-prompt",
            crate::facade_support::PluginSpec::new().with_prompt_contributor(Arc::new(
                move |_ctx| {
                    Box::pin(async move {
                        Ok(vec![crate::PromptContribution::environment(
                            "Overlay", text,
                        )])
                    })
                },
            )),
        )) as Arc<dyn PluginFactory>
    };
    law.parts.host_plugins.push(prompt_plugin("first worker"));
    let crashing = drive_attempt(&law.parts, Some(FrameOpenCrash::AfterSummary), None);
    law.parts.host_plugins = vec![prompt_plugin("replacement worker")];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        law.runner.run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &law.session_id,
                format!("{}-compact", law.prefix),
            )),
            crashing,
            drive_attempt(&law.parts, None, Some(tx)),
        ),
    )
    .await
    .expect("the divergent redrive stops");
    let redrive = rx.recv().await.expect("the redrive executed");
    let settlement = law.compaction_outcome(&receipt).await;
    assert!(
        settlement.is_none(),
        "a journaled summary's divergent replay must never settle Failed: {settlement:?}"
    );
    let error = redrive.expect_err("the divergent command aborts");
    assert_eq!(error.code, crate::RuntimeErrorCode::EffectReplayDivergence);
    assert_eq!(error.turn_failure_cause(), crate::TurnFailureCause::Parked);
    let park = law
        .store
        .load_turn_park(&law.session_id)
        .await
        .expect("read the command root's park")
        .expect("the command root parks for an operator");
    let crate::store::ParkReason::EffectReplayDivergence {
        effect_kind,
        message,
    } = &park.reason
    else {
        panic!("the command keeps its own park classification: {park:?}");
    };
    assert_eq!(effect_kind, "direct");
    assert!(
        message.contains("command.request.instructions"),
        "{message}"
    );
    assert!(
        law.store
            .root_terminal(&law.session_id, &park.turn_id)
            .await
            .expect("read the command root's terminal")
            .is_none(),
        "a parked command remains non-terminal"
    );
    let after = law.head().await;
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.current_frame_node_id, before.current_frame_node_id);
    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 2);
}

/// An administrative compaction submitted to the command lane, then applied
/// by a drive killed at `crash` and redriven. With `input_after`, an input
/// queued behind the compaction runs in the same drive, after it.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture results are established by setup"
)]
pub(super) async fn compaction_crash_case(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
    compactor: LawCompactor,
    input_after: bool,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let mut turns = if compactor == LawCompactor::Standard {
        vec![
            (protocol.answer("answer 1"), 1),
            (protocol.answer("answer 2"), 1),
        ]
    } else {
        // The first root's usage crosses the pressure hook's threshold: an
        // input the compaction did not reset the prompt usage for would
        // compact again.
        vec![(protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS)]
    };
    if input_after {
        turns.push((protocol.answer("answer after the compaction"), 1));
    }
    let model = law_model(ModelScript { turns });
    let mut law = LawSession::open(
        prefix,
        &format!(
            "compact-{}{crash:?}",
            if input_after { "before-input-" } else { "" }
        )
        .to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = compactor;
    law.enqueue("first question").await;
    law.run_root("root-1").await;
    if compactor == LawCompactor::Standard {
        // The production admin compactor keeps the latest user segment. Two
        // committed roots give it an earlier segment to summarize.
        law.enqueue("second question").await;
        law.run_root("root-2").await;
    }
    let first = law.head().await;
    let first_frame = first
        .current_frame_node_id
        .clone()
        .expect("the session stands in its first frame");

    let receipt = law.submit_compaction("compact").await;
    if input_after {
        law.enqueue("a question after the compaction").await;
    }
    model.arm(crash, &mut law.parts);
    let drain = law.drive_crashed_at("compact", crash).await;
    if input_after {
        drain
            .ran()
            .expect("the input queued behind the compaction runs in the redriven drive");
    }

    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "one frame after the first: {chain:?}");
    assert_eq!(chain[1].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    assert_eq!(
        law.compaction_outcome(&receipt).await,
        Some(crate::CompactContextOutcome::Opened {
            frame_node_id: chain[1].2.clone(),
        }),
        "the command settles with the frame it opened"
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        model.expected_summary_calls(1, Some(crash)),
        "one summarizer call: a redrive reads the summary back from its journal, \
         requests only an answer the journal never recorded again, and the input \
         after the compaction sees no stale prompt usage"
    );
    assert_eq!(
        head.head_revision,
        first.head_revision + 1 + u64::from(input_after),
        "the compaction commits once, and the input's root once"
    );
    let path = active_path(&head.graph);
    let seed_at = path
        .iter()
        .position(|text| text.ends_with(SUMMARY_TEXT))
        .expect("the seed is on the path");
    assert_eq!(
        path.iter()
            .filter(|text| text.ends_with(SUMMARY_TEXT))
            .count(),
        1,
        "{path:?}"
    );
    if input_after {
        assert!(
            path[seed_at..]
                .iter()
                .any(|text| text == "a question after the compaction"),
            "the compaction queued before the input applies before it: {path:?}"
        );
    }
    let requests = model
        .summary_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(requests.len(), model.expected_summary_calls(1, Some(crash)));
    assert!(
        requests.windows(2).all(|pair| pair[0] == pair[1]),
        "the compaction session and turn ids remain stable: {requests:?}"
    );
    if compactor == LawCompactor::Standard {
        assert!(
            requests[0]
                .0
                .starts_with(&format!("{}-compaction:", law.session_id))
        );
        assert!(requests[0].1.contains(":standard-compaction:"));
    }
}

/// A production administrative compaction killed before summarization,
/// between the provider and journal, before its frame commit and after it.
/// Each case opens exactly once.
pub async fn compact_with_production_compactor_crash_matrix(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for crash in [
        FrameOpenCrash::BeforeSummary,
        FrameOpenCrash::AfterProviderAnswer,
        FrameOpenCrash::AfterSummary,
        FrameOpenCrash::AfterFrameCommit,
    ] {
        let case = format!("{prefix}-production-admin");
        compaction_crash_case(
            &case,
            Arc::clone(&effect_host),
            Arc::clone(&stores),
            Arc::clone(&runner),
            crash,
            LawCompactor::Standard,
            false,
        )
        .await;
    }
}

/// Where [`an_administrative_compaction_waits_for_the_bound_turn`] kills an
/// execution: the bound turn's drive, or the drive that applies the
/// compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundTurnCrash {
    /// The bound turn's pressure summary is journaled; its frame's commit
    /// is not.
    PressureBeforeCommit,
    /// The bound turn's pressure frame committed; its turn has not.
    PressureAfterCommit,
    /// The bound turn committed; its root has not ended.
    AfterTurnCommit,
    /// The compaction's base is recorded; its summarizer has not run.
    CompactionBeforeSummary,
    /// The compaction's summarizer answered; its answer is not journaled.
    CompactionAfterProviderAnswer,
    /// The compaction's summary is journaled; its commit is not.
    CompactionAfterSummary,
    /// The compaction's commit settled its command; its drive has not gone
    /// on.
    CompactionAfterFrameCommit,
}

impl BoundTurnCrash {
    /// The crash in the bound turn's own drive.
    fn in_bound_turn(self) -> Option<FrameOpenCrash> {
        match self {
            Self::PressureBeforeCommit => Some(FrameOpenCrash::AfterSummary),
            Self::PressureAfterCommit => Some(FrameOpenCrash::AfterFrameCommit),
            Self::AfterTurnCommit => Some(FrameOpenCrash::AfterTurnCommit),
            _ => None,
        }
    }

    /// The crash in the drive that applies the compaction.
    fn in_compaction(self) -> Option<FrameOpenCrash> {
        match self {
            Self::CompactionBeforeSummary => Some(FrameOpenCrash::BeforeSummary),
            Self::CompactionAfterProviderAnswer => Some(FrameOpenCrash::AfterProviderAnswer),
            Self::CompactionAfterSummary => Some(FrameOpenCrash::AfterSummary),
            Self::CompactionAfterFrameCommit => Some(FrameOpenCrash::AfterFrameCommit),
            _ => None,
        }
    }
}

/// The bound turn owns the session head (FIG-4201). An administrative
/// compaction submitted while a root holds its journaled pressure summary
/// never moves the head: it waits on the command lane. Released, the root
/// commits its pressure frame and its turn, and the next drive applies the
/// compaction at the boundary, before the input queued after it. The chain
/// is the first frame, the pressure frame, the compaction frame; each
/// summary is requested once (twice only when a crash lost a paid answer
/// before its journal record); the compaction reset the prompt usage, so
/// the next root's pressure hook never compacts on the bound turn's usage;
/// and on a protocol with live execution state, a global the bound turn set
/// in the pressure frame is gone from the interpreter the next root runs
/// in. Killed at `crash` and redriven, the session ends the same way.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture results are established by setup"
)]
pub async fn an_administrative_compaction_waits_for_the_bound_turn(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Arc<dyn FrameLawProtocol>,
    crash: Option<BoundTurnCrash>,
) {
    let script = protocol.execution_state();
    let set_global = |text: &str| {
        script
            .as_ref()
            .map_or_else(|| protocol.answer(text), |script| script.set_global.clone())
    };
    let model = law_model(ModelScript {
        turns: vec![
            // The first root's usage crosses the pressure hook's threshold,
            // so the bound turn opens a pressure frame.
            (set_global("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            // The bound turn's usage crosses it too: a root that saw it
            // after the compaction would compact again.
            (set_global("answer 2"), PRESSURE_THRESHOLD_TOKENS),
            (
                script.as_ref().map_or_else(
                    || protocol.answer("answer 3"),
                    |script| script.answer_global_type.clone(),
                ),
                1,
            ),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("bound-turn-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    let hold = SummaryHold::default();
    law.parts.compaction.pressure_hold = Some(hold.clone());
    law.enqueue("first question").await;
    law.run_root("root-1").await;
    let before = law.head().await;
    let first_frame = before
        .current_frame_node_id
        .clone()
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;

    let submitted = std::sync::OnceLock::new();
    let bound_turn = async {
        match crash.and_then(BoundTurnCrash::in_bound_turn) {
            Some(crash) => law.run_root_crashed_at("root-2", crash).await,
            None => {
                law.run_root("root-2").await;
            }
        }
    };
    let while_held = hold.while_held(async {
        let receipt = law.submit_compaction("compact").await;
        let during = law.head().await;
        assert_eq!(
            during.head_revision, before.head_revision,
            "the head does not move while the bound turn holds its pressure summary"
        );
        assert_eq!(
            law.compaction_outcome(&receipt).await,
            None,
            "the compaction waits on the command lane"
        );
        submitted
            .set(receipt)
            .expect("the compaction is submitted once");
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(bound_turn, while_held),
    )
    .await
    .expect("the bound turn's drive ends");
    let receipt = submitted
        .into_inner()
        .expect("the compaction was submitted while the pressure summary was held");
    let after_bound_turn = law.head().await;
    let chain = frame_chain(&after_bound_turn, &law.session_id);
    assert_eq!(chain.len(), 2, "the bound turn's pressure frame: {chain:?}");
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    assert_eq!(
        after_bound_turn.head_revision,
        before.head_revision + 2,
        "the pressure frame and the bound turn each commit once, before the compaction"
    );

    law.enqueue("third question").await;
    let outcome = match crash.and_then(BoundTurnCrash::in_compaction) {
        Some(crash) => {
            model.arm(crash, &mut law.parts);
            law.drive_crashed_at("root-3", crash)
                .await
                .ran()
                .expect("the root queued after the compaction runs")
                .outcome
        }
        None => law.run_root("root-3").await,
    };

    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(
        chain.len(),
        3,
        "the first frame, the pressure frame, then the compaction frame: {chain:?}"
    );
    assert_eq!(chain[1].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    assert_eq!(chain[2].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(
        chain[2].1.as_ref(),
        Some(&chain[1].2),
        "the compaction frame follows the pressure frame"
    );
    assert_eq!(
        law.compaction_outcome(&receipt).await,
        Some(crate::CompactContextOutcome::Opened {
            frame_node_id: chain[2].2.clone(),
        })
    );
    assert_eq!(
        head.head_revision,
        after_bound_turn.head_revision + 2,
        "the compaction and the next root each commit once"
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        2 + usize::from(crash == Some(BoundTurnCrash::CompactionAfterProviderAnswer)),
        "the pressure summary and the compaction's, each journaled once and never requested \
         again; the next root saw no stale prompt usage"
    );
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 3);
    let path = active_path(&head.graph);
    assert_eq!(
        path.iter().filter(|text| *text == "FrameOpen").count(),
        3,
        "{path:?}"
    );
    let compaction_seed_at = path
        .iter()
        .rposition(|text| text.ends_with(SUMMARY_TEXT))
        .expect("the compaction's seed is on the path");
    assert!(
        path[compaction_seed_at..]
            .iter()
            .any(|text| text == "third question"),
        "the root queued after the compaction runs in the compaction frame: {path:?}"
    );
    if law.parts.protocol.execution_state().is_some() {
        assert!(
            matches!(
                outcome,
                crate::TurnOutcome::Finished(crate::TurnFinish::FinalValue { ref value })
                    if value == "undefined"
            ),
            "the compaction restarted the live interpreter the next root runs in: {outcome:?}"
        );
    }
}
