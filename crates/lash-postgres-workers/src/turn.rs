//! The turn workload: one session, one turn, one cell, one `Once` tool call.
//!
//! The scripted model answers the turn's first call with a TypeScript cell
//! that calls the `Once` tool `ext_write({ x: 7 })`, and its second call
//! (once the cell's answer is in the transcript) with a final answer. The
//! tool's body writes its witness entry, may hold, and answers what it
//! wrote. Only the model and the tool's body are the runbook's: the session,
//! its turn, the cell and the tool call run behind the lash facade on the
//! production session activation, RLM worker path and tool dispatch, and the
//! turn commits the session's real head.

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::Arc;
use std::time::Duration;

use lash::rlm::Dialect as _;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_core::{ExecutionPolicy, LlmOutputPart, ToolCall, ToolOutcome};
use lash_core_execution::Backend;
use lash_durable::ActorKey;
use lash_sansio::{SessionId, TurnId};

use crate::events::{Event, report};
use crate::witness::{Hold, Witness};

/// The runbook's one session.
pub const SESSION: &str = "workers-session";
/// Its one turn.
pub const RUN: &str = "workers-turn";
/// The `Once` tool the cell calls.
pub const TOOL: &str = "ext_write";
/// The scripted model's name.
const MODEL: &str = "workers-model";

/// The cell the model writes: one `Once` tool call, and what it answered.
const CELL: &str = "<typescript>\nconst written = await tools.ext_write({ x: 7 });\nprint(written);\n</typescript>";
/// What the final answer starts with.
pub const FINAL_PREFIX: &str = "final answer from ";

/// The session's id.
///
/// # Panics
///
/// Never: the id is a valid literal.
#[must_use]
#[expect(clippy::expect_used, reason = "a literal session id is valid")]
pub fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).expect("a valid session id")
}

/// The turn's id.
///
/// # Panics
///
/// Never: the id is a valid literal.
#[must_use]
#[expect(clippy::expect_used, reason = "a literal turn id is valid")]
pub fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).expect("a valid turn id")
}

/// The session's actor.
///
/// # Panics
///
/// Never: the key is a valid literal.
#[must_use]
#[expect(clippy::expect_used, reason = "a literal actor key is valid")]
pub fn actor() -> ActorKey {
    ActorKey::session(SESSION).expect("a valid actor key")
}

/// `ext_write`: a `Once` tool whose body writes its witness entry, holds
/// where the case asks, and answers what it wrote.
struct ExtWrite {
    witness: Witness,
    hold: Hold,
}

#[async_trait::async_trait]
impl StaticToolExecute for ExtWrite {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let id = call.context.call_id().to_string();
        let witness = &self.witness;
        witness.entered(&id, TOOL).await;
        report(
            witness.node(),
            Event::Body {
                call: id.clone(),
                tool: TOOL.to_owned(),
                phase: "entered".to_owned(),
            },
        );
        match self.hold {
            Hold::Step => std::future::pending::<()>().await,
            Hold::StepUntilRelease => witness.released().await,
            Hold::Nothing | Hold::Model | Hold::ModelUntilRelease => {}
        }
        witness.returned(&id, TOOL).await;
        report(
            witness.node(),
            Event::Body {
                call: id,
                tool: TOOL.to_owned(),
                phase: "returned".to_owned(),
            },
        );
        ToolOutcome::ok(serde_json::json!({ "ok": true, "wrote": call.args })).into()
    }
}

#[expect(clippy::expect_used, reason = "the tool's schemas are literals")]
fn ext_write(witness: Witness, hold: Hold) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Writes x to the outside world, once.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "x": { "type": "number" } },
            "required": ["x"]
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("ext_write's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL));
    Arc::new(StaticToolProvider::new(
        vec![definition],
        ExtWrite { witness, hold },
    ))
}

/// The scripted model: before the transcript holds its own call it answers
/// with the cell; after, with a final answer that quotes the transcript's
/// end. Every attempt writes its witness entry first, numbered by the
/// ledger; the first attempt of the first call holds where the case asks.
fn model(witness: Witness, hold: Hold) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("workers-scripted")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let witness = witness.clone();
            async move {
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                let call = if answered { 2 } else { 1 };
                let attempt = witness.model_attempt(call).await;
                report(witness.node(), Event::ModelAttempt { call, attempt });
                if call == 1 && attempt == 1 {
                    match hold {
                        Hold::Model => std::future::pending::<()>().await,
                        Hold::ModelUntilRelease => witness.released().await,
                        Hold::Nothing | Hold::Step | Hold::StepUntilRelease => {}
                    }
                }
                let text = if answered {
                    let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
                    let start = rendered.len().saturating_sub(160);
                    let start = (start..rendered.len())
                        .find(|at| rendered.is_char_boundary(*at))
                        .unwrap_or(rendered.len());
                    format!("{FINAL_PREFIX}{}", &rendered[start..])
                } else {
                    CELL.to_owned()
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
        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        }));
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

#[expect(clippy::expect_used, reason = "the model's metadata is a literal")]
fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// The dialect's worker service with its run deadlines off the clock: a
/// held tool call keeps its cell waiting for as long as the case holds it.
fn untimed_workers() -> lash::vm::WorkerService {
    const OFF_THE_CLOCK: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash::rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    lash::vm::WorkerService::new(config)
}

/// The runbook's deployment over `backend`, as `witness` writes to the
/// outside world and holding at `hold`. It serves no node of its own: the
/// node's runner serves its sessions ([`crate::node`]).
///
/// # Errors
///
/// The core does not build.
pub fn core(backend: &Backend, witness: Witness, hold: Hold) -> Result<lash::LashCore, String> {
    lash::LashCore::rlm_builder(
        backend.clone(),
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::rlm::TypescriptDialect),
            backend,
        )
        .with_worker_service(untimed_workers()),
    )
    .serve_sessions(false)
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .serve_test_llm_profile(model(witness.clone(), hold), metadata())
    .tools(ext_write(witness.clone(), hold))
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("workers-deployment"),
        lash::persistence::LeaseIncarnationId::new(witness.node()),
    ))
    .map_err(|error| format!("build the core: {error}"))
}

/// Create the session and send it the turn's input through `core`: what a
/// host outside the deployment does.
///
/// # Errors
///
/// The facade refused.
pub async fn admit(core: &lash::LashCore) -> Result<(), String> {
    core.session(session())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(8),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .map_err(|error| format!("create the session: {error}"))?
        .send(lash::TurnInput::text(
            "write x, then tell me what was written",
        ))
        .id(run())
        .await
        .map(drop)
        .map_err(|error| format!("send the turn's input: {error}"))
}
