//! FIG-4589: a compaction's system prompt is rendered once, as a recorded
//! step taken before its summarizer call, on every compaction path.
//!
//! The protocol plugin renders the prompt, and a fresh render may use
//! current code, so two workers can render different text for one session.
//! These laws run a compaction on a protocol whose prompt names the worker
//! that rendered it, kill the worker between the journaled summary and the
//! frame's commit, and redrive on another worker. The redrive must serve the
//! recorded prompt: it renders nothing, and its summarizer request matches
//! the journaled one, so the summary is read back and the frame opens once.
//! A redrive that rendered again would send the other worker's text, and
//! diverge from its own journal.

use super::*;
use crate::ActorContext;
use pretty_assertions::assert_eq;

/// The start of every prompt the laws' protocol renders.
const WORKER_PROMPT: &str = "frame-open-law prompt rendered by worker";

/// The standard fake protocol with a system prompt: each runtime built from
/// it is another worker, whose renders name it.
struct WorkerPromptProtocol {
    workers: AtomicUsize,
    compaction_renders: Arc<AtomicUsize>,
}

impl WorkerPromptProtocol {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            workers: AtomicUsize::new(0),
            compaction_renders: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl FrameLawProtocol for WorkerPromptProtocol {
    fn plugins(&self) -> Vec<Arc<dyn PluginFactory>> {
        let worker = self.workers.fetch_add(1, Ordering::SeqCst);
        vec![
            crate::testing::test_standard_protocol_factory_with_runtime_state(
                Arc::new(WorkerPromptSession {
                    worker,
                    compaction_renders: Arc::clone(&self.compaction_renders),
                }),
                None,
            ),
            switch_tool_plugin(),
        ]
    }

    fn protocol_plugin_id(&self) -> &'static str {
        "test_protocol"
    }

    fn answer(&self, text: &str) -> crate::LlmOutputPart {
        standard_answer(text)
    }

    fn continue_as(&self, task: &str) -> crate::LlmOutputPart {
        standard_continue_as(task)
    }
}

/// One worker's protocol session: its prompt names the worker, and it counts
/// the compaction prompts it renders.
struct WorkerPromptSession {
    worker: usize,
    compaction_renders: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::plugin::ProtocolSessionPlugin for WorkerPromptSession {
    async fn render_system_prompt(
        &self,
        ctx: crate::plugin::SystemPromptContext<'_>,
    ) -> Result<Arc<str>, crate::SessionError> {
        if ctx.purpose == crate::plugin::SystemPromptPurpose::Compaction {
            self.compaction_renders.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Arc::from(format!("{WORKER_PROMPT} {}", self.worker)))
    }
}

/// What every path's law holds once its crash case ran: the path opened its
/// frame once from one summarizer call (the crash case asserts both), the
/// compaction's prompt was rendered once, and the one summarizer request
/// carried that render.
fn assert_the_recorded_prompt_was_replayed(protocol: &WorkerPromptProtocol, model: &LawModel) {
    assert_eq!(
        protocol.compaction_renders.load(Ordering::SeqCst),
        1,
        "the redrive serves the compaction prompt its first execution recorded and renders none"
    );
    let instructions = model
        .summary_instructions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        instructions.len(),
        1,
        "the summary is read back from the journal: {instructions:?}"
    );
    assert!(
        instructions[0]
            .as_deref()
            .is_some_and(|prompt| prompt.starts_with(WORKER_PROMPT)),
        "the summarizer call carries the protocol's rendered prompt: {instructions:?}"
    );
    assert!(
        protocol.workers.load(Ordering::SeqCst) >= 2,
        "the redrive ran on another worker than the one that rendered"
    );
}

/// An administrative compaction (`SessionCommand::CompactContext`) killed
/// after its summary is journaled and redriven on another worker replays its
/// recorded prompt.
pub async fn a_commanded_compaction_redriven_on_another_worker_replays_its_recorded_prompt(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = WorkerPromptProtocol::new();
    let model = followup::compaction_crash_case_under(
        &format!("{prefix}-recorded-prompt"),
        effect_host,
        stores,
        runner,
        FrameOpenCrash::AfterSummary,
        LawCompactor::Standard,
        false,
        Arc::clone(&protocol) as Arc<dyn FrameLawProtocol>,
    )
    .await;
    assert_the_recorded_prompt_was_replayed(&protocol, &model);
}

/// A context-pressure compaction killed after its summary is journaled and
/// redriven on another worker replays its recorded prompt.
pub async fn a_pressure_compaction_redriven_on_another_worker_replays_its_recorded_prompt(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = WorkerPromptProtocol::new();
    let model = adversarial::standard_pressure_crash_case(
        &format!("{prefix}-recorded-prompt"),
        effect_host,
        stores,
        runner,
        FrameOpenCrash::AfterSummary,
        Arc::clone(&protocol) as Arc<dyn FrameLawProtocol>,
    )
    .await;
    assert_the_recorded_prompt_was_replayed(&protocol, &model);
}

/// An overflow recovery killed after its summary is journaled and redriven
/// on another worker replays its recorded prompt.
pub async fn an_overflow_recovery_redriven_on_another_worker_replays_its_recorded_prompt(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = WorkerPromptProtocol::new();
    let model = adversarial::overflow_recovery_crash_case(
        &format!("{prefix}-recorded-prompt"),
        effect_host,
        stores,
        runner,
        FrameOpenCrash::AfterSummary,
        Arc::clone(&protocol) as Arc<dyn FrameLawProtocol>,
    )
    .await;
    assert_the_recorded_prompt_was_replayed(&protocol, &model);
}
