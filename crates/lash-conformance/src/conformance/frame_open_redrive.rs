//! FIG-4110: every frame open is atomic, opens exactly once and replays
//! exactly, whoever authors it and wherever its execution dies.
//!
//! Three authors open frames:
//!
//! - **A context-pressure hook.** Core calls it once per turn, before the
//!   Prompt View transforms, with recorded facts. It may run one journaled
//!   summarizer completion and returns a decision. Core opens the frame that
//!   decision names and commits it on its own, before the turn's model call,
//!   through an idempotent fenced write the turn and the hook name. The turn
//!   then runs in the new frame on resident state.
//! - **`continue_as`.** The turn's own `AgentFrameSwitch` outcome, opened by
//!   the turn's commit; the task runs as a follow-on physical turn.
//! - **`/compact`.** An administrative compaction: core records the head
//!   and frame it compacts as one recorded step, a compactor returns seed
//!   nodes over that base, and core opens and commits the frame through an
//!   idempotent fenced write the compaction names. A redrive replays the
//!   recorded base, reads the summary back over it and meets the commit's
//!   receipt, even after the commit moved the head.
//!
//! Each law kills the execution at one point and redrives it on the tier's
//! runner: before `/compact`'s summarizer runs, after the summarizer's
//! completion is journaled but before the frame's commit, after the frame's
//! commit, before and after the turn's commit. However the execution dies, the session ends with one frame per
//! open, chained in order (each frame's predecessor is the frame current at
//! its open), each seed once, one summarizer call per compaction and one
//! model call per physical turn, and every commit made exactly once. A
//! compaction frame's successor never sees the frame it left's prompt usage:
//! the follow-on turn after a `continue_as` whose response reported usage
//! over the threshold does not compact again. On a protocol with live
//! execution state, a pressure frame restarts it from the frame's seed.
//!
//! The pressure laws run over any protocol through a [`FrameLawProtocol`]:
//! the protocol's plugins and how its model answers and switches frames.
//! [`StandardFrameLawProtocol`] is the standard protocol's; a protocol crate
//! registers its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

use crate::admit;
use crate::plugin::PluginFactory;

/// The prompt usage at which the laws' pressure hook compacts.
const PRESSURE_THRESHOLD_TOKENS: i64 = 1_000;
/// Marks a summarizer request so the provider counts it apart from the
/// turns' own calls.
const SUMMARY_REQUEST_MARKER: &str = "frame-open-law-summary";
const SUMMARY_TEXT: &str = "summary of the first root";
const FOLLOW_ON_TASK: &str = "finish in the continued frame";

/// How a protocol's model answers and switches frames, and the plugins that
/// make the protocol, for the frame-open laws.
pub trait FrameLawProtocol: Send + Sync {
    /// The session's plugins: the protocol and anything its frame switch
    /// needs.
    fn plugins(&self) -> Vec<Arc<dyn PluginFactory>>;
    /// A model response that ends the turn with `text`.
    fn answer(&self, text: &str) -> crate::LlmOutputPart;
    /// A model response that switches the turn to a new frame running
    /// `task` (`continue_as`).
    fn continue_as(&self, task: &str) -> crate::LlmOutputPart;
    /// For a protocol with live execution state, the responses that set a
    /// session global and read it back; `None` for a protocol with none.
    fn execution_state(&self) -> Option<ExecutionStateScript> {
        None
    }
}

/// The responses a protocol with live execution state answers with in
/// [`a_pressure_frame_restarts_the_live_execution_state`].
pub struct ExecutionStateScript {
    /// Sets a session global and ends the turn.
    pub set_global: crate::LlmOutputPart,
    /// Ends the turn with the global's type as its answer: `"undefined"`
    /// once a frame ended it.
    pub answer_global_type: crate::LlmOutputPart,
}

/// The tool whose call switches a standard-protocol turn's frame.
const SWITCH_TOOL: &str = "frame_open_law_switch";

/// The standard protocol, whose model switches frames through a tool that
/// returns `ToolControl::SwitchAgentFrame`.
pub struct StandardFrameLawProtocol;

impl StandardFrameLawProtocol {
    pub fn shared() -> Arc<dyn FrameLawProtocol> {
        Arc::new(Self)
    }
}

impl FrameLawProtocol for StandardFrameLawProtocol {
    fn plugins(&self) -> Vec<Arc<dyn PluginFactory>> {
        crate::testing::test_standard_protocol_factories()
            .into_iter()
            .chain([Arc::new(crate::plugin::StaticPluginFactory::new(
                "conformance-frame-open-switch",
                crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool)),
            )) as Arc<dyn PluginFactory>])
            .collect()
    }

    fn answer(&self, text: &str) -> crate::LlmOutputPart {
        crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }
    }

    fn continue_as(&self, task: &str) -> crate::LlmOutputPart {
        crate::LlmOutputPart::ToolCall {
            call_id: "frame-open-law-switch-call".into(),
            tool_name: SWITCH_TOOL.into(),
            input_json: serde_json::json!({ "task": task }).to_string(),
            replay: None,
        }
    }
}

struct SwitchTool;

fn switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{SWITCH_TOOL}"),
        SWITCH_TOOL,
        "Switches the turn to a new frame that runs the given task.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for SwitchTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![switch_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == SWITCH_TOOL).then(|| Arc::new(switch_tool().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"switched": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("frame-open-law-continue-as")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some(FOLLOW_ON_TASK.to_string()),
                },
            ),
        ))
    }
}

/// Where a law kills the execution under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOpenCrash {
    /// `/compact`'s base is recorded; its summarizer has not run.
    BeforeSummary,
    /// The summarizer's completion is journaled; the frame's commit is not.
    AfterSummary,
    /// The frame's own commit is durable: the pressure frame's, before its
    /// turn runs, or `/compact`'s, before it reports.
    AfterFrameCommit,
    /// The turn's model and tool effects are journaled; its commit is not.
    BeforeTurnCommit,
    /// The turn that opened a frame (by pressure or `continue_as`) committed.
    AfterTurnCommit,
    /// The `continue_as` follow-on turn committed; the root has not ended.
    AfterFollowOnCommit,
}

/// Panics at the law's crash point. One probe serves one run of the
/// crashing attempt.
struct CrashProbe {
    crash: FrameOpenCrash,
    context_transforms: AtomicUsize,
    effect_loops: AtomicUsize,
    deliveries: AtomicUsize,
}

impl CrashProbe {
    fn new(crash: FrameOpenCrash) -> Self {
        Self {
            crash,
            context_transforms: AtomicUsize::new(0),
            effect_loops: AtomicUsize::new(0),
            deliveries: AtomicUsize::new(0),
        }
    }
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashProbe {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        use lash_core::runtime::RuntimeTurnPhase;
        match phase {
            RuntimeTurnPhase::ContextTransform => {
                let seen = self.context_transforms.fetch_add(1, Ordering::SeqCst);
                if self.crash == FrameOpenCrash::AfterFrameCommit && seen == 0 {
                    panic!("injected crash after the pressure frame's commit");
                }
            }
            RuntimeTurnPhase::PostCommitDelivery => {
                let seen = self.deliveries.fetch_add(1, Ordering::SeqCst);
                if (self.crash == FrameOpenCrash::AfterTurnCommit && seen == 0)
                    || (self.crash == FrameOpenCrash::AfterFollowOnCommit && seen == 1)
                {
                    panic!("injected crash after a turn's commit");
                }
            }
            _ => {}
        }
    }

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            let seen = self.effect_loops.fetch_add(1, Ordering::SeqCst);
            if self.crash == FrameOpenCrash::BeforeTurnCommit && seen == 0 {
                panic!("injected crash before the turn's commit");
            }
        }
    }

    fn begin_named(&self, _phase: &str) {}
}

/// Compacts once the previous turn's prompt usage reaches the threshold: one
/// direct summarizer completion over the committed frame, then a compaction
/// frame seeded with the summary. Its crashing copy dies right after the
/// completion.
struct ThresholdCompaction {
    crash_after_summary: bool,
}

async fn summarize(
    session_id: &SessionId,
    policy: &crate::SessionPolicy,
    scoped_effect_controller: &crate::ScopedEffectController<'_>,
    direct_completions: &crate::DirectCompletionClient<'_>,
) -> Result<crate::SessionAppendNode, crate::plugin::ContextError> {
    let turn_id = scoped_effect_controller
        .turn_id()
        .map(ToString::to_string)
        .unwrap_or_else(|| scoped_effect_controller.scope_id().to_string());
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
            session_id.clone(),
            SUMMARY_REQUEST_MARKER.to_string(),
            format!("{turn_id}:{SUMMARY_REQUEST_MARKER}"),
        ),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    };
    let completion = direct_completions
        .direct_llm_completion_caused_by(request, "compaction", None)
        .await?;
    Ok(crate::SessionAppendNode::message(
        crate::PluginMessage::text(
            crate::MessageRole::Assistant,
            completion.response.full_text(),
        ),
    ))
}

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
        if !over_threshold {
            return Ok(crate::plugin::ContextPressureDecision::Continue);
        }
        let seed = summarize(
            &ctx.session_id,
            ctx.state.policy(),
            &ctx.scoped_effect_controller,
            &ctx.direct_completions,
        )
        .await?;
        if self.crash_after_summary {
            panic!("injected crash after the summary, before the pressure frame's commit");
        }
        Ok(crate::plugin::ContextPressureDecision::OpenFrame {
            records: Vec::new(),
            task: "conformance pressure compaction".to_string(),
            seed: vec![seed],
        })
    }
}

/// `/compact`'s compactor: one direct summarizer completion. Its crashing
/// copies die right before or right after the completion.
struct SummaryCompactor {
    crash_before_summary: bool,
    crash_after_summary: bool,
}

#[async_trait::async_trait]
impl crate::plugin::ContextCompactor for SummaryCompactor {
    fn id(&self) -> &'static str {
        "conformance.summary_compactor"
    }

    async fn compact(
        &self,
        ctx: &crate::plugin::CompactionContext<'_>,
    ) -> Result<Option<crate::plugin::ContextCompaction>, crate::plugin::ContextError> {
        if self.crash_before_summary {
            panic!("injected crash before the summary, after the compaction's base is recorded");
        }
        let seed = summarize(
            &ctx.session_id,
            ctx.state.policy(),
            &ctx.scoped_effect_controller,
            &ctx.direct_completions,
        )
        .await?;
        if self.crash_after_summary {
            panic!("injected crash after the summary, before the compaction's commit");
        }
        Ok(Some(crate::plugin::ContextCompaction::new(vec![seed])))
    }
}

/// How the law's model answers each call, in order of the turns' calls.
struct ModelScript {
    /// The turns' own responses, in call order, each with its reported
    /// prompt usage.
    turns: Vec<(crate::LlmOutputPart, i64)>,
}

/// The counted scripted model every run of a law shares.
struct LawModel {
    turn_calls: Arc<AtomicUsize>,
    summary_calls: Arc<AtomicUsize>,
    provider: crate::ProviderHandle,
}

fn law_model(script: ModelScript) -> LawModel {
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let turns = Arc::new(script.turns);
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let turn_calls = Arc::clone(&turn_calls);
            let summary_calls = Arc::clone(&summary_calls);
            move |request: crate::LlmRequest| {
                let (part, input_tokens) = if request.scope.agent_frame_id == SUMMARY_REQUEST_MARKER
                {
                    summary_calls.fetch_add(1, Ordering::SeqCst);
                    (
                        crate::LlmOutputPart::Text {
                            text: SUMMARY_TEXT.to_string(),
                            response_meta: None,
                        },
                        1,
                    )
                } else {
                    let index = turn_calls.fetch_add(1, Ordering::SeqCst);
                    turns.get(index).cloned().unwrap_or_else(|| {
                        panic!("the law's model was asked a turn call it did not script: {index}")
                    })
                };
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![part],
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
        .build()
        .into_handle();
    LawModel {
        turn_calls,
        summary_calls,
        provider,
    }
}

/// Everything a runtime for a law is built from, shared by every attempt so
/// each is the same session on the same store.
#[derive(Clone)]
struct LawParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimePersistence>,
    protocol: Arc<dyn FrameLawProtocol>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: &LawParts, crash: Option<FrameOpenCrash>) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    let crash_after_summary = crash == Some(FrameOpenCrash::AfterSummary);
    let compaction: Arc<dyn PluginFactory> = Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-frame-open-compaction",
        crate::facade_support::PluginSpec::new()
            .with_context_pressure_hook(
                100,
                Arc::new(ThresholdCompaction {
                    crash_after_summary,
                }),
            )
            .with_context_compactor(
                100,
                Arc::new(SummaryCompactor {
                    crash_before_summary: crash == Some(FrameOpenCrash::BeforeSummary),
                    crash_after_summary,
                }),
            ),
    ));
    let mut runtime = Box::pin(
        crate::LashRuntime::builder(parts.host.clone(), crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(
                parts
                    .protocol
                    .plugins()
                    .into_iter()
                    .chain([compaction])
                    .collect(),
            )
            .with_store(Arc::clone(&parts.store))
            .build(),
    )
    .await
    .expect("build the frame-open conformance runtime");
    if let Some(crash) = crash.filter(|crash| {
        !matches!(
            crash,
            FrameOpenCrash::BeforeSummary | FrameOpenCrash::AfterSummary
        )
    }) {
        runtime.set_turn_phase_probe(Arc::new(CrashProbe::new(crash)));
    }
    runtime
}

type DriveResultTx = tokio::sync::mpsc::UnboundedSender<
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>,
>;

/// One attempt at a drive. A crashing attempt must die at its crash point;
/// the others send back how their drive ended.
fn drive_attempt(
    parts: &LawParts,
    crash: Option<FrameOpenCrash>,
    result_tx: Option<DriveResultTx>,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(&parts, crash).await;
            let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let Some(result_tx) = result_tx else {
                panic!(
                    "the crash at {crash:?} did not fire: {:?}",
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
fn active_path(graph: &crate::SessionGraph) -> Vec<String> {
    use crate::facade_support::{SessionGraphFacadeOps as _, SessionNodeProjection as _};
    graph
        .active_path_nodes()
        .into_iter()
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

/// The session's frames, first to current: each frame's reason and
/// predecessor.
fn frame_chain(
    head: &crate::store::PersistedSessionRead,
    session_id: &SessionId,
) -> Vec<(String, Option<crate::FrameNodeId>, crate::FrameNodeId)> {
    use crate::facade_support::SessionGraphFacadeOps as _;
    head.graph
        .agent_frame_records(session_id)
        .into_iter()
        .map(|record| {
            (
                record.reason.as_str().to_string(),
                record.previous_frame_node_id,
                record.frame_node_id,
            )
        })
        .collect()
}

/// A law's session, store and first root.
struct LawSession {
    parts: LawParts,
    store: Arc<dyn crate::RuntimePersistence>,
    session_id: SessionId,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    prefix: String,
}

impl LawSession {
    async fn open(
        prefix: &str,
        law: &str,
        effect_host: Arc<dyn crate::EffectHost>,
        stores: Arc<dyn crate::StoreSet>,
        runner: Arc<dyn crate::ConformanceTurnRunner>,
        protocol: Arc<dyn FrameLawProtocol>,
        provider: crate::ProviderHandle,
    ) -> Self {
        let session_id = SessionId::from(format!("{prefix}-{law}"));
        let mut host =
            crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
                .host_config(
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1).with_max_turn_input_admission(1),
                );
        host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(provider));
        let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
        Self {
            parts: LawParts {
                session_id: session_id.clone(),
                host,
                store: Arc::clone(&store),
                protocol,
            },
            store,
            session_id,
            runner,
            prefix: format!("{prefix}-{law}"),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn enqueue(&self, text: &str) {
        self.store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                self.session_id.clone(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text(text),
            ))
            .await
            .expect("accept a queued input");
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn head(&self) -> crate::store::PersistedSessionRead {
        self.store
            .load_session()
            .await
            .expect("read the session head")
            .expect("the session committed")
    }

    /// Runs the root queued next to its end, with no crash.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn run_root(&self, drive: &str) -> crate::TurnOutcome {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner.run_turn(
                admit(crate::ExecutionScope::queue_drain(
                    &self.session_id,
                    format!("{}-{drive}", self.prefix),
                )),
                drive_attempt(&self.parts, None, Some(tx)),
            ),
        )
        .await
        .expect("the drive ends");
        rx.recv()
            .await
            .expect("the tier's runner ran the drive")
            .unwrap_or_else(|error| panic!("the drive runs: {error:?}"))
            .ran()
            .expect("the drive runs its root")
            .outcome
    }

    /// Runs the root queued next, killing it at `crash` and redriving it.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn run_root_crashed_at(&self, drive: &str, crash: FrameOpenCrash) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.runner.run_crashed_then_redriven_turn(
                admit(crate::ExecutionScope::queue_drain(
                    &self.session_id,
                    format!("{}-{drive}", self.prefix),
                )),
                drive_attempt(&self.parts, Some(crash), None),
                drive_attempt(&self.parts, None, Some(tx)),
            ),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("the redrive after {crash:?} ends (it diverged from its journal)")
        });
        rx.recv()
            .await
            .expect("the tier's runner ran the redriven drive")
            .unwrap_or_else(|error| panic!("the redrive after {crash:?} replays: {error:?}"))
            .ran()
            .expect("the redrive runs the root to its end");
    }
}

/// A context-pressure frame, killed at `crash` and redriven, opens once:
/// one summarizer call, one compaction frame after the first, its seed once,
/// and the frame's commit and the turn's each made once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_pressure_frame_opens_once_whatever_its_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Arc<dyn FrameLawProtocol>,
    crash: FrameOpenCrash,
) {
    let model = law_model(ModelScript {
        turns: vec![
            // The first root's usage crosses the hook's threshold.
            (protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let law = LawSession::open(
        prefix,
        &format!("pressure-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;

    law.enqueue("first question").await;
    law.run_root("root-1").await;
    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 0);
    let first_frame = law
        .head()
        .await
        .current_frame_node_id
        .expect("the session stands in its first frame");

    law.enqueue("second question").await;
    let before = law.head().await.head_revision;
    law.run_root_crashed_at("root-2", crash).await;

    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        1,
        "one summarizer call: a redrive reads the summary back from its journal"
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
        "the pressure frame's commit and the turn's each land once"
    );
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 2, "one frame after the first: {chain:?}");
    assert_eq!(chain[1].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(
        chain[1].1.as_ref(),
        Some(&first_frame),
        "the compaction frame follows the frame current at its open"
    );
    assert_eq!(head.current_frame_node_id.as_ref(), Some(&chain[1].2));
    let path = active_path(&head.graph);
    assert_eq!(
        path.iter().filter(|text| *text == "FrameOpen").count(),
        2,
        "{path:?}"
    );
    assert_eq!(
        path.iter().filter(|text| *text == SUMMARY_TEXT).count(),
        1,
        "the seed lands once: {path:?}"
    );
    let seed_at = path
        .iter()
        .position(|text| text == SUMMARY_TEXT)
        .expect("the seed is on the path");
    assert_eq!(path[seed_at - 1], "FrameOpen", "the seed opens the frame");
    assert!(
        path[seed_at..].iter().any(|text| text == "second question"),
        "the compacted root runs in the new frame: {path:?}"
    );
}

/// A root whose first turn opens a context-pressure frame and ends in a
/// `continue_as`, killed at `crash` and redriven, commits both frames in
/// order, each exactly once: the compaction frame follows the first, the
/// `continue_as` frame follows the compaction frame, the follow-on runs
/// there, and the follow-on never compacts again on the switched turn's
/// usage.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_pressure_frame_then_continue_as_commits_both_frames_once(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Arc<dyn FrameLawProtocol>,
    crash: FrameOpenCrash,
) {
    let model = law_model(ModelScript {
        turns: vec![
            (protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            // The switching turn's usage also crosses the threshold: the
            // follow-on must not read it as its own frame's.
            (
                protocol.continue_as(FOLLOW_ON_TASK),
                PRESSURE_THRESHOLD_TOKENS,
            ),
            (protocol.answer("answer in the continued frame"), 1),
        ],
    });
    let law = LawSession::open(
        prefix,
        &format!("pressure-continue-as-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;

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

    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        1,
        "one summarizer call, and no second compaction on the switched turn's usage"
    );
    assert_eq!(
        model.turn_calls.load(Ordering::SeqCst),
        3,
        "one model call per physical turn"
    );
    let head = law.head().await;
    assert_eq!(
        head.head_revision,
        before + 3,
        "the pressure frame, the switching turn and the follow-on each commit once"
    );
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(chain.len(), 3, "two frames after the first: {chain:?}");
    assert_eq!(chain[1].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(chain[1].1.as_ref(), Some(&first_frame));
    assert_ne!(chain[2].0, crate::AgentFrameReason::COMPACTION);
    assert_eq!(
        chain[2].1.as_ref(),
        Some(&chain[1].2),
        "the continue_as frame follows the compaction frame"
    );
    assert_eq!(head.current_frame_node_id.as_ref(), Some(&chain[2].2));
    let path = active_path(&head.graph);
    assert_eq!(
        path.iter().filter(|text| *text == "FrameOpen").count(),
        3,
        "{path:?}"
    );
    assert_eq!(
        path.iter().filter(|text| *text == SUMMARY_TEXT).count(),
        1,
        "{path:?}"
    );
}

/// A context-pressure frame ends the frame's live execution state, exactly as
/// its durable state (F5): a global the first root set is gone from the
/// interpreter the compacted root runs in, and from the state it commits.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_pressure_frame_restarts_the_live_execution_state(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    protocol: Arc<dyn FrameLawProtocol>,
) {
    let script = protocol
        .execution_state()
        .expect("the protocol under test has live execution state");
    let model = law_model(ModelScript {
        turns: vec![
            (script.set_global, PRESSURE_THRESHOLD_TOKENS),
            (script.answer_global_type, 1),
        ],
    });
    let law = LawSession::open(
        prefix,
        "pressure-execution-state",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;

    law.enqueue("set the global").await;
    law.run_root("root-1").await;
    law.enqueue("read the global").await;
    let outcome = law.run_root("root-2").await;

    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 1);
    let chain = frame_chain(&law.head().await, &law.session_id);
    assert_eq!(chain.len(), 2, "one compaction frame: {chain:?}");
    let answer = match outcome {
        crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { text }) => text,
        crate::TurnOutcome::Finished(crate::TurnFinish::FinalValue { value }) => value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_string),
        other => panic!("the compacted root finishes: {other:?}"),
    };
    assert_eq!(
        answer, "undefined",
        "the compacted root's interpreter restarted from the frame's seed, without the \
         ended frame's global"
    );
}

/// `/compact`, killed at `crash` and redriven, opens its frame once: one
/// summarizer call, one compaction frame, one commit. Killed after its
/// commit, the redrive loads the moved head, replays the recorded base and
/// the summary over it, and meets the commit's receipt instead of opening a
/// second frame from the moved head (FIG-4133).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_compaction_frame_opens_once_whatever_its_crash(
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
    let law = LawSession::open(
        prefix,
        &format!("compact-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.enqueue("first question").await;
    law.run_root("root-1").await;
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
                    let opened = Box::pin(runtime.compact_context(None, scope))
                        .await
                        .unwrap_or_else(|error| panic!("the compaction runs: {error}"));
                    match result_tx {
                        Some(result_tx) => {
                            let _ = result_tx.send(opened);
                            crate::ConformanceTurnEnd::Settled
                        }
                        None => panic!("injected crash after the compaction's commit"),
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
        1,
        "one summarizer call: a redrive reads the summary back from its journal"
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
        path.iter().filter(|text| *text == SUMMARY_TEXT).count(),
        1,
        "{path:?}"
    );
}

/// Register the frame-open laws (FIG-4110) over a protocol: a pressure frame
/// and a pressure frame followed by a `continue_as`, each killed at every
/// crash point and redriven. The fixture hands back a guard, a prefix, the
/// tier's effect host, the store set under test, its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) and the
/// [`FrameLawProtocol`](crate::FrameLawProtocol) under test.
#[macro_export]
macro_rules! frame_open_protocol_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::frame_open_protocol_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (a_pressure_frame_crashed_after_its_summary_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterSummary),
            (a_pressure_frame_crashed_after_its_commit_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterFrameCommit),
            (a_pressure_frame_crashed_after_its_turns_commit_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterTurnCommit),
            (a_pressure_frame_then_continue_as_crashed_after_the_summary_commits_both_once,
                a_pressure_frame_then_continue_as_commits_both_frames_once, AfterSummary),
            (a_pressure_frame_then_continue_as_crashed_after_the_pressure_commit_commits_both_once,
                a_pressure_frame_then_continue_as_commits_both_frames_once, AfterFrameCommit),
            (a_pressure_frame_then_continue_as_crashed_before_the_switch_commit_commits_both_once,
                a_pressure_frame_then_continue_as_commits_both_frames_once, BeforeTurnCommit),
            (a_pressure_frame_then_continue_as_crashed_after_the_switch_commit_commits_both_once,
                a_pressure_frame_then_continue_as_commits_both_frames_once, AfterTurnCommit),
            (a_pressure_frame_then_continue_as_crashed_after_the_follow_on_commit_commits_both_once,
                a_pressure_frame_then_continue_as_commits_both_frames_once, AfterFollowOnCommit));
    };
    (@law [$($attrs:tt)*] $fixture:block; ($name:ident, $law:ident, $crash:ident) $(, $rest:tt)*) => {
        $($attrs)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let (_guard, prefix, host, stores, runner, protocol) = $fixture;
            $crate::registration_macro_support::$law(
                prefix,
                host,
                stores,
                runner,
                protocol,
                $crate::registration_macro_support::FrameOpenCrash::$crash,
            )
            .await;
        }
        $crate::frame_open_protocol_redrive_tests!(@law [$($attrs)*] $fixture; $($rest),*);
    };
    (@law [$($attrs:tt)*] $fixture:block;) => {};
}

/// Register the execution-state frame-open law (FIG-4110, F5) over a
/// protocol with live execution state. The fixture hands back what
/// [`frame_open_protocol_redrive_tests`]'s does.
#[macro_export]
macro_rules! frame_open_execution_state_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_pressure_frame_restarts_the_live_execution_state() {
            let (_guard, prefix, host, stores, runner, protocol) = $fixture;
            $crate::registration_macro_support::a_pressure_frame_restarts_the_live_execution_state(
                prefix, host, stores, runner, protocol,
            )
            .await;
        }
    };
}

/// Register the frame-open laws (FIG-4110) on the standard protocol: the
/// protocol laws of [`frame_open_protocol_redrive_tests`], and `/compact`
/// killed before its summary, after its summary and after its commit
/// (FIG-4133). The fixture hands back a guard, a prefix, the tier's effect
/// host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! frame_open_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::frame_open_protocol_redrive_tests!($(#[$attr])* {
            let (guard, prefix, host, stores, runner) = $fixture;
            (
                guard,
                prefix,
                host,
                stores,
                runner,
                $crate::registration_macro_support::StandardFrameLawProtocol::shared(),
            )
        });
        $crate::frame_open_redrive_tests!(@compact [$(#[$attr])*] $fixture;
            (a_compaction_crashed_before_its_summary_opens_once, BeforeSummary),
            (a_compaction_crashed_after_its_summary_opens_once, AfterSummary),
            (a_compaction_crashed_after_its_commit_opens_once, AfterFrameCommit));
    };
    (@compact [$($attrs:tt)*] $fixture:block; ($name:ident, $crash:ident) $(, $rest:tt)*) => {
        $($attrs)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::a_compaction_frame_opens_once_whatever_its_crash(
                prefix,
                host,
                stores,
                runner,
                $crate::registration_macro_support::FrameOpenCrash::$crash,
            )
            .await;
        }
        $crate::frame_open_redrive_tests!(@compact [$($attrs)*] $fixture; $($rest),*);
    };
    (@compact [$($attrs:tt)*] $fixture:block;) => {};
}
