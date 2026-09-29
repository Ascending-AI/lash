//! FIG-4165: the reverse overlap, sealed redrive and production admin crash laws.

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

#[expect(
    clippy::expect_used,
    reason = "conformance fixture results are established by setup"
)]
async fn seal_newer_admission(store: &Arc<dyn crate::RuntimeStore>, session_id: &SessionId) {
    let current = store
        .drive_epoch(session_id)
        .await
        .expect("read drive epoch");
    let seal = store
        .seal_drive_epoch(
            session_id,
            &crate::store::AdmissionId::new(format!("{session_id}-sealed-after-commit")),
            current.epoch,
            &crate::store::RootStartNonce::new(format!("{session_id}-sealed-start")),
        )
        .await
        .expect("seal an admission after the frame committed");
    assert!(
        matches!(seal, crate::store::DriveEpochSeal::Sealed(_)),
        "{seal:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture results are established by setup"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "a crash case specifies its tier, compactor and admission race"
)]
pub(super) async fn compaction_crash_case(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
    compactor: LawCompactor,
    seal_after_commit: bool,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: if compactor == LawCompactor::Standard {
            vec![
                (protocol.answer("answer 1"), 1),
                (protocol.answer("answer 2"), 1),
            ]
        } else {
            vec![(protocol.answer("answer 1"), 1)]
        },
    });
    let mut law = LawSession::open(
        prefix,
        &format!("compact-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = compactor;
    model.arm(crash, &mut law.parts);
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

    let compaction =
        |crash: Option<FrameOpenCrash>,
         result_tx: Option<tokio::sync::mpsc::UnboundedSender<bool>>| {
            let parts = law.parts.clone();
            let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
                let parts = parts.clone();
                let result_tx = result_tx.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(&parts, crash).await;
                    let opened = unless_the_provider_answer_is_lost(
                        &parts,
                        crash,
                        Box::pin(runtime.compact_context(None, scope)),
                    )
                    .await
                    .unwrap_or_else(|error| panic!("the compaction runs: {error}"));
                    match result_tx {
                        Some(result_tx) => {
                            let _ = result_tx.send(opened);
                            crate::ConformanceTurnEnd::Settled
                        }
                        None => {
                            if seal_after_commit {
                                seal_newer_admission(&parts.store, &parts.session_id).await;
                            }
                            panic!("injected crash after the compaction's commit")
                        }
                    }
                })
            });
            attempt
        };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        law.runner.run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::runtime_operation(format!(
                "{}-compact",
                law.prefix
            ))),
            // Killed after its commit, the crashing attempt runs the whole
            // compaction and dies before it reports.
            compaction(
                (crash != FrameOpenCrash::AfterFrameCommit).then_some(crash),
                None,
            ),
            compaction(None, Some(tx)),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("the compaction's redrive after {crash:?} ends"));
    assert!(
        rx.recv()
            .await
            .expect("the tier's runner ran the redriven compaction"),
        "the redriven compaction reports its frame open"
    );

    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        model.expected_summary_calls(1, Some(crash)),
        "one summarizer call: a redrive reads the summary back from its journal, \
         and requests only an answer the journal never recorded again"
    );
    let head = law.head().await;
    assert_eq!(
        head.head_revision,
        first.head_revision + 1,
        "the compaction commits once"
    );
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "one frame after the first: {chain:?}");
    assert_eq!(chain[1].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    let path = active_path(&head.graph);
    assert_eq!(
        path.iter()
            .filter(|text| text.ends_with(SUMMARY_TEXT))
            .count(),
        1,
        "{path:?}"
    );
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

/// A committed admin frame survives a newer admission sealing before redrive.
/// The old fence is refused, but the durable head proves the open already landed.
pub async fn redrive_after_commit_with_sealed_admission_reports_opened(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    compaction_crash_case(
        prefix,
        effect_host,
        stores,
        runner,
        FrameOpenCrash::AfterFrameCommit,
        LawCompactor::Law,
        true,
    )
    .await;
}

/// Production `/compact` killed before summarization, between the provider and
/// journal, before its frame commit and after it. Each case opens exactly once.
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

/// Pressure loses its head CAS to `/compact`, and redrives without a second
/// frame, stale prompt usage or the old interpreter global.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture results are established by setup"
)]
pub async fn reverse_compact_pressure_overlap_redrives_once(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Arc<dyn FrameLawProtocol>,
) {
    let script = protocol.execution_state();
    let model = law_model(ModelScript {
        turns: vec![
            (
                script.as_ref().map_or_else(
                    || protocol.answer("answer 1"),
                    |script| script.set_global.clone(),
                ),
                PRESSURE_THRESHOLD_TOKENS,
            ),
            (
                script.map_or_else(
                    || protocol.answer("answer 2"),
                    |script| script.answer_global_type,
                ),
                1,
            ),
            (protocol.answer("answer 3"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        "reverse-overlap",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.pressure_hold = Some(SummaryHold::default());
    law.enqueue("first question").await;
    law.run_root("root-1").await;
    let before = law.head().await;
    law.enqueue("second question").await;

    let hold = law
        .parts
        .compaction
        .pressure_hold
        .clone()
        .expect("hold pressure summary");
    let compact_parts = law.parts.clone();
    let compact_runner = Arc::clone(&law.runner);
    let compact_scope = admit(crate::ExecutionScope::runtime_operation(format!(
        "{}-compact",
        law.prefix
    )));
    let during = hold.while_held(async move {
        let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
            let parts = compact_parts.clone();
            Box::pin(async move {
                let mut runtime = build_runtime(&parts, None).await;
                assert!(
                    Box::pin(runtime.compact_context(None, scope))
                        .await
                        .expect("admin compaction wins while pressure is held")
                );
                assert!(
                    runtime.state().last_prompt_usage.is_none(),
                    "the winning frame clears the old prompt usage"
                );
                crate::ConformanceTurnEnd::Settled
            })
        });
        compact_runner.run_turn(compact_scope, attempt).await;
    });
    let (refused, ()) = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(law.run_root_to_any_end("root-2"), during),
    )
    .await
    .expect("the overlapping drive ends");
    let failure = refused.expect_err("pressure meets the winning admin frame's head CAS");
    assert_eq!(failure.code, crate::RuntimeErrorCode::StoreCommitSuperseded);
    assert!(
        failure.to_string().contains("head revision conflict"),
        "{failure}"
    );
    // A superseded root ends typed. Its engine does not retry the recorded
    // admission against the same moved head; recovery admits a new root.
    let outcome = law.run_root("root-2-redrive").await;
    if law.parts.protocol.execution_state().is_some() {
        assert!(
            matches!(outcome, crate::TurnOutcome::Finished(
            crate::TurnFinish::FinalValue { ref value }) if value == "undefined"),
            "the redrive reset the live interpreter: {outcome:?}"
        );
    }
    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(
        chain.len(),
        2,
        "only the winning compaction frame: {chain:?}"
    );
    assert_eq!(chain[1].1, before.current_frame_node_id);
    assert_eq!(head.head_revision, before.head_revision + 2);
    assert_eq!(
        active_path(&head.graph)
            .iter()
            .filter(|text| *text == SUMMARY_TEXT)
            .count(),
        1
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        2,
        "both summaries are journaled once, and neither is re-requested"
    );
    law.enqueue("third question").await;
    law.run_root("root-3").await;
    assert_eq!(
        frame_chain(&law.head().await, &law.session_id).len(),
        2,
        "the next root sees no stale prompt usage"
    );
    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 2);
}
