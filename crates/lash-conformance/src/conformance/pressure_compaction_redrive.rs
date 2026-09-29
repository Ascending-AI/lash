//! FIG-4110: a context-pressure compaction opens exactly one frame, and a
//! drive crashed after the compacting turn's commit replays it without a
//! second summary or a second frame.
//!
//! Core calls each plugin's context-pressure hook once per turn, before the
//! Prompt View transforms. The hook reads recorded facts (the previous
//! provider-reported prompt usage, the committed read view), may run one
//! journaled summarizer completion, and returns a decision; core opens the
//! frame the decision names on the resident state before the turn runs, and
//! the frame commits with the turn (ADR 0001, ADR 0105 §6, ADR 0112 §9).
//!
//! The law's first root answers with a prompt usage over the hook's
//! threshold. The second root's hook then summarizes and opens a compaction
//! frame; the drive dies after that root's commit and the tier redrives it.
//! The redrive runs the prepare step again over the recorded base: the hook
//! decides again, its summarizer completion is read back from the journal,
//! and core re-derives the same frame, which the replayed commit already
//! holds. The session must end in one compaction frame whose predecessor is
//! the first frame, seeded once, with one summarizer call and one model call
//! per root in total and each root committed once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

use crate::admit;
use crate::plugin::PluginFactory;

/// The prompt usage at which the law's hook compacts.
const PRESSURE_THRESHOLD_TOKENS: i64 = 1_000;
/// Marks the summarizer's request so the provider can count it apart from
/// the turns' own calls.
const SUMMARY_REQUEST_MARKER: &str = "pressure-compaction-law-summary";
const SUMMARY_TEXT: &str = "summary of the first root";

/// Panics as the pressure-compacted root's delivery begins: its commit is
/// durable and the drive has not ended.
struct PanicAfterCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAfterCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            panic!("injected crash after the pressure-compacted turn's commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, _phase: &str) {}
}

/// Compacts once the previous turn's prompt usage reaches the threshold:
/// one direct summarizer completion over the committed frame, then a
/// compaction frame seeded with the summary.
struct ThresholdCompaction;

#[async_trait::async_trait]
impl crate::plugin::ContextPressureHook for ThresholdCompaction {
    fn id(&self) -> &'static str {
        "conformance.pressure_compaction"
    }

    async fn decide(
        &self,
        ctx: &crate::plugin::ContextPressureContext<'_>,
    ) -> Result<crate::plugin::ContextPressureDecision, crate::plugin::ContextError> {
        let over_threshold = ctx
            .prompt_usage
            .as_ref()
            .is_some_and(|usage| usage.total() >= PRESSURE_THRESHOLD_TOKENS);
        if !over_threshold || ctx.state.messages().is_empty() {
            return Ok(crate::plugin::ContextPressureDecision::Continue);
        }
        let turn_id = ctx
            .scoped_effect_controller
            .turn_id()
            .map(ToString::to_string)
            .unwrap_or_default();
        let policy = ctx.state.policy();
        let request = crate::LlmRequest {
            instructions: None,
            model: policy.model.id.clone(),
            messages: vec![lash_sansio::llm::types::LlmMessage::text(
                lash_sansio::llm::types::LlmRole::User,
                "Summarize the conversation so far.",
            )],
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: lash_sansio::llm::types::LlmToolChoice::None,
            model_variant: policy.model.variant.clone(),
            model_capability: policy.model.capability.clone(),
            generation: policy.generation.clone(),
            scope: crate::LlmRequestScope::new(
                ctx.session_id.clone(),
                SUMMARY_REQUEST_MARKER.to_string(),
                format!("{turn_id}:{SUMMARY_REQUEST_MARKER}"),
            ),
            output_spec: None,
            stream_events: None,
            provider_trace: None,
        };
        let completion = ctx
            .direct_completions
            .direct_llm_completion_caused_by(request, "compaction", None)
            .await?;
        Ok(crate::plugin::ContextPressureDecision::OpenFrame {
            records: Vec::new(),
            task: "conformance pressure compaction".to_string(),
            seed: vec![crate::SessionAppendNode::message(
                crate::PluginMessage::text(
                    crate::MessageRole::Assistant,
                    completion.response.full_text(),
                ),
            )],
        })
    }
}

/// Everything a runtime for this law is built from, shared by every attempt
/// so each is the same session on the same store.
#[derive(Clone)]
struct RedriveParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
    compaction: Arc<dyn PluginFactory>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: RedriveParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain([parts.compaction])
                    .collect(),
            )
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .build(),
    )
    .await
    .expect("build the pressure-compaction redrive conformance runtime")
}

type DriveResultTx = tokio::sync::mpsc::UnboundedSender<
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>,
>;

/// One attempt at a drive: a crashing one panics after its root's commit;
/// the others send back how their drive ended.
fn attempt(
    parts: &RedriveParts,
    crash: bool,
    result_tx: Option<DriveResultTx>,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if crash {
                runtime.set_turn_phase_probe(Arc::new(PanicAfterCommit));
            }
            let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let Some(result_tx) = result_tx else {
                panic!(
                    "the crash probe did not fire after the pressure-compacted commit: {:?}",
                    drive.map(crate::facade_support::QueuedTurnDrain::ran)
                );
            };
            let end = crate::ConformanceTurnEnd::of(&drive);
            let _ = result_tx.send(drive);
            end
        })
    })
}

/// The committed active path, first frame to leaf: each message's text, and
/// `FrameOpen` for each frame boundary.
/// The session's active path from its root, frame opens and message text,
/// paged through history: the resident window holds only the current frame
/// (ADR 0112).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the session under test has a head"
)]
async fn active_path(store: &Arc<dyn crate::RuntimeStore>, session_id: &SessionId) -> Vec<String> {
    use crate::facade_support::SessionNodeProjection as _;
    let budget = crate::store::HistoryBudget {
        max_nodes: std::num::NonZeroU32::new(64).expect("nonzero node budget"),
        max_bytes: std::num::NonZeroU64::new(1 << 20).expect("nonzero byte budget"),
    };
    let mut anchor = crate::store::HistoryAnchor::Head;
    let mut newest_first = Vec::new();
    loop {
        let page = store
            .load_ancestors(session_id, anchor, budget)
            .await
            .expect("page the session history");
        newest_first.extend(page.nodes.into_iter().map(|node| node.record));
        match page.next {
            Some(cursor) => anchor = crate::store::HistoryAnchor::Cursor(cursor),
            None => break,
        }
    }
    newest_first
        .into_iter()
        .rev()
        .filter_map(|node| {
            if matches!(node.payload, crate::SessionNodePayload::FrameOpen { .. }) {
                return Some("FrameOpen".to_string());
            }
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

/// A pressure compaction opens exactly one frame with one summarizer call,
/// and a drive crashed after its commit replays it with no second summary
/// and no second frame.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_pressure_compaction_redriven_after_its_commit_opens_one_frame_and_summarizes_once(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-pressure-compaction-session"));
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let turn_calls = Arc::clone(&turn_calls);
            let summary_calls = Arc::clone(&summary_calls);
            move |request: crate::LlmRequest| {
                let (text, input_tokens) = if request.scope.agent_frame_id == SUMMARY_REQUEST_MARKER
                {
                    summary_calls.fetch_add(1, Ordering::SeqCst);
                    (SUMMARY_TEXT.to_string(), 1)
                } else {
                    let index = turn_calls.fetch_add(1, Ordering::SeqCst);
                    // The first root's usage crosses the hook's threshold.
                    (
                        format!("answer {}", index + 1),
                        if index == 0 {
                            PRESSURE_THRESHOLD_TOKENS
                        } else {
                            1
                        },
                    )
                };
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        usage: lash_sansio::llm::types::LlmUsage {
                            input_tokens,
                            output_tokens: 1,
                            ..Default::default()
                        },
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1).with_max_turn_input_admission(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    let compaction: Arc<dyn PluginFactory> = Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-pressure-compaction",
        crate::facade_support::PluginSpec::new()
            .with_context_pressure_hook(100, Arc::new(ThresholdCompaction)),
    ));
    let parts = RedriveParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store),
        compaction,
    };
    let enqueue = |text: &'static str| {
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        async move {
            store
                .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                    session_id,
                    crate::TurnInputIngress::NextTurn,
                    crate::TurnInput::text(text),
                ))
                .await
                .expect("accept a queued input");
        }
    };

    // The first root answers over the threshold; nothing compacts yet.
    enqueue("first question").await;
    let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_turn(
            admit(crate::ExecutionScope::queue_drain(
                &session_id,
                format!("{prefix}-pressure-compaction-1"),
            )),
            attempt(&parts, false, Some(first_tx)),
        ),
    )
    .await
    .expect("the first drive ends");
    first_rx
        .recv()
        .await
        .expect("the tier's runner ran the first drive")
        .unwrap_or_else(|error| panic!("the first drive runs: {error:?}"))
        .ran()
        .expect("the first drive runs the first input");
    assert_eq!(summary_calls.load(Ordering::SeqCst), 0);
    let first = crate::conformance::helpers::load_current_window(store.as_ref(), &session_id)
        .await
        .expect("read the first root's head")
        .expect("the first root committed");
    let first_frame = first
        .current_frame_node_id
        .clone()
        .expect("the session stands in its first frame");

    // The second root compacts; its drive dies after the commit and is
    // redriven.
    enqueue("second question").await;
    let before = store
        .load_session_head_meta(&session_id)
        .await
        .expect("read the session head")
        .map_or(0, |head| head.head_revision);
    let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::queue_drain(
                &session_id,
                format!("{prefix}-pressure-compaction-2"),
            )),
            attempt(&parts, true, None),
            attempt(&parts, false, Some(second_tx)),
        ),
    )
    .await
    .expect("the redrive after the compacted commit ends (it diverged from its journal)");
    let second = second_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven drive")
        .unwrap_or_else(|error| panic!("the redrive after the compacted commit replays: {error:?}"))
        .ran()
        .expect("the redrive runs the second root to its end");
    assert_eq!(second.assistant_output.safe_text, "answer 2");

    assert_eq!(
        summary_calls.load(Ordering::SeqCst),
        1,
        "one summarizer call: the redrive reads the summary back from the journal"
    );
    assert_eq!(
        turn_calls.load(Ordering::SeqCst),
        2,
        "one model call per root: the compacted turn's call is read back"
    );
    let head = crate::conformance::helpers::load_current_window(store.as_ref(), &session_id)
        .await
        .expect("read the compacted head")
        .expect("the second root committed");
    assert_eq!(
        head.head_revision,
        before + 1,
        "the compacted root commits once"
    );
    let frame = head
        .current_frame_node_id
        .clone()
        .expect("the session stands in a frame");
    assert_ne!(frame, first_frame, "the compacted root opened a frame");
    {
        use crate::facade_support::SessionGraphFacadeOps as _;
        let records = head.window.agent_frame_records(&session_id);
        let current = records
            .iter()
            .find(|record| record.frame_node_id == frame)
            .expect("the current frame has a record");
        assert_eq!(current.reason.as_str(), crate::AgentFrameReason::COMPACTION);
        assert_eq!(
            current.previous_frame_node_id.as_ref(),
            Some(&first_frame),
            "exactly one frame opened: the current frame follows the first"
        );
    }
    assert_eq!(
        active_path(&store, &session_id).await,
        [
            "FrameOpen",
            "first question",
            "answer 1",
            "FrameOpen",
            SUMMARY_TEXT,
            "second question",
            "answer 2"
        ],
        "one compaction frame, seeded once, then the compacted root"
    );
}

/// Register the pressure-compaction redrive law (FIG-4110): a context-pressure
/// compaction opens one frame with one summarizer call, and a drive crashed
/// after its commit replays it. The fixture hands back a guard, a prefix, the
/// tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! pressure_compaction_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::pressure_compaction_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (a_pressure_compaction_redriven_after_its_commit_opens_one_frame_and_summarizes_once, "pressure-compaction-redrive"));
    };
    (@law [$(#[$attr:meta])*] $fixture:block; ($law:ident, $label:literal)) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}
