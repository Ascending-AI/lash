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
//! - **`compact_context`.** An administrative compaction: core records the head
//!   and frame it compacts as one recorded step, a compactor returns seed
//!   nodes over that base, and core opens and commits the frame through an
//!   idempotent fenced write the compaction names. A redrive replays the
//!   recorded base, reads the summary back over it and meets the commit's
//!   receipt, even after the commit moved the head.
//!
//! Each law kills the execution at one point and redrives it on the tier's
//! runner: before `compact_context`'s summarizer runs, after the summarizer's
//! provider answered but before its answer is journaled, after the
//! summarizer's completion is journaled but before the frame's commit, after
//! the frame's commit, before and after the turn's commit. However the
//! execution dies, the session ends with one frame per open, chained in order
//! (each frame's predecessor is the frame current at its open), each seed
//! once, one model call per physical turn, and every commit made exactly
//! once.
//!
//! The summarizer's provider call is at-least-once, like every model call
//! lash makes (F3, FIG-4134): once a summary's result is journaled it is never
//! requested again, so a crash anywhere else costs one summarizer call per
//! compaction, but a crash between the provider's answer and its journal
//! record requests it again on redrive. The summarizer is then called twice,
//! never more, and the session still ends with exactly one frame. A
//! compaction frame's successor never sees the frame it left's prompt usage:
//! the follow-on turn after a `continue_as` whose response reported usage
//! over the threshold does not compact again. On a protocol with live
//! execution state, a pressure frame restarts it from the frame's seed.
//!
//! Beyond the crash matrix (FIG-4134): the production standard compactor and
//! its overflow recovery replay exactly as the laws' synthetic compactor does
//! (a redrive's admitted window hashes to the request identity the first
//! execution journaled); an administrative compaction writes under the drive
//! fence current when it starts, so an admission sealed while it summarizes
//! refuses it typed; a
//! session deleted while a frame opens takes nothing of the open, and a fork
//! made meanwhile never sees a partial seed; an empty seed opens a frame; a
//! frame whose commit the store refuses leaves nothing visible; two plugins
//! whose pressure hooks share an id keep their records apart; and every open
//! restarts the live interpreter, staged or committed, with a store or
//! without.
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

mod adversarial;
pub use adversarial::*;
mod followup;
pub use followup::*;
mod superseded_root;
pub use superseded_root::*;

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
/// [`a_pressure_frame_restarts_the_live_execution_state`] and
/// [`every_open_restarts_the_live_execution_state`].
pub struct ExecutionStateScript {
    /// The name of the session global [`Self::set_global`] sets, as the
    /// protocol's live execution state spells it.
    pub global: &'static str,
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
    /// An administrative compaction's base is recorded; its summarizer has
    /// not run.
    BeforeSummary,
    /// The summarizer's provider answered; its answer is not journaled.
    AfterProviderAnswer,
    /// The summarizer's completion is journaled; the frame's commit is not.
    AfterSummary,
    /// The frame's own commit is durable: the pressure frame's, before its
    /// turn runs, or an administrative compaction's, before it reports.
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
    context_pressure_ends: AtomicUsize,
    context_transforms: AtomicUsize,
    effect_loops: AtomicUsize,
    deliveries: AtomicUsize,
}

impl CrashProbe {
    fn new(crash: FrameOpenCrash) -> Self {
        Self {
            crash,
            context_pressure_ends: AtomicUsize::new(0),
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

    fn end_named(&self, phase: &str) {
        // A hook the law does not own (the production standard compactor)
        // is killed right after its decision returns: its summary is
        // journaled, and the frame's commit has not run.
        if self.crash == FrameOpenCrash::AfterSummary
            && phase.starts_with("plugin_hook.context_pressure.")
            && self.context_pressure_ends.fetch_add(1, Ordering::SeqCst) == 0
        {
            panic!("injected crash after the summary, before the pressure frame's commit");
        }
    }
}

/// Compacts once the previous turn's prompt usage reaches the threshold: one
/// direct summarizer completion over the committed frame, then a compaction
/// frame seeded with the summary. Its crashing copy dies right after the
/// completion.
struct ThresholdCompaction {
    crash_after_summary: bool,
    seed: LawSeed,
    hold: Option<SummaryHold>,
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
        extra_body: policy.model.extra_body.clone(),
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
        if self.seed == LawSeed::Empty {
            return Ok(crate::plugin::ContextPressureDecision::OpenFrame {
                records: Vec::new(),
                task: "conformance pressure compaction".to_string(),
                seed: Vec::new(),
            });
        }
        let seed = summarize(
            &ctx.session_id,
            ctx.state.policy(),
            &ctx.scoped_effect_controller,
            &ctx.direct_completions,
        )
        .await?;
        if let Some(hold) = &self.hold {
            hold.wait().await;
        }
        if self.crash_after_summary {
            panic!("injected crash after the summary, before the pressure frame's commit");
        }
        let (records, seed) = match self.seed {
            LawSeed::Oversized => (
                vec![crate::SessionAppendNode::message(
                    crate::PluginMessage::text(
                        crate::MessageRole::Assistant,
                        OVERSIZED_RECORD_TEXT,
                    ),
                )],
                std::iter::once(seed)
                    .chain((0..OVERSIZED_SEED_NODES).map(|ordinal| {
                        crate::SessionAppendNode::message(crate::PluginMessage::text(
                            crate::MessageRole::Assistant,
                            format!("oversized seed {ordinal}"),
                        ))
                    }))
                    .collect(),
            ),
            LawSeed::Summary | LawSeed::Empty => (Vec::new(), vec![seed]),
        };
        Ok(crate::plugin::ContextPressureDecision::OpenFrame {
            records,
            task: "conformance pressure compaction".to_string(),
            seed,
        })
    }
}

/// The administrative compaction's compactor: one direct summarizer
/// completion. Its crashing copies die right before or right after the
/// completion.
struct SummaryCompactor {
    crash_before_summary: bool,
    crash_after_summary: bool,
    hold: Option<SummaryHold>,
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
        if let Some(hold) = &self.hold {
            hold.wait().await;
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
    /// Set, the next summarizer call answers from the provider's side and
    /// never returns to the runtime: the answer is paid for, and lost before
    /// it is journaled.
    hang_next_summary: Arc<std::sync::atomic::AtomicBool>,
    /// Fires when a hung summarizer call has answered.
    summary_answered: Arc<tokio::sync::Notify>,
    /// Every summarizer request's compaction session id and turn id (its
    /// scope's frame and request ids), in call order.
    summary_requests: CompactionIds,
}

/// A turn the law's model answers with a provider's context-overflow
/// refusal.
fn context_overflow() -> (crate::LlmOutputPart, i64) {
    (
        crate::LlmOutputPart::Text {
            text: CONTEXT_OVERFLOW_MARKER.to_string(),
            response_meta: None,
        },
        1,
    )
}

/// Marks a scripted turn the model answers with a context-overflow refusal.
const CONTEXT_OVERFLOW_MARKER: &str = "frame-open-law-context-overflow";

/// Whether `request` is a summarizer's: the laws' compactor marks its own,
/// and the standard compactor names a compaction session in its scope.
fn is_summary_request(request: &crate::LlmRequest) -> bool {
    request.scope.agent_frame_id == SUMMARY_REQUEST_MARKER
        || request.scope.agent_frame_id.contains("-compaction:")
}

fn law_model(script: ModelScript) -> LawModel {
    let turn_calls = Arc::new(AtomicUsize::new(0));
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let hang_next_summary = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let summary_answered = Arc::new(tokio::sync::Notify::new());
    let summary_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let turns = Arc::new(script.turns);
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let turn_calls = Arc::clone(&turn_calls);
            let summary_calls = Arc::clone(&summary_calls);
            let hang_next_summary = Arc::clone(&hang_next_summary);
            let summary_answered = Arc::clone(&summary_answered);
            let summary_requests = Arc::clone(&summary_requests);
            move |request: crate::LlmRequest| {
                let mut hang = false;
                let (part, input_tokens) = if is_summary_request(&request) {
                    summary_calls.fetch_add(1, Ordering::SeqCst);
                    summary_requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((
                            request.scope.agent_frame_id.clone(),
                            request.scope.request_id.clone(),
                        ));
                    hang = hang_next_summary.swap(false, Ordering::SeqCst);
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
                let overflow = matches!(
                    &part,
                    crate::LlmOutputPart::Text { text, .. } if text == CONTEXT_OVERFLOW_MARKER
                );
                let summary_answered = Arc::clone(&summary_answered);
                async move {
                    if hang {
                        // The provider has answered; the runtime never sees
                        // it, and the law kills the execution.
                        summary_answered.notify_one();
                        std::future::pending::<()>().await;
                    }
                    if overflow {
                        return Ok(crate::LlmResponse {
                            terminal_reason:
                                lash_sansio::llm::types::LlmTerminalReason::ContextOverflow,
                            terminal_diagnostic: Some("prompt is too long".to_string()),
                            ..crate::LlmResponse::default()
                        });
                    }
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
        hang_next_summary,
        summary_answered,
        summary_requests,
    }
}

impl LawModel {
    /// The summarizer calls a law expects after `crash`: one per compaction,
    /// and a second when the crash lost a paid answer before its journal
    /// record (F3: the provider call is at-least-once).
    fn expected_summary_calls(&self, compactions: usize, crash: Option<FrameOpenCrash>) -> usize {
        compactions + usize::from(crash == Some(FrameOpenCrash::AfterProviderAnswer))
    }

    /// Arms the provider-answer crash when `crash` names it, and hands the
    /// law's attempts the signal to die on.
    fn arm(&self, crash: FrameOpenCrash, parts: &mut LawParts) {
        if crash == FrameOpenCrash::AfterProviderAnswer {
            self.hang_next_summary.store(true, Ordering::SeqCst);
            parts.compaction.provider_answered = Some(Arc::clone(&self.summary_answered));
        }
    }
}

/// Compaction session ids and turn ids, as a law records them.
type CompactionIds = Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// The seed the laws' pressure hook opens its frame with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LawSeed {
    /// One summary message.
    #[default]
    Summary,
    /// No seed at all: an explicit empty `OpenFrame` seed.
    Empty,
    /// The summary, a record in the frame being left, and more seed nodes
    /// than a commit may hold: the store refuses the frame's commit.
    Oversized,
}

/// Seed nodes past the laws' commit node budget.
const OVERSIZED_SEED_NODES: usize = 600;
const OVERSIZED_RECORD_TEXT: &str = "a record the refused frame would have left behind";

/// Which plugins compact the law's session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LawCompactor {
    /// The laws' own pressure hook and compactor.
    #[default]
    Law,
    /// The production standard-compaction plugin: its pressure threshold,
    /// its overflow recovery and its compactor.
    Standard,
    /// Two plugins whose pressure hooks share one id, each recording a node
    /// on every turn.
    DuplicateHookIds,
}

/// Holds an administrative compaction once its summary is journaled, before
/// its frame commit, until the law releases it. Released, it holds nothing
/// again: a tier that replays the compaction's handler from the top
/// (Restate's replay leg) replays every step up to the hold, and must not
/// wait for a second release. Holding after the last journaled step keeps
/// every re-execution replaying the same steps, whatever the law does while
/// it holds.
#[derive(Clone)]
struct SummaryHold {
    reached: Arc<tokio::sync::Notify>,
    released: tokio::sync::watch::Sender<bool>,
}

impl Default for SummaryHold {
    fn default() -> Self {
        Self {
            reached: Arc::default(),
            released: tokio::sync::watch::Sender::new(false),
        }
    }
}

impl SummaryHold {
    async fn wait(&self) {
        let mut released = self.released.subscribe();
        if *released.borrow() {
            return;
        }
        self.reached.notify_one();
        // The sender lives in the hold itself, so the channel never closes.
        let _ = released.wait_for(|released| *released).await;
    }

    /// Runs `during` once a summarizer is held, then releases it for good.
    async fn while_held<F: std::future::Future<Output = ()>>(&self, during: F) {
        self.reached.notified().await;
        during.await;
        self.released.send_replace(true);
    }
}

/// How a law's session compacts.
#[derive(Clone, Default)]
struct LawCompaction {
    compactor: LawCompactor,
    seed: LawSeed,
    /// Holds an administrative compaction after its summary.
    hold: Option<SummaryHold>,
    /// Holds the pressure hook after its journaled summary.
    pressure_hold: Option<SummaryHold>,
    /// The signal a provider-answer crash dies on.
    provider_answered: Option<Arc<tokio::sync::Notify>>,
    /// The runtime keeps no store.
    storeless: bool,
    /// Where the standard compactor's pressure request ids are recorded on
    /// every execution that would compact (FIG-4072).
    request_ids: Option<CompactionIds>,
}

/// Everything a runtime for a law is built from, shared by every attempt so
/// each is the same session on the same store.
#[derive(Clone)]
struct LawParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
    protocol: Arc<dyn FrameLawProtocol>,
    compaction: LawCompaction,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: &LawParts, crash: Option<FrameOpenCrash>) -> crate::LashRuntime {
    let mut parts = parts.clone();
    if parts.compaction.compactor == LawCompactor::Standard
        && matches!(
            crash,
            Some(FrameOpenCrash::BeforeSummary | FrameOpenCrash::AfterSummary)
        )
    {
        parts.host.tracing.trace_sink = Some(Arc::new(followup::CompactorCrash {
            crash: crash.expect("the crash point is present"),
        }));
    }
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    let crash_after_summary = crash == Some(FrameOpenCrash::AfterSummary);
    let law = &parts.compaction;
    let compaction: Vec<Arc<dyn PluginFactory>> = match law.compactor {
        LawCompactor::Law => vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-frame-open-compaction",
            crate::facade_support::PluginSpec::new()
                .with_context_pressure_hook(
                    100,
                    Arc::new(ThresholdCompaction {
                        crash_after_summary,
                        seed: law.seed,
                        hold: law.pressure_hold.clone(),
                    }),
                )
                .with_context_compactor(
                    100,
                    Arc::new(SummaryCompactor {
                        crash_before_summary: crash == Some(FrameOpenCrash::BeforeSummary),
                        crash_after_summary,
                        hold: law.hold.clone(),
                    }),
                ),
        ))],
        LawCompactor::Standard => {
            let mut plugins: Vec<Arc<dyn PluginFactory>> = vec![Arc::new(
                lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
            )];
            if let Some(recorded) = &law.request_ids {
                plugins.push(Arc::new(crate::plugin::StaticPluginFactory::new(
                    "conformance-compaction-request-ids",
                    crate::facade_support::PluginSpec::new().with_context_pressure_hook(
                        // Asked before the standard compactor's hook.
                        200,
                        Arc::new(RequestIdProbe {
                            recorded: Arc::clone(recorded),
                        }),
                    ),
                )));
            }
            plugins
        }
        LawCompactor::DuplicateHookIds => DUPLICATE_HOOK_PLUGINS
            .iter()
            .map(|(plugin_id, record)| {
                Arc::new(crate::plugin::StaticPluginFactory::new(
                    plugin_id,
                    crate::facade_support::PluginSpec::new()
                        .with_context_pressure_hook(100, Arc::new(RecordingHook { record })),
                )) as Arc<dyn PluginFactory>
            })
            .collect(),
    };
    let builder =
        crate::LashRuntime::builder(parts.host.clone(), crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(
                parts
                    .protocol
                    .plugins()
                    .into_iter()
                    .chain(compaction)
                    .collect(),
            );
    let builder = if law.storeless {
        builder
    } else {
        builder.with_store(crate::conformance::helpers::session_view(
            &parts.store,
            parts.session_id.clone(),
        ))
    };
    let mut runtime = Box::pin(builder.build())
        .await
        .expect("build the frame-open conformance runtime");
    // The laws' own hook and compactor die at their summary themselves; the
    // probe kills everything else, and a hook the law does not own after its
    // decision.
    if let Some(crash) = crash.filter(|crash| match crash {
        FrameOpenCrash::BeforeSummary | FrameOpenCrash::AfterProviderAnswer => false,
        FrameOpenCrash::AfterSummary => law.compactor != LawCompactor::Law,
        _ => true,
    }) {
        runtime.set_turn_phase_probe(Arc::new(CrashProbe::new(crash)));
    }
    runtime
}

/// Records the compaction session id and turn id the standard compactor's
/// pressure summary derives, on every execution whose prompt usage crosses
/// its threshold, and continues: the standard hook, asked next, compacts.
struct RequestIdProbe {
    recorded: CompactionIds,
}

#[async_trait::async_trait]
impl crate::plugin::ContextPressureHook for RequestIdProbe {
    fn id(&self) -> &'static str {
        "conformance.compaction_request_ids"
    }

    async fn decide(
        &self,
        ctx: &crate::plugin::ContextPressureContext<'_>,
    ) -> Result<crate::plugin::ContextPressureDecision, crate::plugin::ContextError> {
        let compacts = ctx
            .prompt_usage
            .as_ref()
            .is_some_and(|usage| usage.total() >= adversarial::STANDARD_PRESSURE_TOKENS);
        if compacts
            && let Some((session_id, turn_id)) =
                lash_plugin_standard_compaction::pressure_compaction_request_ids(ctx)?
        {
            self.recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((session_id.to_string(), turn_id.to_string()));
        }
        Ok(crate::plugin::ContextPressureDecision::Continue)
    }
}

/// The plugins of [`LawCompactor::DuplicateHookIds`]: two plugin ids, one
/// hook id, and the text each one's hook records.
const DUPLICATE_HOOK_PLUGINS: [(&str, &str); 2] = [
    ("conformance-duplicate-hook-a", "recorded by plugin a"),
    ("conformance-duplicate-hook-b", "recorded by plugin b"),
];

/// A pressure hook that records one message on every turn, under the one id
/// both duplicate-hook plugins register.
struct RecordingHook {
    record: &'static str,
}

#[async_trait::async_trait]
impl crate::plugin::ContextPressureHook for RecordingHook {
    fn id(&self) -> &'static str {
        "conformance.duplicate_hook"
    }

    async fn decide(
        &self,
        _ctx: &crate::plugin::ContextPressureContext<'_>,
    ) -> Result<crate::plugin::ContextPressureDecision, crate::plugin::ContextError> {
        Ok(crate::plugin::ContextPressureDecision::Record {
            nodes: vec![crate::SessionAppendNode::message(
                crate::PluginMessage::text(crate::MessageRole::Assistant, self.record),
            )],
        })
    }
}

/// Runs `attempt` to its end, unless the law armed a provider-answer crash:
/// then the execution dies the moment the hung summarizer call answered,
/// before its answer can be journaled.
async fn unless_the_provider_answer_is_lost<T>(
    parts: &LawParts,
    crash: Option<FrameOpenCrash>,
    attempt: impl std::future::Future<Output = T>,
) -> T {
    let answered = parts
        .compaction
        .provider_answered
        .clone()
        .filter(|_| crash == Some(FrameOpenCrash::AfterProviderAnswer));
    let Some(answered) = answered else {
        return attempt.await;
    };
    tokio::select! {
        biased;
        () = answered.notified() => {
            panic!("injected crash after the summarizer's provider answered, before its journal record")
        }
        ended = attempt => ended,
    }
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
            let drive = unless_the_provider_answer_is_lost(
                &parts,
                crash,
                Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                ))),
            )
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
    head: &LawHead,
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

/// A committed session as a law reads it: the current window's revision and
/// frame, and the whole history paged back from the head.
struct LawHead {
    head_revision: u64,
    current_frame_node_id: Option<crate::FrameNodeId>,
    graph: crate::SessionGraph,
}

/// A law's session, store and first root.
struct LawSession {
    parts: LawParts,
    store: Arc<dyn crate::RuntimeStore>,
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
                compaction: LawCompaction::default(),
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
    async fn enqueue(&self, text: &str) -> crate::InputId {
        self.store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                self.session_id.clone(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text(text),
            ))
            .await
            .expect("accept a queued input")
            .input_id
    }

    async fn head(&self) -> LawHead {
        self.head_of(&self.session_id).await
    }

    /// `session_id`'s committed head and whole history, as [`Self::head`]
    /// reads the law's own session.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn head_of(&self, session_id: &SessionId) -> LawHead {
        let window =
            crate::conformance::helpers::load_current_window(self.store.as_ref(), session_id)
                .await
                .expect("read the session head")
                .expect("the session committed");
        let budget = crate::store::HistoryBudget {
            max_nodes: std::num::NonZeroU32::new(64).expect("nonzero node budget"),
            max_bytes: std::num::NonZeroU64::new(1 << 20).expect("nonzero byte budget"),
        };
        let mut anchor = crate::store::HistoryAnchor::Head;
        let mut newest_first = Vec::new();
        loop {
            let page = self
                .store
                .load_ancestors(session_id, anchor, budget)
                .await
                .expect("page the session history");
            newest_first.extend(page.nodes.into_iter().map(|node| node.record));
            match page.next {
                Some(cursor) => anchor = crate::store::HistoryAnchor::Cursor(cursor),
                None => break,
            }
        }
        let leaf = newest_first.first().map(|node| node.node_id.clone());
        newest_first.reverse();
        LawHead {
            head_revision: window.head_revision,
            current_frame_node_id: window.current_frame_node_id,
            graph: crate::SessionGraph::from_nodes(newest_first, leaf)
                .expect("the paged history is one graph"),
        }
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
    let mut law = LawSession::open(
        prefix,
        &format!("pressure-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    model.arm(crash, &mut law.parts);

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
        model.expected_summary_calls(1, Some(crash)),
        "one summarizer call: a redrive reads the summary back from its journal, \
         and requests only an answer the journal never recorded again"
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
    let mut law = LawSession::open(
        prefix,
        &format!("pressure-continue-as-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
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

    assert_eq!(
        model.summary_calls.load(Ordering::SeqCst),
        model.expected_summary_calls(1, Some(crash)),
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

/// An administrative compaction, killed at `crash` and redriven, opens its
/// frame once: one summarizer call, one compaction frame, one commit.
/// Killed after its commit, the redrive loads the moved head, replays the
/// recorded base and the summary over it, and meets the commit's receipt
/// instead of opening a second frame from the moved head (FIG-4133).
pub async fn a_compaction_frame_opens_once_whatever_its_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: FrameOpenCrash,
) {
    followup::compaction_crash_case(
        prefix,
        effect_host,
        stores,
        runner,
        crash,
        LawCompactor::Law,
        false,
    )
    .await;
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
        #[ignore = "FIG-4165: pressure CAS loss leaves the root bound to its old admission; recovery needs an admission policy"]
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn reverse_compact_pressure_overlap_redrives_once() {
            let (_guard, prefix, host, stores, runner, protocol) = $fixture;
            $crate::registration_macro_support::reverse_compact_pressure_overlap_redrives_once(
                prefix, host, stores, runner, protocol,
            ).await;
        }
        $crate::frame_open_protocol_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (a_pressure_frame_crashed_after_its_provider_answered_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterProviderAnswer),
            (a_pressure_frame_crashed_after_its_summary_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterSummary),
            (a_pressure_frame_crashed_after_its_commit_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterFrameCommit),
            (a_pressure_frame_crashed_after_its_turns_commit_opens_once,
                a_pressure_frame_opens_once_whatever_its_crash, AfterTurnCommit),
            (a_pressure_frame_then_continue_as_crashed_after_the_provider_answered_commits_both_once,
                a_pressure_frame_then_continue_as_commits_both_frames_once, AfterProviderAnswer),
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

/// Register the execution-state frame-open laws (FIG-4110, F5; FIG-4134)
/// over a protocol with live execution state: a pressure frame, a staged
/// open and administrative compactions with and without a store each restart
/// the live interpreter. The fixture hands back what
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
        $crate::frame_open_execution_state_tests!(@path [$(#[$attr])*] $fixture;
            (a_staged_open_restarts_the_live_execution_state, Staged),
            (a_compaction_restarts_the_live_execution_state, Compact),
            (a_storeless_compaction_restarts_the_live_execution_state, StorelessCompact));
    };
    (@path [$($attrs:tt)*] $fixture:block; ($name:ident, $path:ident) $(, $rest:tt)*) => {
        $($attrs)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let (_guard, prefix, host, stores, runner, protocol) = $fixture;
            $crate::registration_macro_support::every_open_restarts_the_live_execution_state(
                prefix,
                host,
                stores,
                runner,
                protocol,
                $crate::registration_macro_support::LiveResetPath::$path,
            )
            .await;
        }
        $crate::frame_open_execution_state_tests!(@path [$($attrs)*] $fixture; $($rest),*);
    };
    (@path [$($attrs:tt)*] $fixture:block;) => {};
}

/// Register the frame-open laws (FIG-4110) on the standard protocol: the
/// protocol laws of [`frame_open_protocol_redrive_tests`]; an administrative
/// compaction killed before its summary, after its provider answered, after
/// its summary and after its commit (FIG-4133); and the FIG-4134 laws: the
/// production standard compactor and its overflow recovery across the crash
/// matrix, an administrative compaction superseded by a newer admission or
/// by a pressure frame, a session deleted and a fork made during an open, an
/// empty seed, a refused frame commit, and pressure hooks sharing an id; and
/// FIG-4200's root whose held pressure frame another runtime overtakes,
/// ending typed on the drive loop and the engine path, uninterrupted, across
/// a crash before its end and on a fresh journal. The fixture hands back a
/// guard, a prefix, the tier's effect host, the store set under test and its
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
        $crate::frame_open_redrive_tests!(@crashed [$(#[$attr])*] $fixture;
            (a_compaction_crashed_before_its_summary_opens_once,
                a_compaction_frame_opens_once_whatever_its_crash, BeforeSummary),
            (a_compaction_crashed_after_its_provider_answered_opens_once,
                a_compaction_frame_opens_once_whatever_its_crash, AfterProviderAnswer),
            (a_compaction_crashed_after_its_summary_opens_once,
                a_compaction_frame_opens_once_whatever_its_crash, AfterSummary),
            (a_compaction_crashed_after_its_commit_opens_once,
                a_compaction_frame_opens_once_whatever_its_crash, AfterFrameCommit),
            (a_standard_compaction_frame_crashed_after_its_provider_answered_opens_once,
                a_standard_compaction_frame_opens_once_whatever_its_crash, AfterProviderAnswer),
            (a_standard_compaction_frame_crashed_after_its_summary_opens_once,
                a_standard_compaction_frame_opens_once_whatever_its_crash, AfterSummary),
            (a_standard_compaction_frame_crashed_after_its_commit_opens_once,
                a_standard_compaction_frame_opens_once_whatever_its_crash, AfterFrameCommit),
            (a_standard_compaction_frame_crashed_before_its_turns_commit_opens_once,
                a_standard_compaction_frame_opens_once_whatever_its_crash, BeforeTurnCommit),
            (a_standard_compaction_frame_crashed_after_its_turns_commit_opens_once,
                a_standard_compaction_frame_opens_once_whatever_its_crash, AfterTurnCommit),
            (an_overflow_recovery_frame_crashed_after_its_provider_answered_opens_once,
                an_overflow_recovery_frame_opens_once_whatever_its_crash, AfterProviderAnswer),
            (an_overflow_recovery_frame_crashed_after_its_summary_opens_once,
                an_overflow_recovery_frame_opens_once_whatever_its_crash, AfterSummary),
            (an_overflow_recovery_frame_crashed_after_its_commit_opens_once,
                an_overflow_recovery_frame_opens_once_whatever_its_crash, AfterFrameCommit),
            (an_overflow_recovery_frame_crashed_before_its_turns_commit_opens_once,
                an_overflow_recovery_frame_opens_once_whatever_its_crash, BeforeTurnCommit),
            (an_overflow_recovery_frame_crashed_after_its_turns_commit_opens_once,
                an_overflow_recovery_frame_opens_once_whatever_its_crash, AfterTurnCommit),
            (an_empty_pressure_seed_crashed_after_its_commit_opens_one_frame,
                an_empty_pressure_seed_opens_one_frame, AfterFrameCommit),
            (an_empty_pressure_seed_crashed_after_its_turns_commit_opens_one_frame,
                an_empty_pressure_seed_opens_one_frame, AfterTurnCommit),
            (pressure_hooks_sharing_an_id_crashed_after_the_turns_commit_keep_their_records_apart,
                pressure_hooks_sharing_an_id_keep_their_records_apart, AfterTurnCommit));
        $crate::frame_open_redrive_tests!(@superseded [$(#[$attr])*] $fixture;
            (a_superseded_root_ends_typed_on_the_drive_loop, DriveLoop, None),
            (a_superseded_root_ends_typed_on_the_drive_loop_across_a_crash_before_its_end,
                DriveLoop, CrashBeforeEnd),
            (a_superseded_root_ends_typed_on_the_drive_loop_on_a_fresh_journal,
                DriveLoop, FreshJournal),
            (a_superseded_root_ends_typed_on_the_engine_path, Engine, None),
            (a_superseded_root_ends_typed_on_the_engine_path_across_a_crash_before_its_end,
                Engine, CrashBeforeEnd),
            (a_superseded_root_ends_typed_on_the_engine_path_on_a_fresh_journal,
                Engine, FreshJournal));
        $crate::frame_open_redrive_tests!(@once [$(#[$attr])*] $fixture;
            redrive_after_commit_with_sealed_admission_reports_opened,
            compact_with_production_compactor_crash_matrix,
            a_compaction_superseded_by_a_newer_admission_is_refused,
            a_compaction_overlapping_a_pressure_frame_is_refused,
            a_session_deleted_during_an_open_keeps_nothing_of_it,
            a_fork_made_during_an_open_never_sees_its_seed,
            a_refused_frame_commit_leaves_nothing_visible);
    };
    (@crashed [$($attrs:tt)*] $fixture:block; ($name:ident, $law:ident, $crash:ident) $(, $rest:tt)*) => {
        $($attrs)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(
                prefix,
                host,
                stores,
                runner,
                $crate::registration_macro_support::FrameOpenCrash::$crash,
            )
            .await;
        }
        $crate::frame_open_redrive_tests!(@crashed [$($attrs)*] $fixture; $($rest),*);
    };
    (@crashed [$($attrs:tt)*] $fixture:block;) => {};
    (@superseded [$($attrs:tt)*] $fixture:block; ($name:ident, $path:ident, $recovery:ident) $(, $rest:tt)*) => {
        $($attrs)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::a_superseded_root_ends_typed_on_every_drive_path(
                prefix,
                host,
                stores,
                runner,
                $crate::registration_macro_support::SupersededRootPath::$path,
                $crate::registration_macro_support::SupersededRootRecovery::$recovery,
            )
            .await;
        }
        $crate::frame_open_redrive_tests!(@superseded [$($attrs)*] $fixture; $($rest),*);
    };
    (@superseded [$($attrs:tt)*] $fixture:block;) => {};
    (@once [$($attrs:tt)*] $fixture:block; $law:ident $(, $rest:ident)*) => {
        $($attrs)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
        $crate::frame_open_redrive_tests!(@once [$($attrs)*] $fixture; $($rest),*);
    };
    (@once [$($attrs:tt)*] $fixture:block;) => {};
}
