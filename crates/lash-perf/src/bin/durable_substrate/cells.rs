//! The RLM cell turn (H5): a cell session runs behind the lash facade, on
//! the production turn driver, the RLM worker path, its durable snapshot
//! store and the production tool dispatch. Only the model and `ext_echo`'s
//! body are the bench's.
//!
//! The model answers a cell session's first call with a TypeScript cell that
//! awaits `ext_echo` `cell_calls` times, keeping each answer (its padding
//! included) in its heap, and its second with a text answer.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use lash::rlm::Dialect as _;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_core::runtime::durable::session::TurnServices;
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolOutcome};
use lash_core_execution::Backend;
use lash_sansio::SessionId;

use crate::recorder::Recorder;
use crate::turn::{ANSWER, Scripts, session_of_admission};

/// The `Once` tool a cell awaits.
const CELL_TOOL: &str = "ext_echo";
/// The model every cell session names.
const MODEL: &str = "durable-substrate-cell-model";

/// A node's cell core over `backend`, running `scripts`, reporting model
/// calls to `recorder`. It serves no node of its own: the bench's node
/// serves its sessions.
///
/// # Errors
///
/// The core does not build.
pub fn core(
    backend: &Backend,
    scripts: Arc<Scripts>,
    recorder: Arc<Recorder>,
) -> Result<lash::LashCore> {
    lash::LashCore::rlm_builder(
        backend.clone(),
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(100_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(512))
                .build(),
            Arc::new(lash::rlm::TypescriptDialect),
            backend,
        )
        .with_worker_service(untimed_workers()),
    )
    .serve_sessions(false)
    .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .serve_test_llm_profile(model(scripts, recorder), metadata()?)
    .tools(echo()?)
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        "durable-substrate",
        "durable-substrate-boot",
    ))
    .map_err(|error| anyhow::anyhow!("build the cell core: {error}"))
}

/// The turn services `core`'s sessions run with.
#[must_use]
pub fn services(core: &lash::LashCore) -> Arc<dyn TurnServices> {
    lash::testing::session_turn_services(core)
}

/// Create the cell session `session` through `core`.
///
/// # Errors
///
/// The facade refused.
pub async fn create_session(core: &lash::LashCore, session: &SessionId) -> Result<()> {
    core.session(session.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1_000_000),
        )))
        .await
        .map(drop)
        .map_err(|error| anyhow::anyhow!("create {session}: {error}"))
}

/// The dialect's worker service with its run deadlines off the clock: the
/// bench measures the cell, not a deadline.
fn untimed_workers() -> lash::rlm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    lash::rlm::WorkerService::new(config)
}

fn metadata() -> Result<lash_core::LlmProfileMetadata> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(1_000_000)
        .build()
        .map_err(|error| anyhow::anyhow!("the cell model's metadata: {error}"))
}

/// The cell `session`'s script asks for.
fn cell_source(scripts: &Scripts, session: &SessionId) -> String {
    let script = scripts.get(session);
    let pad = "x".repeat(script.cell_payload);
    format!(
        "<typescript>\nconst kept = [];\nfor (let i = 0; i < {calls}; i++) {{\n  const answer = await tools.{CELL_TOOL}({{ i: i, pad: \"{pad}\" }});\n  kept.push(answer);\n}}\nprint(kept.length);\n</typescript>",
        calls = script.cell_calls,
    )
}

/// The model: a cell session's first call is answered with its cell, every
/// call that sees the cell's answer with [`ANSWER`].
fn model(scripts: Arc<Scripts>, recorder: Arc<Recorder>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("durable-substrate-cell")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let scripts = Arc::clone(&scripts);
            let recorder = Arc::clone(&recorder);
            async move {
                let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
                let session = session_of_admission(&rendered);
                if let Some(session) = &session {
                    recorder.model_call(session);
                }
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                let text = match session {
                    Some(session) if !answered => cell_source(&scripts, &session),
                    _ => ANSWER.to_owned(),
                };
                Ok(streamed(&request, &text))
            }
        })
        .build()
        .into_handle()
}

/// A text answer, streamed as one delta.
fn streamed(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Delta {
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        });
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

/// `ext_echo`: a `Once` tool that answers its arguments, padding included.
struct Echo;

#[async_trait::async_trait]
impl StaticToolExecute for Echo {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(call.args.clone()).into()
    }
}

fn echo() -> Result<Arc<dyn lash_core::ToolProvider>> {
    let definition = lash_core::ToolDefinition::raw(
        CELL_TOOL,
        CELL_TOOL,
        "Answers its arguments.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "i": { "type": "number" }, "pad": { "type": "string" } },
            "required": ["i", "pad"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .map_err(|error| anyhow::anyhow!("ext_echo's schemas: {error}"))?
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], CELL_TOOL));
    Ok(Arc::new(StaticToolProvider::new(vec![definition], Echo)))
}
