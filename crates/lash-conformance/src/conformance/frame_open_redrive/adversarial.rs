//! FIG-4134: the frame-open laws an adversarial review found missing.
//!
//! - The production standard compactor and its overflow recovery, killed at
//!   every crash point, replay exactly as the laws' synthetic compactor does:
//!   the redrive's admitted window hashes to the request identity the first
//!   execution journaled its summary under, so it reads the summary back.
//! - `/compact` presents the drive fence current when it starts: an admission
//!   sealed before its frame commit, or a pressure frame a turn commits meanwhile,
//!   refuses it typed with nothing of it durable, and the admitted turn
//!   proceeds.
//! - A session deleted while `/compact` opens its frame keeps nothing of the open,
//!   and a fork made meanwhile sees the point it forked from, never a partial
//!   seed.
//! - An explicit empty pressure seed opens one frame; a pressure frame whose
//!   commit the store refuses leaves nothing of the open visible.
//! - Two plugins whose pressure hooks share an id keep their records apart.
//! - Every open restarts the live interpreter: a staged open, `/compact` with
//!   a store and `/compact` without one.

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

/// Prompt usage over the standard compactor's pressure threshold on the
/// laws' model (a 200k window less its 20k buffer).
pub(super) const STANDARD_PRESSURE_TOKENS: i64 = 190_000;
const OVERFLOW_RECOVERY_PENDING: &str =
    "Standard-compaction context-overflow recovery marker (pending):";
const OVERFLOW_RECOVERY_COMPLETED: &str =
    "Standard-compaction context-overflow recovery completed:";

type CompactionResult = Result<bool, crate::facade_support::PluginOperationInvokeError>;

impl LawSession {
    /// Runs `/compact` once on the tier's runner, holding it after its
    /// journaled summary while `during` runs, and answers what the
    /// compaction returned.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn compact_holding<F: std::future::Future<Output = ()>>(
        &self,
        drive: &str,
        during: F,
    ) -> CompactionResult {
        let hold = self
            .parts
            .compaction
            .hold
            .clone()
            .expect("the law holds its compaction's summarizer");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let parts = self.parts.clone();
        let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
            let parts = parts.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = build_runtime(&parts, None).await;
                let _ = tx.send(Box::pin(runtime.compact_context(None, scope)).await);
                crate::ConformanceTurnEnd::Settled
            })
        });
        let compaction = self.runner.run_turn(
            admit(crate::ExecutionScope::runtime_operation(format!(
                "{}-{drive}",
                self.prefix
            ))),
            attempt,
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            futures_util::future::join(compaction, hold.while_held(during)),
        )
        .await
        .expect("the held compaction ends");
        rx.recv()
            .await
            .expect("the tier's runner ran the compaction")
    }

    /// Runs the root queued next once and answers how its drive ended,
    /// whatever that was: the attempt reports itself settled either way.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(super) async fn run_root_to_any_end(
        &self,
        drive: &str,
    ) -> Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let parts = self.parts.clone();
        let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
            let parts = parts.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = build_runtime(&parts, None).await;
                let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )))
                .await;
                let _ = tx.send(drive);
                crate::ConformanceTurnEnd::Settled
            })
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner.run_turn(
                admit(crate::ExecutionScope::queue_drain(
                    &self.session_id,
                    format!("{}-{drive}", self.prefix),
                )),
                attempt,
            ),
        )
        .await
        .expect("the drive ends");
        rx.recv().await.expect("the tier's runner ran the drive")
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

/// The production standard compactor, killed at `crash` in the root its
/// pressure threshold compacts and redriven, opens one frame: the summary
/// is journaled under the request identity the admitted window hashes to,
/// so the redrive reads it back (one summarizer call, or two when the crash
/// lost a paid answer before its journal record), and the seed lands once.
///
/// FIG-4072 (ADR 0112 §14.4): every execution of the root, the redrive of a
/// crash before the terminal commit included (it reloads the window the
/// root was admitted on, after the frame's commit moved the head), derives
/// the same compaction session id and turn id, and they are the ids the
/// summarizer's provider requests carry.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_standard_compaction_frame_opens_once_whatever_its_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    let protocol = StandardFrameLawProtocol::shared();
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
    law.run_root("root-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;
    let before = law.head().await.head_revision;
    law.run_root_crashed_at("root-2", crash).await;

    assert_standard_frame_opened_once(&law, &model, crash, before, &first_frame).await;
    let derived = request_ids
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    // Every crash but the one after the terminal commit leaves the root to a
    // redrive that prepares it again, from its admitted window.
    let executions = if crash == FrameOpenCrash::AfterTurnCommit {
        1
    } else {
        2
    };
    assert!(
        derived.len() >= executions,
        "each execution of the root derives its compaction's ids: {derived:?}"
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
        "one model call per root"
    );
    let head = law.head().await;
    assert_eq!(
        head.head_revision,
        before + 2,
        "the frame's commit and the turn's each land once"
    );
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
        "the compacted root runs in the new frame: {path:?}"
    );
}

/// The standard compactor's overflow recovery, killed at `crash` in the
/// root that recovers and redriven: the provider refused the first root as
/// too long, and the next root records `Completed` in the frame it leaves
/// and opens one recovery frame, each exactly once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_overflow_recovery_frame_opens_once_whatever_its_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    let protocol = StandardFrameLawProtocol::shared();
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
    law.run_root("root-1").await;
    let overflowed = law.head().await;
    let first_frame = overflowed
        .current_frame_node_id
        .clone()
        .expect("the session stands in its first frame");
    assert_eq!(
        count(&every_message(&overflowed), |text| text
            .starts_with(OVERFLOW_RECOVERY_PENDING)),
        1,
        "the refused root leaves its recovery marker"
    );
    law.enqueue("second question").await;
    let before = overflowed.head_revision;
    law.run_root_crashed_at("root-2", crash).await;

    assert_standard_frame_opened_once(&law, &model, crash, before, &first_frame).await;
    let head = law.head().await;
    let messages = every_message(&head);
    assert_eq!(
        count(&messages, |text| text
            .starts_with(OVERFLOW_RECOVERY_PENDING)),
        1,
        "{messages:?}"
    );
    assert_eq!(
        count(&messages, |text| text
            .starts_with(OVERFLOW_RECOVERY_COMPLETED)),
        1,
        "the recovery records Completed once: {messages:?}"
    );
}

/// `/compact` writes beside the drive under the fence current when it
/// starts. Held after its summary while another worker's admission seals a
/// newer drive epoch (without moving the head), it is refused typed with
/// nothing of it durable, and the next root proceeds in the frame it was in.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_compaction_superseded_by_a_newer_admission_is_refused(
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
        "compact-superseded",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.hold = Some(SummaryHold::default());
    law.enqueue("first question").await;
    law.run_root("root-1").await;
    let before = law.head().await;

    let store = Arc::clone(&law.store);
    let session_id = law.session_id.clone();
    let refused = law
        .compact_holding("compact", async move {
            let stored = store
                .drive_epoch(&session_id)
                .await
                .expect("read the drive epoch");
            let sealed = store
                .seal_drive_epoch(
                    &session_id,
                    &crate::store::AdmissionId::new(format!("{session_id}-newer-admission")),
                    stored.epoch,
                    &crate::store::RootStartNonce::new(format!("{session_id}-newer-start")),
                )
                .await
                .expect("seal a newer drive epoch");
            assert!(
                matches!(sealed, crate::store::DriveEpochSeal::Sealed(_)),
                "{sealed:?}"
            );
        })
        .await;
    assert!(
        matches!(
            refused,
            Err(crate::facade_support::PluginOperationInvokeError::Store(
                crate::StoreError::StaleDriveFence { .. }
            ))
        ),
        "a compaction whose fence a newer admission superseded is refused typed: {refused:?}"
    );
    let after = law.head().await;
    assert_eq!(
        after.head_revision, before.head_revision,
        "nothing of the refused compaction is durable"
    );
    assert_eq!(frame_chain(&after, &law.session_id).len(), 1);

    law.enqueue("second question").await;
    law.run_root("root-2").await;
    let head = law.head().await;
    assert_eq!(frame_chain(&head, &law.session_id).len(), 1);
    let path = active_path(&head.graph);
    assert!(path.iter().any(|text| text == "answer 2"), "{path:?}");
    assert_eq!(count(&path, |text| text == SUMMARY_TEXT), 0, "{path:?}");
}

/// `/compact` held after its summary while a root's pressure hook opens and
/// commits its own frame: the root's admission superseded the compaction's
/// fence, so the compaction is refused typed, and the session holds exactly
/// the pressure frame.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_compaction_overlapping_a_pressure_frame_is_refused(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
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
        "compact-overlaps-pressure",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.compaction.hold = Some(SummaryHold::default());
    law.enqueue("first question").await;
    law.run_root("root-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;

    let refused = law
        .compact_holding("compact", async {
            law.run_root("root-2").await;
        })
        .await;
    assert!(
        matches!(
            refused,
            Err(crate::facade_support::PluginOperationInvokeError::Store(
                crate::StoreError::StaleDriveFence { .. }
            ))
        ),
        "{refused:?}"
    );
    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        2,
        "the pressure hook's summary and the refused compaction's"
    );
    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "only the pressure frame opened: {chain:?}");
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    let path = active_path(&head.graph);
    assert_eq!(count(&path, |text| text == SUMMARY_TEXT), 1, "{path:?}");
    assert!(path.iter().any(|text| text == "answer 2"), "{path:?}");
}

/// A session deleted while `/compact` opens its frame keeps nothing of the open:
/// the compaction fails, and the session stays deleted.
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
    law.run_root("root-1").await;

    let store = Arc::clone(&law.store);
    let session_id = law.session_id.clone();
    let compacted = law
        .compact_holding("compact", async move {
            store
                .delete_session(&session_id)
                .await
                .expect("delete the session while its frame opens");
        })
        .await;
    assert!(
        compacted.is_err(),
        "a compaction of a deleted session commits nothing: {compacted:?}"
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

/// A fork made at the head while `/compact` opens its frame sees the point it
/// forked from: the compaction commits its frame in the source session, and
/// the fork holds no part of the seed.
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
    law.run_root("root-1").await;
    let forked_from = law.head().await;
    let leaf = forked_from
        .graph
        .leaf_node_id
        .clone()
        .expect("the session has a leaf");
    let fork_id = SessionId::from(format!("{}-fork", law.session_id));

    let store = Arc::clone(&law.store);
    let fork = fork_id.clone();
    let compacted = law
        .compact_holding("compact", async move {
            store
                .fork_session(&crate::ForkSessionRequest {
                    session_id: fork,
                    node_id: leaf,
                    relation: crate::SessionRelation::Root,
                    pending_observer_intents: Vec::new(),
                    policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
                })
                .await
                .expect("fork the session while its frame opens");
        })
        .await;
    assert!(
        matches!(compacted, Ok(true)),
        "the compaction commits its frame: {compacted:?}"
    );
    let source = law.head().await;
    assert_eq!(frame_chain(&source, &law.session_id).len(), 2);
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
/// redriven: no summary, one frame after the first, and the root runs in it.
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
    law.run_root("root-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;
    let before = law.head().await.head_revision;
    law.run_root_crashed_at("root-2", crash).await;

    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 0);
    let head = law.head().await;
    assert_eq!(head.head_revision, before + 2);
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
        "the empty seed adds nothing, and the root runs in the new frame"
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
    law.run_root("root-1").await;
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");
    law.enqueue("second question").await;
    let drive = law
        .run_root_to_any_end("root-2")
        .await
        .map(crate::facade_support::QueuedTurnDrain::ran);

    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(
        chain.len(),
        1,
        "no frame opened: {chain:?} (the root: {drive:?})"
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
    law.run_root_crashed_at("root-1", crash).await;

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
    /// A staged open ([`crate::LashRuntime::open_agent_frame`]).
    Staged,
    /// `/compact` on a store-backed runtime.
    Compact,
    /// `/compact` on a runtime with no store.
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
        turns: vec![(script.set_global, 1)],
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
    law.run_root("root-1").await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let parts = law.parts.clone();
    let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
        let parts = parts.clone();
        let tx = tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(&parts, None).await;
            let before = live_holds(&mut runtime, global).await;
            let mut runtime = match path {
                LiveResetPath::Staged => {
                    let opened = runtime
                        .open_agent_frame(crate::OpenAgentFrameRequest::new(
                            crate::FrameKey::from_caller_material("frame-open-law-staged")
                                .expect("non-empty frame material"),
                            crate::AgentFrameReason::new("staged"),
                        ))
                        .await
                        .expect("stage a frame");
                    assert!(opened.opened);
                    runtime
                }
                LiveResetPath::Compact => {
                    assert!(
                        Box::pin(runtime.compact_context(None, scope))
                            .await
                            .expect("the compaction runs")
                    );
                    runtime
                }
                LiveResetPath::StorelessCompact => {
                    let snapshot = runtime
                        .snapshot_execution_state()
                        .await
                        .expect("read the live execution state")
                        .expect("the first root left execution state");
                    let mut storeless_parts = parts.clone();
                    storeless_parts.compaction.storeless = true;
                    let mut storeless = build_runtime(&storeless_parts, None).await;
                    storeless
                        .restore_execution_state(&snapshot)
                        .await
                        .expect("restore the live execution state without a store");
                    assert!(live_holds(&mut storeless, global).await);
                    assert!(
                        Box::pin(storeless.compact_context(None, scope))
                            .await
                            .expect("the storeless compaction runs")
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
            admit(crate::ExecutionScope::runtime_operation(format!(
                "{}-open",
                law.prefix
            ))),
            attempt,
        ),
    )
    .await
    .expect("the open ends");
    let (before, after) = rx.recv().await.expect("the tier's runner ran the open");
    assert!(
        before,
        "the reopened runtime's interpreter holds the first root's global"
    );
    assert!(
        !after,
        "the {path:?} open restarted the live interpreter without the ended frame's global"
    );
}
