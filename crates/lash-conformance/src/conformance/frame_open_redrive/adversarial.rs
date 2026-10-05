//! FIG-4134: the frame-open laws an adversarial review found missing.
//!
//! - The production standard compactor and its overflow recovery, killed at
//!   every crash point, replay exactly as the laws' synthetic compactor does:
//!   the redrive's admitted window hashes to the request identity the first
//!   execution journaled its summary under, so it reads the summary back.
//! - An administrative compaction queued before an input applies before it
//!   (FIG-4201), killed at every crash point: the input's run executes in the
//!   compaction frame and its pressure hook sees no stale prompt usage.
//! - A session deleted while an administrative compaction opens its frame
//!   keeps nothing of the open, and a fork made meanwhile sees the point it
//!   forked from, never a partial seed.
//! - An explicit empty pressure seed opens one frame; a pressure frame whose
//!   commit the store refuses leaves nothing of the open visible.
//! - Two plugins whose pressure hooks share an id keep their records apart.
//! - Every open restarts the live interpreter: a host's commanded open, an
//!   administrative compaction the command lane applies, and a storeless
//!   runtime's direct compaction.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

use super::{
    DUPLICATE_HOOK_PLUGINS, FrameLawProtocol, FrameOpenCrash, LawCompactor, LawModel, LawSeed,
    LawSession, ModelScript, OVERSIZED_RECORD_TEXT, PRESSURE_THRESHOLD_TOKENS, SUMMARY_TEXT,
    StandardFrameLawProtocol, SummaryHold, active_path, build_runtime, context_overflow,
    frame_chain, law_model,
};
use crate::admit;
use lash_core::testing::TestTurnExecution as _;

/// Prompt usage over the standard compactor's pressure threshold on the
/// laws' model (a 200k window less its 20k buffer).
pub(super) const STANDARD_PRESSURE_TOKENS: i64 = 190_000;
const OVERFLOW_RECOVERY_PLUGIN_TYPE: &str = "standard_compaction.overflow_recovery";

fn recovery_records(head: &super::LawHead) -> Vec<serde_json::Value> {
    use crate::facade_support::SessionNodeProjection as _;
    head.graph
        .nodes
        .iter()
        .filter_map(|node| {
            let (plugin_type, body) = node.plugin()?;
            (plugin_type == OVERFLOW_RECOVERY_PLUGIN_TYPE).then(|| body.clone())
        })
        .collect()
}

type ShiftResult =
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>;

impl LawSession {
    /// Submits an administrative compaction and runs the shift that applies
    /// it once on the tier's runner, holding the compaction after its
    /// journaled summary while `during` runs, and answers how the shift
    /// ended with the command's receipt.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn compact_holding<F: std::future::Future<Output = ()>>(
        &self,
        shift: &str,
        during: F,
    ) -> (ShiftResult, crate::SessionCommandReceipt) {
        let hold = self
            .parts
            .compaction
            .hold
            .clone()
            .expect("the law holds its compaction's summarizer");
        let receipt = self.submit_compaction(shift).await;
        let (drove, ()) = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            futures_util::future::join(self.execute_run_to_any_end(shift), hold.while_held(during)),
        )
        .await
        .expect("the held compaction's shift ends");
        (drove, receipt)
    }

    /// Executes the run queued next once and answers how its shift ended,
    /// whatever that was: the attempt reports itself settled either way.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(super) async fn execute_run_to_any_end(
        &self,
        shift: &str,
    ) -> Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let parts = self.parts.clone();
        let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
            let parts = parts.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = build_runtime(&parts, None).await;
                let shift = Box::pin(runtime.execute_next_queued_run(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )))
                .await;
                let _ = tx.send(shift);
                crate::ConformanceTurnEnd::Settled
            })
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner.run_turn(
                admit(crate::ExecutionScope::turn(
                    &self.session_id,
                    crate::TurnId::fixture(format!("{}-{shift}", self.prefix)),
                )),
                attempt,
            ),
        )
        .await
        .expect("the shift ends");
        rx.recv().await.expect("the tier's runner ran the shift")
    }
}

/// Every message text in `head`'s whole history, on the active path or off
/// it.
fn every_message(head: &super::LawHead) -> Vec<String> {
    use crate::facade_support::SessionNodeProjection as _;
    head.graph
        .nodes
        .iter()
        .filter_map(|node| {
            node.message().map(|message| {
                message
                    .parts
                    .iter()
                    .map(|part| part.content().into_owned())
                    .collect::<String>()
            })
        })
        .collect()
}

fn count(texts: &[String], wanted: impl Fn(&str) -> bool) -> usize {
    texts.iter().filter(|text| wanted(text)).count()
}

/// The production standard compactor, killed at `crash` in the run its
/// pressure threshold compacts and redriven, opens one frame: the summary
/// is journaled under the request identity the admitted window hashes to,
/// so the redrive reads it back (one summarizer call, or two when the crash
/// lost a paid answer before its journal record), and the seed lands once.
///
/// FIG-4072 (ADR 0112 §14.4): every execution of the run, the redrive of a
/// crash before the terminal commit included (it reloads the window the
/// run was admitted on, after the frame's commit moved the head), derives
/// the same compaction session id and turn id, and they are the ids the
/// summarizer's provider requests carry.
pub async fn a_standard_compaction_frame_opens_once_whatever_its_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    standard_pressure_crash_case(
        prefix,
        effect_host,
        stores,
        runner,
        crash,
        StandardFrameLawProtocol::shared(),
    )
    .await;
}

/// [`a_standard_compaction_frame_opens_once_whatever_its_crash`] over
/// `protocol`, answering the law's model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn standard_pressure_crash_case(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
    protocol: Arc<dyn FrameLawProtocol>,
) -> LawModel {
    let model = law_model(ModelScript {
        turns: vec![
            (protocol.answer("answer 1"), STANDARD_PRESSURE_TOKENS),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("standard-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = LawCompactor::Standard;
    let request_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    law.parts.compaction.request_ids = Some(Arc::clone(&request_ids));
    model.arm(crash, &mut law.parts);

    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;
    let before = law.head().await.head_revision;
    law.execute_run_crashed_at("run-2", crash).await;

    assert_standard_frame_opened_once(&law, &model, crash, before, &first_frame).await;
    let derived = request_ids
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    // Every crash but the one after the terminal commit leaves the run to a
    // redrive that prepares it again, from its admitted window.
    let executions = if crash == FrameOpenCrash::AfterTurnCommit {
        1
    } else {
        2
    };
    assert!(
        derived.len() >= executions,
        "each execution of the run derives its compaction's ids: {derived:?}"
    );
    assert!(
        derived.windows(2).all(|pair| pair[0] == pair[1]),
        "every execution derives the same compaction session id and turn id: {derived:?}"
    );
    let (session_id, turn_id) = &derived[0];
    assert!(
        session_id.starts_with(&format!("{}-compaction:", law.session_id)),
        "{session_id}"
    );
    assert!(turn_id.contains(":standard-compaction:"), "{turn_id}");
    let requested = model
        .summary_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(
        requested.iter().all(|request| request == &derived[0]),
        "the summarizer's requests carry the derived ids: {requested:?} vs {derived:?}"
    );
    model
}

/// What a standard-compaction frame's laws hold after the redrive.
async fn assert_standard_frame_opened_once(
    law: &LawSession,
    model: &LawModel,
    crash: FrameOpenCrash,
    before: u64,
    first_frame: &crate::FrameNodeId,
) {
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        model.expected_summary_calls(1, Some(crash)),
        "the redrive reads the summary back from its journal: its admitted window \
         hashes to the request identity the first execution recorded"
    );
    assert_eq!(
        model.turn_calls.load(Ordering::SeqCst),
        2,
        "one model call per run"
    );
    let head = law.head().await;
    law.receipts
        .assert_since(before, head.head_revision, 1, 1, 0, 1);
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "one frame after the first: {chain:?}");
    assert_eq!(chain[1].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(chain[1].1.as_ref(), Some(first_frame));
    let path = active_path(&head.graph);
    assert_eq!(
        count(&path, |text| text.ends_with(SUMMARY_TEXT)),
        1,
        "the seed lands once: {path:?}"
    );
    assert!(
        path.iter().any(|text| text == "second question"),
        "the compacted run executes in the new frame: {path:?}"
    );
}

/// The standard compactor's overflow recovery, killed at `crash` in the
/// run that recovers and redriven: the provider refused the first run as
/// too long, and the next run records `Completed` in the frame it leaves
/// and opens one recovery frame, each exactly once.
pub async fn an_overflow_recovery_frame_opens_once_whatever_its_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    overflow_recovery_crash_case(
        prefix,
        effect_host,
        stores,
        runner,
        crash,
        StandardFrameLawProtocol::shared(),
    )
    .await;
}

/// [`an_overflow_recovery_frame_opens_once_whatever_its_crash`] over
/// `protocol`, answering the law's model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn overflow_recovery_crash_case(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
    protocol: Arc<dyn FrameLawProtocol>,
) -> LawModel {
    let model = law_model(ModelScript {
        turns: vec![context_overflow(), (protocol.answer("answer 2"), 1)],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("overflow-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = LawCompactor::Standard;
    model.arm(crash, &mut law.parts);

    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let overflowed = law.head().await;
    let first_frame = overflowed
        .current_frame_node_id
        .clone()
        .expect("the session stands in its first frame");
    assert_eq!(
        recovery_records(&overflowed),
        [serde_json::json!({"format": 1, "record": {"kind": "pending"}})],
        "the refused run leaves a typed recovery node outside conversation"
    );
    assert!(
        !every_message(&overflowed)
            .iter()
            .any(|text| text.contains("context-overflow recovery marker"))
    );
    law.enqueue("second question").await;
    let before = overflowed.head_revision;
    law.execute_run_crashed_at("run-2", crash).await;

    assert_standard_frame_opened_once(&law, &model, crash, before, &first_frame).await;
    let head = law.head().await;
    assert_eq!(
        recovery_records(&head),
        [
            serde_json::json!({"format": 1, "record": {"kind": "pending"}}),
            serde_json::json!({"format": 1, "record": {"kind": "completed"}}),
        ],
        "recovery records commit once and store each kind once"
    );
    assert!(
        !every_message(&head)
            .iter()
            .any(|text| text.contains("context-overflow recovery marker"))
    );
    model
}

/// A failed summarizer invocation leaves the stored recovery pending. Repeated
/// live faults and replay divergences spend no attempt and keep their typed cause;
/// the next healthy shift can still complete the recovery.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: setup establishes each result"
)]
pub async fn an_overflow_recovery_summarizer_fault_aborts_without_a_record(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![context_overflow(), (protocol.answer("recovered answer"), 1)],
    });
    let mut law = LawSession::open(
        prefix,
        "recovery-fault",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = LawCompactor::Standard;
    law.enqueue("first question").await;
    law.execute_run("overflow").await;
    let before = law.head().await;
    for code in [
        crate::RuntimeErrorCode::RuntimeStore,
        crate::RuntimeErrorCode::LashlangCellReplayDivergence,
    ] {
        for _ in 0..lash_plugin_standard_compaction::OVERFLOW_RECOVERY_MAX_ATTEMPTS {
            let runtime = build_runtime(&law.parts, None).await;
            let state = crate::conformance::helpers::load_window_state(&law.store, &law.session_id)
                .await
                .expect("read stored pending recovery")
                .expect("the overflow committed");
            let injected = code.clone();
            let ctx = crate::plugin::ContextPressureContext {
                writer_formats: lash_sansio::build_newest_writer_formats(),
                session_id: law.session_id.clone(),
                plugin_config: state.admitted_plugin_config(),
                state: state.read_view(),
                prompt_usage: None,
                max_context_tokens: Some(200_000),
                traces: crate::plugin::PluginTraceEmitter::discard(),
                scoped_effect_controller: crate::ScopedEffectController::shared(
                    Arc::new(crate::testing::UnavailableEffectController),
                    crate::AdmittedScope::runtime_operation("recovery-fault-law"),
                )
                .expect("scoped faulting completion"),
                direct_completions: crate::DirectCompletionClient::from_llm_fn(move |_, _| {
                    Err(crate::PluginError::Runtime(crate::RuntimeError::new(
                        injected.clone(),
                        "injected summarizer journal fault",
                    )))
                }),
                system_prompt: None,
            };
            let error = runtime
                .services
                .plugins
                .decide_context_pressure(&ctx, None)
                .await
                .expect_err("a fault must raise instead of deciding a failed recovery record");
            let crate::plugin::ContextError::Plugin(crate::PluginError::Runtime(error)) = error
            else {
                panic!("the summarizer fault keeps its runtime cause: {error:?}");
            };
            assert_eq!(error.code, code);
            assert_eq!(
                law.head().await.head_revision,
                before.head_revision,
                "an aborted recovery writes nothing"
            );
        }
    }
    law.enqueue("continue after the journal recovers").await;
    law.execute_run("healthy").await;
    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        recovery_records(&law.head().await),
        [
            serde_json::json!({"format": 1, "record": {"kind": "pending"}}),
            serde_json::json!({"format": 1, "record": {"kind": "completed"}}),
        ]
    );
}

/// An administrative compaction queued before an input applies before it
/// (FIG-4201): one shift applies the command at its boundary, then runs the
/// input's run in the compaction frame. The run's pressure hook sees no
/// stale prompt usage, though the run before the compaction crossed its
/// threshold. Killed at `crash` and redriven, the compaction opens once, its
/// summary is requested once (twice only when the crash lost a paid answer
/// before its journal record), and the input runs once, after it.
pub async fn a_compaction_queued_before_an_input_applies_before_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    super::followup::compaction_crash_case(
        prefix,
        effect_host,
        stores,
        runner,
        crash,
        LawCompactor::Law,
        true,
    )
    .await;
}

/// A session deleted while an administrative compaction opens its frame keeps
/// nothing of the open: the compaction fails, and the session stays deleted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_session_deleted_during_an_open_keeps_nothing_of_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![(protocol.answer("answer 1"), 1)],
    });
    let mut law = LawSession::open(
        prefix,
        "compact-deleted",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.hold = Some(SummaryHold::default());
    law.enqueue("first question").await;
    law.execute_run("run-1").await;

    let store = Arc::clone(&law.store);
    let session_id = law.session_id.clone();
    let (drove, _receipt) = law
        .compact_holding("compact", async move {
            store
                .delete_session(&session_id)
                .await
                .expect("delete the session while its frame opens");
        })
        .await;
    assert!(
        drove.is_err(),
        "a compaction of a deleted session commits nothing: {:?}",
        drove.map(crate::facade_support::QueuedTurnDrain::ran)
    );
    assert!(
        matches!(
            law.store
                .lookup_session(&law.session_id)
                .await
                .expect("look the session up"),
            crate::SessionLookup::Deleted
        ),
        "the open did not bring the deleted session back"
    );
}

/// A fork made at the head while an administrative compaction opens its
/// frame sees the point it forked from: the compaction commits its frame in
/// the source session, and the fork holds no part of the seed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_fork_made_during_an_open_never_sees_its_seed(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![(protocol.answer("answer 1"), 1)],
    });
    let mut law = LawSession::open(
        prefix,
        "compact-forked",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.hold = Some(SummaryHold::default());
    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let forked_from = law.head().await;
    let fork_id = SessionId::fixture(format!("{}-fork", law.session_id));

    let store = Arc::clone(&law.store);
    let fork = fork_id.clone();
    let source = law.session_id.clone();
    let forked_revision = forked_from.head_revision;
    let (drove, receipt) = law
        .compact_holding("compact", async move {
            store
                .fork_session(&crate::ForkSessionRequest {
                    session_id: fork,
                    source_session_id: source,
                    head_revision: forked_revision,
                    relation: crate::SessionRelation::Root,
                    pending_observer_intents: Vec::new(),
                    config: crate::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    )
                    .into(),
                })
                .await
                .expect("fork the session while its frame opens");
        })
        .await;
    assert!(
        drove.is_ok(),
        "the compaction's shift ends: {:?}",
        drove.map(crate::facade_support::QueuedTurnDrain::ran)
    );
    let source = law.head().await;
    let chain = frame_chain(&source, &law.session_id);
    assert_eq!(chain.len(), 2);
    assert_eq!(
        law.compaction_outcome(&receipt).await,
        Some(crate::CompactContextOutcome::Opened {
            frame_node_id: chain[1].2.clone(),
        }),
        "the compaction commits its frame"
    );
    assert_eq!(
        count(&active_path(&source.graph), |text| text == SUMMARY_TEXT),
        1
    );
    let forked = law.head_of(&fork_id).await;
    assert_eq!(
        active_path(&forked.graph),
        active_path(&forked_from.graph),
        "the fork holds the point it forked from, and nothing of the seed"
    );
    assert_eq!(
        count(&every_message(&forked), |text| text == SUMMARY_TEXT),
        0
    );
}

/// An explicit empty pressure seed opens one frame, killed at `crash` and
/// redriven: no summary, one frame after the first, and the run executes in it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_empty_pressure_seed_opens_one_frame(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            (protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("empty-seed-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.seed = LawSeed::Empty;
    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;
    let before = law.head().await.head_revision;
    law.execute_run_crashed_at("run-2", crash).await;

    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 0);
    let head = law.head().await;
    law.receipts
        .assert_since(before, head.head_revision, 1, 1, 0, 1);
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "{chain:?}");
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    let path = active_path(&head.graph);
    let opened_at = path
        .iter()
        .rposition(|text| text == "FrameOpen")
        .expect("the frame is on the path");
    assert_eq!(
        path[opened_at + 1..],
        ["second question", "answer 2"],
        "the empty seed adds nothing, and the run executes in the new frame"
    );
}

/// A pressure frame whose own commit the store refuses (its seed is past
/// the commit's node budget) leaves nothing of the open visible: no frame,
/// no seed, and no record in the frame it would have left.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_refused_frame_commit_leaves_nothing_visible(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![(protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS)],
    });
    let mut law = LawSession::open(
        prefix,
        "refused-frame-commit",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.seed = LawSeed::Oversized;
    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;
    let shift = law
        .execute_run_to_any_end("run-2")
        .await
        .map(crate::facade_support::QueuedTurnDrain::ran);

    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(
        chain.len(),
        1,
        "no frame opened: {chain:?} (the run: {shift:?})"
    );
    assert_eq!(head.current_frame_node_id.as_ref(), Some(&first_frame));
    let messages = every_message(&head);
    assert_eq!(
        count(&messages, |text| text == SUMMARY_TEXT
            || text == OVERSIZED_RECORD_TEXT
            || text.starts_with("oversized seed")),
        0,
        "nothing of the refused open is durable: {messages:?}"
    );
}

/// Two plugins whose pressure hooks share an id each record on every turn,
/// killed at `crash` and redriven: each plugin's record lands once, in its
/// own namespace.
pub async fn pressure_hooks_sharing_an_id_keep_their_records_apart(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![(protocol.answer("answer 1"), 1)],
    });
    let mut law = LawSession::open(
        prefix,
        &format!("duplicate-hook-ids-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.compactor = LawCompactor::DuplicateHookIds;
    law.enqueue("first question").await;
    law.execute_run_crashed_at("run-1", crash).await;

    let path = active_path(&law.head().await.graph);
    for (plugin_id, record) in DUPLICATE_HOOK_PLUGINS {
        assert_eq!(
            count(&path, |text| text == record),
            1,
            "{plugin_id}'s record lands once: {path:?}"
        );
    }
}

/// How [`every_open_restarts_the_live_execution_state`] opens its frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveResetPath {
    /// A host's frame open: a session command the shift applies at a turn
    /// boundary, on the runtime whose shift applies it (FIG-4202).
    HostOpen,
    /// An administrative compaction the command lane applies, on the
    /// runtime whose shift applies it.
    Compact,
    /// A storeless runtime's direct compaction.
    StorelessCompact,
}

/// Whether the live interpreter holds `global`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn live_holds(runtime: &mut crate::LashRuntime, global: &str) -> bool {
    let Some(state) = runtime
        .snapshot_execution_state()
        .await
        .expect("read the live execution state")
    else {
        return false;
    };
    let holds = |bytes: &[u8]| {
        bytes
            .windows(global.len())
            .any(|window| window == global.as_bytes())
    };
    holds(&state.root) || state.components.values().any(|bytes| holds(bytes))
}

/// Every accepted open restarts the live interpreter from the new frame's
/// seed (F5), whichever path opens it: a global the first root set, which a
/// fresh runtime restores into its interpreter, is gone from the live
/// interpreter once the frame opens.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn every_open_restarts_the_live_execution_state(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Arc<dyn FrameLawProtocol>,
    path: LiveResetPath,
) {
    let script = protocol
        .execution_state()
        .expect("the protocol under test has live execution state");
    let global = script.global;
    let model = law_model(ModelScript {
        turns: vec![(script.set_global, 1), (protocol.answer("restored"), 1)],
    });
    let law = LawSession::open(
        prefix,
        &format!("live-reset-{path:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.enqueue("set the global").await;
    law.execute_run("run-1").await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let parts = law.parts.clone();
    let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
        let parts = parts.clone();
        let tx = tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(&parts, None).await;
            let before = live_holds(&mut runtime, global).await;
            let mut runtime = match path {
                LiveResetPath::HostOpen => {
                    let command = crate::SessionCommand::OpenAgentFrame {
                        request: Box::new(crate::OpenAgentFrameRequest::new(
                            crate::FrameKey::from_caller_material("frame-open-law-host-open")
                                .expect("non-empty frame material"),
                            crate::AgentFrameReason::new("host_open"),
                        )),
                    };
                    parts
                        .store
                        .enqueue_queued_work(
                            crate::QueuedWorkBatchDraft::new(
                                parts.session_id.clone(),
                                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                                command.clone(),
                            )
                            .with_source_key(command.source_key("live-reset")),
                        )
                        .await
                        .expect("accept the frame-open command");
                    let drained = Box::pin(runtime.execute_next_queued_run(
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    ))
                    .await
                    .expect("the shift applies the frame open");
                    assert!(
                        drained.ran().is_none(),
                        "the shift only applies the command lane"
                    );
                    runtime
                }
                LiveResetPath::Compact => {
                    let command = crate::SessionCommand::CompactContext { instructions: None };
                    parts
                        .store
                        .enqueue_queued_work(
                            crate::QueuedWorkBatchDraft::new(
                                parts.session_id.clone(),
                                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                                command.clone(),
                            )
                            .with_source_key(command.source_key("live-reset")),
                        )
                        .await
                        .expect("accept the compaction command");
                    let drained = Box::pin(runtime.execute_next_queued_run(
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    ))
                    .await
                    .expect("the shift applies the compaction");
                    assert!(
                        drained.ran().is_none(),
                        "the shift only applies the command lane"
                    );
                    runtime
                }
                LiveResetPath::StorelessCompact => {
                    let snapshot = runtime
                        .snapshot_execution_state()
                        .await
                        .expect("read the live execution state")
                        .expect("the first run left execution state");
                    let mut storeless_parts = parts.clone();
                    storeless_parts.compaction.storeless = true;
                    let mut storeless = build_runtime(&storeless_parts, None).await;
                    storeless
                        .restore_execution_state(&snapshot)
                        .await
                        .expect("restore the live execution state without a store");
                    let activated = Box::pin(
                        storeless.execute_turn(
                            crate::TurnInput::text("activate the restored interpreter"),
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope
                                    .rescope(admit(crate::ExecutionScope::turn(
                                        &parts.session_id,
                                        crate::TurnId::fixture("restore-activation"),
                                    )))
                                    .expect("admit the restored executor's activation turn"),
                            ),
                        ),
                    )
                    .await
                    .expect("the recorded turn activates the restored executor");
                    assert!(
                        matches!(activated.outcome, crate::TurnOutcome::Finished(_)),
                        "{activated:?}"
                    );
                    assert!(live_holds(&mut storeless, global).await);
                    assert!(
                        matches!(
                            Box::pin(storeless.compact_storeless_context(None, scope))
                                .await
                                .expect("the storeless compaction runs"),
                            crate::CompactContextOutcome::Opened { .. }
                        ),
                        "the storeless compaction opens its frame"
                    );
                    storeless
                }
            };
            let after = live_holds(&mut runtime, global).await;
            let _ = tx.send((before, after));
            crate::ConformanceTurnEnd::Settled
        })
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        law.runner.run_turn(
            admit(crate::ExecutionScope::turn(
                &law.session_id,
                crate::TurnId::fixture(format!("{}-open", law.prefix)),
            )),
            attempt,
        ),
    )
    .await
    .expect("the open ends");
    let (before, after) = rx.recv().await.expect("the tier's runner ran the open");
    if path == LiveResetPath::Compact {
        assert_eq!(
            frame_chain(&law.head().await, &law.session_id).len(),
            2,
            "the command lane applied the compaction"
        );
    }
    assert!(
        before,
        "the reopened runtime's interpreter holds the first run's global"
    );
    assert!(
        !after,
        "the {path:?} open restarted the live interpreter without the ended frame's global"
    );
}
