//! Context-overflow recovery harness (`runbooks/context-overflow-recovery`).
//!
//! One process, one SQLite scratch store set and a local `restate-server`,
//! the zero-infra engine (ADR 0104 §4); no container and no token. Driven by
//! `scripts/context-overflow-recovery-e2e.sh`, which runs it under
//! `scripts/ci/with-service.sh restate` and owns the artifact directory and
//! the exact gates.
//!
//! The scenario is FIG-1272's whole claim, end to end:
//!
//! 1. A turn calls a tool whose result is far larger than anything the prompt
//!    budget anticipated. The proactive compaction check reads the *previous*
//!    turn's reported usage, so it cannot see this result: the overflow lands
//!    mid-turn, which is exactly the case the ticket exists for.
//! 2. The provider refuses the next request as too large. The turn stops as
//!    `TurnStop::ContextOverflow` — its own outcome, not the undifferentiated
//!    `ProviderError` an auth failure or a 500 produces.
//! 3. The host reads that outcome off the public turn report
//!    (`TurnReport::is_context_overflow`).
//! 4. The same session runs another turn and finishes. Nothing was restarted.
//!
//! A separate standard-protocol session exercises plugin-owned overflow
//! recovery on the same Restate stack. The RLM outcome arms remain separate.
//!
//! A control phase drives the same harness into a plain provider error and
//! requires a *different* stop, because "distinguishable from a provider
//! error" is the claim, and one outcome observed alone never proves a
//! distinction.
//!
//! Every phase prints one JSON `checkpoint` line on stdout. The script's gate
//! reads those lines; nothing here judges itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use lash::SessionId;
use lash::plugins::{PluginRegistrar, PluginSessionContext, SessionPlugin};
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolBinding, ToolCall,
    ToolDefinition, ToolDefinitionBindingExt, ToolOutcome, ToolProvider,
};
use serde_json::{Value, json};

/// The oversized tool result. Large enough that no reader mistakes it for an
/// ordinary payload, small enough that the harness stays fast.
const OVERSIZED_BYTES: usize = 512 * 1024;

/// The tool the scripted cell calls to pull the oversized result into the
/// turn's context.
const OVERSIZED_TOOL: &str = "oversized_report";
const RECOVERY_PENDING: &str = "Standard-compaction context-overflow recovery marker (pending):";
const RECOVERY_COMPLETED: &str = "Standard-compaction context-overflow recovery completed:";
const RECOVERY_SUMMARY: &str = "Compaction summary:";

#[tokio::main]
async fn main() -> Result<()> {
    let run_id = format!(
        "{:x}",
        lash_restate_postgres_workers_e2e::current_epoch_ms()
    );

    // Arm 1: the provider states the terminal reason itself.
    let overflow = overflow_and_recovery(
        &run_id,
        Script::Overflow,
        "context_overflow_recovered",
        "recovery",
    )
    .await?;
    emit(&overflow);
    // This is the path a real provider takes, and the one that used to collapse into
    // `ProviderError` no matter how well it had been classified.
    let classified = overflow_and_recovery(
        &run_id,
        Script::ClassifiedOverflow,
        "classified_overflow_recovered",
        "classified",
    )
    .await?;
    emit(&classified);
    let control = provider_error_control(&run_id).await?;
    emit(&control);
    emit(&standard_plugin_recovery(&run_id).await?);
    Ok(())
}

fn emit(checkpoint: &Value) {
    println!("{checkpoint}");
}

/// Read committed messages across both the old and current frames.
async fn durable_messages(session: &lash::LashSession) -> Result<Vec<lash_core::Message>> {
    let durable = session.durable();
    let mut messages = Vec::new();
    let mut anchor = lash::persistence::HistoryAnchor::Head;
    loop {
        let page = durable
            .history(
                anchor,
                lash::persistence::HistoryBudget {
                    max_nodes: std::num::NonZeroU32::new(128).context("positive page limit")?,
                    max_bytes: std::num::NonZeroU64::new(32 * 1024 * 1024)
                        .context("positive byte limit")?,
                },
            )
            .await?;
        for node in page.nodes {
            if let lash::persistence::SessionNodePayload::Event {
                event: lash::persistence::SessionHistoryRecord::Conversation(message),
            } = node.record.payload
            {
                messages.push(message.to_message());
            }
        }
        match page.next {
            Some(next) => anchor = lash::persistence::HistoryAnchor::Cursor(next),
            None => break,
        }
    }
    messages.reverse();
    Ok(messages)
}

fn plugin_record(message: &lash_core::Message, title: &str) -> bool {
    matches!(message.origin, Some(lash_core::MessageOrigin::Plugin { ref plugin_id, .. }) if plugin_id == "standard_compaction")
        && message
            .parts
            .iter()
            .any(|part| part.content().starts_with(title))
}

/// A standard turn uses a native tool call, then the plugin summarizes the
/// committed overflow and opens a frame before the next turn runs.
async fn standard_plugin_recovery(run_id: &str) -> Result<Value> {
    let harness = Harness::new(Script::ClassifiedOverflow, Protocol::Standard).await?;
    let session_id = SessionId::from(format!("context-overflow-standard-{run_id}"));
    let session = harness.open(&session_id).await?;
    let overflow = session
        .send(lash::TurnInput::text("summarize the attached report"))
        .output()
        .await
        .context("standard overflow turn")?;
    let before = session.read_view();
    let pending = before
        .messages()
        .iter()
        .any(|message| plugin_record(message, RECOVERY_PENDING));
    let frame_before = before.to_snapshot().current_frame_node_id;

    let continued = session
        .send(lash::TurnInput::text("now give me the verdict"))
        .output()
        .await
        .context("standard recovery turn")?;
    let after = session.read_view();
    let snapshot = after.to_snapshot();
    let frame_after = snapshot.current_frame_node_id.clone();
    let frame_reason = snapshot
        .agent_frames
        .iter()
        .find(|frame| Some(&frame.frame_node_id) == frame_after.as_ref())
        .map(|frame| frame.reason.as_str().to_string());
    let history = durable_messages(&session).await?;
    let summary_chars = after
        .messages()
        .iter()
        .find_map(|message| {
            message.parts.iter().find_map(|part| {
                part.content()
                    .strip_prefix(RECOVERY_SUMMARY)
                    .map(|text| text.trim().len())
            })
        })
        .unwrap_or(0);

    Ok(json!({
        "checkpoint": "standard_plugin_recovered",
        "protocol": "standard",
        "session_id": session_id.as_str(),
        "oversized_tool_result_bytes": harness.served_tool_bytes(),
        "provider_calls": harness.provider_calls(),
        "overflow_stop": stop_tag(&overflow.result.outcome)?,
        "overflow_is_context_overflow": overflow.result.is_context_overflow(),
        "plugin_recovery_pending": pending,
        "plugin_recovery_completed": history.iter().any(|message| plugin_record(message, RECOVERY_COMPLETED)),
        "plugin_recovery_summary_chars": summary_chars,
        "recovery_frame_reason": frame_reason,
        "recovery_frame_moved": frame_before != frame_after,
        "continued_is_success": continued.result.is_success(),
        "continued_is_context_overflow": continued.result.is_context_overflow(),
        "continued_assistant_message": continued.result.assistant_message(),
    }))
}

/// Phases 1-4: overflow, its own outcome, continued session.
///
/// The two differ only in how the provider states the overflow -- a terminal reason on an
/// accepted response, or a failure whose text lash classifies itself -- and the point of
/// running both is that the outcome and the continued session must be identical either way.
async fn overflow_and_recovery(
    run_id: &str,
    script: Script,
    checkpoint: &str,
    session_tag: &str,
) -> Result<Value> {
    let harness = Harness::new(script, Protocol::Rlm).await?;
    let session_id = SessionId::from(format!("context-overflow-{session_tag}-{run_id}"));
    let session = harness.open(&session_id).await?;

    let overflow = session
        .send(lash::TurnInput::text("summarize the attached report"))
        .output()
        .await;
    let overflow = match overflow {
        Ok(output) => output,
        Err(error) => bail!("the overflow turn did not settle into a report: {error}"),
    };
    let overflow_stop = stop_tag(&overflow.result.outcome)?;
    let tool_bytes = harness.served_tool_bytes();

    // The same session, not a new one: the claim is that the session continues.
    let continued = session
        .send(lash::TurnInput::text("now give me the verdict"))
        .output()
        .await
        .map_err(|err| anyhow!("{err}"))
        .context("the continued turn")?;
    let continued_history_len = session.read_view().messages().len();

    Ok(json!({
        "checkpoint": checkpoint,
        "dialect": SERVED_DIALECT,
        "session_id": session_id.as_str(),
        "oversized_tool_result_bytes": tool_bytes,
        "provider_calls": harness.provider_calls(),
        "overflow_outcome": serde_json::to_value(&overflow.result.outcome)
            .context("serialize the overflow outcome")?,
        "overflow_stop": overflow_stop,
        "overflow_is_context_overflow": overflow.result.is_context_overflow(),
        "overflow_is_success": overflow.result.is_success(),
        "continued_stop": stop_tag(&continued.result.outcome)?,
        "history_messages_after_continued_turn": continued_history_len,
        "continued_is_success": continued.result.is_success(),
        "continued_is_context_overflow": continued.result.is_context_overflow(),
        "continued_assistant_message": continued.result.assistant_message(),
        "continued_final_value": continued.result.final_value(),
    }))
}

/// The control: a plain provider error on the same harness must not produce
/// the overflow outcome.
async fn provider_error_control(run_id: &str) -> Result<Value> {
    let harness = Harness::new(Script::ProviderError, Protocol::Rlm).await?;
    let session_id = SessionId::from(format!("context-overflow-control-{run_id}"));
    let session = harness.open(&session_id).await?;

    let failed = session
        .send(lash::TurnInput::text("summarize the attached report"))
        .output()
        .await;
    let outcome = match failed {
        Ok(output) => {
            serde_json::to_value(&output.result.outcome).context("serialize the control outcome")?
        }
        Err(error) => bail!("the control turn did not settle into a report: {error}"),
    };
    let stop = outcome
        .get("stopped")
        .and_then(stop_name)
        .ok_or_else(|| anyhow!("the control turn did not stop: {outcome}"))?;

    Ok(json!({
        "checkpoint": "provider_error_control",
        "dialect": SERVED_DIALECT,
        "session_id": session_id.as_str(),
        "control_stop": stop,
        "control_outcome": outcome,
    }))
}

fn stop_tag(outcome: &lash_core::facade_support::TurnOutcome) -> Result<Option<String>> {
    let value = serde_json::to_value(outcome).context("serialize a turn outcome")?;
    Ok(value.get("stopped").and_then(stop_name))
}

/// `TurnStop` serializes a unit variant as a bare string and a data-carrying
/// one as a single-key object; read the tag from serde rather than restating
/// the spellings here.
fn stop_name(stopped: &Value) -> Option<String> {
    match stopped {
        Value::String(tag) => Some(tag.clone()),
        Value::Object(map) => map.keys().next().cloned(),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// The provider states `ContextOverflow` on an accepted response.
    Overflow,
    /// The provider merely fails; lash's `is_context_overflow_text` classifier
    /// is what turns the failure text into a `ContextOverflow` terminal reason.
    ClassifiedOverflow,
    ProviderError,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Rlm,
    Standard,
}

struct Harness {
    core: lash::LashCore,
    provider_calls: Arc<AtomicUsize>,
    tool_bytes: Arc<AtomicUsize>,
    _deployment: lash_restate_postgres_workers_e2e::local_restate::LocalDeployment,
    _scratch: tempfile::TempDir,
}

impl Harness {
    /// A core on lash-restate's engine over a scratch SQLite store set, its
    /// endpoint served and registered with the local server: each arm is a
    /// deployment of its own, so its scripted provider is the one the server
    /// drives its turns with.
    async fn new(script: Script, protocol: Protocol) -> Result<Self> {
        let restate = lash_restate_postgres_workers_e2e::local_restate::LocalRestate::from_env()?;
        let scratch = tempfile::tempdir().context("scratch dir for the SQLite store set")?;
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let tool_bytes = Arc::new(AtomicUsize::new(0));

        let stores = lash_sqlite_store::SqliteStoreSet::open(scratch.path().join("sessions"))
            .await
            .context("open the SQLite store set")?;
        let engine = restate.engine(Arc::new(stores));
        let backend = lash::Backend::new(engine.clone());
        let builder = match protocol {
            Protocol::Rlm => {
                let rlm = lash_protocol_rlm::RlmProtocolPluginFactory::new(
                    lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                        .channel(lash_protocol_rlm::RlmChannel::Cell)
                        .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                            1_000_000,
                        ))
                        .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                        .build(),
                    &backend,
                );
                lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, rlm)
            }
            Protocol::Standard => {
                lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
            }
        };
        let builder = builder
            .provider(scripted_provider(
                script,
                protocol,
                Arc::clone(&provider_calls),
            ))
            .model(
                lash::ModelSpec::builder("context-overflow-recovery-mock")
                    .context_window_tokens(200_000)
                    .build()
                    .map_err(anyhow::Error::msg)?,
            )
            .commit_budget(lash::CommitBudget::bounded(4 * 1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .trace_jsonl_path(
                std::env::var_os("LASH_CONTEXT_OVERFLOW_TRACE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/dev/null")),
            )
            .plugin(Arc::new(OverflowPluginFactory {
                tool_bytes: Arc::clone(&tool_bytes),
                protocol,
            }));
        let builder = if protocol == Protocol::Standard {
            builder.plugin(Arc::new(
                lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
            ))
        } else {
            builder
        };
        let core = builder
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "context-overflow-recovery",
                format!("context-overflow-recovery:{}", std::process::id()),
            ))
            .context("build the context-overflow-recovery core")?;
        let worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .context("the core's process worker config")?,
        )
        .context("build the process worker")?;
        let deployment = restate
            .serve(&engine, engine.endpoint_builder(worker).build())
            .await?;

        Ok(Self {
            core,
            provider_calls,
            tool_bytes,
            _deployment: deployment,
            _scratch: scratch,
        })
    }

    /// Open `session_id`, creating it on first use: the recovery scenario
    /// reaches each session the same way before and after its restart, so it
    /// means create-or-use (FIG-4112).
    async fn open(&self, session_id: &SessionId) -> Result<lash::LashSession> {
        match self
            .core
            .session(session_id)
            .create(lash::SessionCreation::default())
            .await
        {
            Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("create session `{session_id}`"));
            }
        }
        self.core
            .session(session_id)
            .open()
            .await
            .with_context(|| format!("open session `{session_id}`"))
    }

    fn provider_calls(&self) -> usize {
        self.provider_calls.load(Ordering::SeqCst)
    }

    fn served_tool_bytes(&self) -> usize {
        self.tool_bytes.load(Ordering::SeqCst)
    }
}

/// The RLM language this harness serves, and the single source every layer
/// reads it from: the cell delimiters below, the `dialect` field on every
/// checkpoint, and — through that field — the artifact directory the companion
/// script names for the row. TypeScript is the only RLM language (ADR 0096);
/// when that stops being true this constant is what a second row changes,
/// and nothing downstream repeats the literal (FIG-3169).
pub(crate) const SERVED_DIALECT: &str = "typescript";

/// One cell, in the served dialect.
fn cell(body: &str) -> String {
    format!("<{SERVED_DIALECT}>\n{body}\n</{SERVED_DIALECT}>")
}

/// The cell that pulls the oversized tool result into this turn's context and
/// does *not* finish, so the protocol asks the provider again with the
/// oversized result in the request. That second request is the one that
/// overflows.
fn oversized_call_cell() -> String {
    cell(&format!(
        "const report = await tools.{OVERSIZED_TOOL}({{}});"
    ))
}

fn scripted_provider(
    script: Script,
    protocol: Protocol,
    calls: Arc<AtomicUsize>,
) -> lash::provider::ProviderHandle {
    lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
        .kind("context-overflow-recovery")
        .complete(move |request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let is_recovery_summary = protocol == Protocol::Standard && request.messages.iter().any(|message| {
                message.blocks.iter().any(|block| match block {
                    lash::provider::LlmContentBlock::Text { text, .. } =>
                        text.contains("Recover a task whose turn stopped because the provider refused"),
                    _ => false,
                })
            });
            async move {
                if is_recovery_summary {
                    return Ok(text_response(
                        "Recovery summary: the oversized report was requested; its body was elided, and the verdict remains to be stated.".to_string(),
                    ));
                }
                Ok(match (script, call) {
                    // Turn 1, call 1: reach for the oversized report.
                    (_, 0) => match protocol {
                        Protocol::Rlm => text_response(oversized_call_cell()),
                        Protocol::Standard => tool_call_response(),
                    },
                    // Turn 1, call 2: the request now carries the oversized
                    // result and the model refuses it as too large.
                    (Script::Overflow, 1) => terminal_response(
                        lash_core::LlmTerminalReason::ContextOverflow,
                        "prompt is too long: 512000 tokens > 200000 maximum",
                    ),
                    // The classifier arm: no structured terminal reason at all,
                    // just the failure text a real provider returns. Lash's own
                    // `is_context_overflow_text` has to name this one.
                    (Script::ClassifiedOverflow, 1) => {
                        return Err(lash::provider::LlmTransportError::new(
                            "This model's maximum context length is 200000 tokens, \
                             however you requested 512844 tokens",
                        ));
                    }
                    // The control arm: the same shape of failure, classified
                    // as an ordinary provider error.
                    (Script::ProviderError, 1) => terminal_response(
                        lash_core::LlmTerminalReason::ProviderError,
                        "upstream returned 500",
                    ),
                    // Turn 2: the session continues.
                    (_, _) => match protocol {
                        Protocol::Rlm => text_response(lash_restate_postgres_workers_e2e::scripted_finish_cell(
                            "\"the report checks out\"",
                        )),
                        Protocol::Standard => text_response("the report checks out".to_string()),
                    },
                })
            }
        })
        .build()
        .into_handle()
}

fn tool_call_response() -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        parts: vec![lash_core::LlmOutputPart::ToolCall {
            call_id: "oversized-report-call".to_string(),
            tool_name: OVERSIZED_TOOL.to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..lash::provider::LlmResponse::default()
    }
}

fn text_response(text: String) -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        parts: vec![lash_core::LlmOutputPart::Text {
            text,
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..lash::provider::LlmResponse::default()
    }
}

fn terminal_response(
    reason: lash_core::LlmTerminalReason,
    diagnostic: &str,
) -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        terminal_reason: reason,
        terminal_diagnostic: Some(diagnostic.to_string()),
        response_metadata: Default::default(),
        ..lash::provider::LlmResponse::default()
    }
}

struct OverflowPluginFactory {
    tool_bytes: Arc<AtomicUsize>,
    protocol: Protocol,
}

impl lash::plugins::PluginFactory for OverflowPluginFactory {
    fn id(&self) -> &'static str {
        "context-overflow-recovery"
    }

    fn build(
        &self,
        _ctx: &PluginSessionContext,
    ) -> Result<Arc<dyn SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(OverflowPlugin {
            tool_bytes: Arc::clone(&self.tool_bytes),
            protocol: self.protocol,
        }))
    }
}

struct OverflowPlugin {
    tool_bytes: Arc<AtomicUsize>,
    protocol: Protocol,
}

impl SessionPlugin for OverflowPlugin {
    fn id(&self) -> &'static str {
        "context-overflow-recovery"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), lash::plugins::PluginError> {
        if self.protocol == Protocol::Rlm {
            reg.context().compact(100, Arc::new(ReportCompactor));
        }
        reg.tools()
            .provider(oversized_tool_provider(Arc::clone(&self.tool_bytes)))
            .map_err(|err| lash::plugins::PluginError::Session(err.to_string()))?;
        Ok(())
    }
}

/// The host's compaction policy for this fixture: replace the frame with one
/// short summary. Lash owns none of this choice.
struct ReportCompactor;

#[async_trait]
impl lash_core::facade_support::ContextCompactor for ReportCompactor {
    fn id(&self) -> &'static str {
        "context_overflow_recovery.compactor"
    }

    async fn compact(
        &self,
        _ctx: &lash_core::facade_support::CompactionContext<'_>,
    ) -> std::result::Result<
        Option<lash_core::facade_support::ContextCompaction>,
        lash_core::facade_support::ContextError,
    > {
        Ok(Some(lash_core::facade_support::ContextCompaction::new(
            vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::MessageRole::Assistant,
                    "Compaction summary: the oversized report was requested and its body dropped.",
                )
                .with_origin(lash_core::MessageOrigin::Plugin {
                    plugin_id: "context_overflow_recovery".to_string(),
                    transient: false,
                }),
            )],
        )))
    }
}

fn oversized_tool_provider(tool_bytes: Arc<AtomicUsize>) -> Arc<dyn ToolProvider> {
    Arc::new(StaticToolProvider::new(
        vec![
            ToolDefinition::raw(
                "tool:oversized_report",
                OVERSIZED_TOOL,
                "Return a deterministic report far larger than the prompt budget anticipated.",
                json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                json!({
                    "type": "object",
                    "properties": { "report": { "type": "string" } },
                    "required": ["report"],
                    "additionalProperties": false
                }),
            )
            .with_tool_binding(ToolBinding::new(["tools"], OVERSIZED_TOOL)),
        ],
        OversizedTools { tool_bytes },
    )) as Arc<dyn ToolProvider>
}

#[derive(Clone)]
struct OversizedTools {
    tool_bytes: Arc<AtomicUsize>,
}

#[async_trait]
impl StaticToolExecute for OversizedTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            let report = "lash-context-overflow-recovery-report "
                .repeat(OVERSIZED_BYTES / "lash-context-overflow-recovery-report ".len());
            self.tool_bytes.store(report.len(), Ordering::SeqCst);
            let _ = call;
            ToolOutcome::ok(json!({ "report": report }))
        })
        .await
        .into()
    }
}
