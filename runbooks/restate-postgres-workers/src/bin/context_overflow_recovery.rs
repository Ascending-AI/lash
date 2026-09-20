//! Context-overflow recovery harness (`runbooks/context-overflow-recovery`).
//!
//! One process, one SQLite scratch store, no container and no token. Driven by
//! `scripts/context-overflow-recovery-e2e.sh`, which owns the artifact
//! directory and the exact gates.
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
//!    (`TurnReport::is_context_overflow`), decides its own policy, and acts on
//!    the existing seam: `compact_context` on the session admin surface.
//! 4. The same session runs another turn and finishes. Nothing was restarted.
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
use lash::persistence::SessionStoreFactory;
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

const ROLLING_HISTORY_PLUGIN_ID: &str = "rolling_history";
const OVERFLOW_RECOVERY_MARKER_TITLE: &str =
    "Rolling-history context-overflow recovery marker (pending):";
const OVERFLOW_RECOVERY_COMPLETED_TITLE: &str =
    "Rolling-history context-overflow recovery completed:";

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
    // Arm 2: the provider only *fails*, and lash's own classifier
    // (`is_context_overflow_text`) is what names the overflow. This is the path
    // a real provider takes, and the one that used to collapse into
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
    Ok(())
}

fn emit(checkpoint: &Value) {
    println!("{checkpoint}");
}

/// Every durable message node the session holds, across frames and branches.
fn durable_messages(view: &lash_core::SessionReadView) -> Vec<lash_core::Message> {
    fn walk(nodes: &[lash::messages::SessionMessageTreeNode], out: &mut Vec<lash_core::Message>) {
        for node in nodes {
            out.push(node.message.clone());
            walk(&node.children, out);
        }
    }
    let mut out = Vec::new();
    walk(&view.message_tree(), &mut out);
    out
}

/// Phases 1-3: overflow, host recovery, continued session.
///
/// Runs for either overflow arm. The two differ only in how the provider states
/// the overflow -- a terminal reason on an accepted response, or a failure whose
/// text lash classifies itself -- and the point of running both is that the
/// outcome, the recovery and the continued session must be identical either way.
async fn overflow_and_recovery(
    run_id: &str,
    script: Script,
    checkpoint: &str,
    session_tag: &str,
) -> Result<Value> {
    let harness = Harness::new(script)?;
    let session_id = SessionId::from(format!("context-overflow-{session_tag}-{run_id}"));
    let session = harness.open(&session_id).await?;

    let overflow = session
        .turn(lash::TurnInput::text("summarize the attached report"))
        .run()
        .await;
    let overflow = match overflow {
        Ok(output) => output,
        Err(error) => bail!("the overflow turn did not settle into a report: {error}"),
    };
    let overflow_stop = stop_tag(&overflow.result.outcome)?;
    let tool_bytes = harness.served_tool_bytes();

    // No host compaction call: the plugin owns the recovery now. The host
    // only observes the durable recovery state the plugin appended, and the
    // next turn below re-derives and executes it.
    let overflow_history = session.read_view().messages().to_vec();
    let plugin_recovery_pending = overflow_history.iter().any(|message| {
        let origin = message.origin.as_ref().map(|origin| match origin {
            lash_core::MessageOrigin::Plugin { plugin_id, .. } => plugin_id.clone(),
            _ => String::new(),
        });
        origin.as_deref() == Some(ROLLING_HISTORY_PLUGIN_ID)
            && message
                .parts
                .iter()
                .any(|part| part.content().starts_with(OVERFLOW_RECOVERY_MARKER_TITLE))
    });
    if overflow.result.is_context_overflow() && !plugin_recovery_pending {
        bail!("the plugin silently swallowed the overflow trigger");
    }

    // FIG-3107: recovery completes through a durable agent-frame switch. The
    // frame the overflow turn ran in is the baseline the recovery frame
    // leaves behind.
    let pre_recovery = session.read_view().to_snapshot();
    let recovery_frame_before_id = pre_recovery.current_frame_node_id.clone();

    // The same session, not a new one: the claim is that the session continues.
    let continued = session
        .turn(lash::TurnInput::text("now give me the verdict"))
        .run()
        .await
        .map_err(|err| anyhow!("{err}"))
        .context("the post-recovery turn")?;
    let continued_history = session.read_view().messages().to_vec();
    let continued_history_len = continued_history.len();
    // The recovery's terminal record is durable in the frame the recovery left
    // behind, so the durable read is the whole message tree, not the active
    // frame's projection: after the switch the session is resident in the
    // recovery frame and reads only that frame's messages.
    let overflow_history_after_turn = durable_messages(&session.read_view());

    // The recovery frame exists and the session is resident in it after
    // recovery: the latest frame record carries the compaction reason and the
    // session's current frame moved off the overflow turn's frame.
    let post_recovery = session.read_view().to_snapshot();
    let recovery_frame = post_recovery.agent_frames.last().map(|frame| {
        (
            frame.reason.as_str().to_string(),
            frame.frame_node_id.clone(),
        )
    });
    let recovery_frame_id = post_recovery.current_frame_node_id.clone();

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
        "plugin_recovery_pending": plugin_recovery_pending,
        "continued_stop": stop_tag(&continued.result.outcome)?,
        // Plugin-owned recovery evidence, re-derived from the durable history
        // after the continued turn: the plugin appended its summary and the
        // completed record, and the original history stays inspectable.
        "plugin_recovery_completed": overflow_history_after_turn.iter().any(|message| {
            message.parts.iter().any(|part| {
                part.content()
                    .starts_with(OVERFLOW_RECOVERY_COMPLETED_TITLE)
            })
        }),
        "plugin_recovery_summary_chars": overflow_history_after_turn
            .iter()
            .filter_map(|message| {
                message.parts.iter().find_map(|part| {
                    part.content()
                        .strip_prefix("Compaction summary:")
                        .map(|rest| rest.trim().len())
                })
            })
            .next()
            .unwrap_or(0),
        "history_messages_after_recovery": continued_history_len,
        "continued_is_success": continued.result.is_success(),
        "continued_is_context_overflow": continued.result.is_context_overflow(),
        "continued_assistant_message": continued.result.assistant_message(),
        "continued_final_value": continued.result.final_value(),
        // FIG-3107 frame evidence: the recovery frame exists (latest frame
        // record with the compaction reason) and the continued session is
        // resident in it — the current frame moved off the overflow turn's
        // frame.
        "recovery_frame_reason": recovery_frame
            .as_ref()
            .map(|(reason, _)| reason.clone()),
        "recovery_frame_id": recovery_frame_id,
        "recovery_frame_moved": recovery_frame_before_id != recovery_frame_id,
    }))
}

/// The control: a plain provider error on the same harness must not produce
/// the overflow outcome.
async fn provider_error_control(run_id: &str) -> Result<Value> {
    let harness = Harness::new(Script::ProviderError)?;
    let session_id = SessionId::from(format!("context-overflow-control-{run_id}"));
    let session = harness.open(&session_id).await?;

    let failed = session
        .turn(lash::TurnInput::text("summarize the attached report"))
        .run()
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

struct Harness {
    core: lash::LashCore,
    provider_calls: Arc<AtomicUsize>,
    tool_bytes: Arc<AtomicUsize>,
    _scratch: tempfile::TempDir,
    _attachments: tempfile::TempDir,
}

impl Harness {
    fn new(script: Script) -> Result<Self> {
        let scratch = tempfile::tempdir().context("scratch dir for the SQLite backend")?;
        let attachments = tempfile::tempdir().context("attachment dir")?;
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let tool_bytes = Arc::new(AtomicUsize::new(0));

        let store_factory: Arc<dyn SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(scratch.path().join("sessions")),
        );
        let rlm = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::persistence::InMemoryLashlangArtifactStore::default()),
        );

        let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, rlm)
            .with_native_queued_work()
            .provider(scripted_provider(script, Arc::clone(&provider_calls)))
            .model(
                lash::ModelSpec::builder("context-overflow-recovery-mock")
                    .context_window_tokens(200_000)
                    .build()
                    .map_err(anyhow::Error::msg)?,
            )
            .store_factory(store_factory)
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                attachments.path().to_path_buf(),
            )))
            .commit_budget(lash::CommitBudget::bounded(4 * 1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::default(),
            ))
            .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
            .trace_jsonl_path(
                std::env::var_os("LASH_CONTEXT_OVERFLOW_TRACE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/dev/null")),
            )
            .plugin(Arc::new(
                lash_plugin_rolling_history::RollingHistoryPluginFactory::default(),
            ))
            .plugin(Arc::new(OverflowPluginFactory {
                tool_bytes: Arc::clone(&tool_bytes),
            }))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "context-overflow-recovery",
                format!("context-overflow-recovery:{}", std::process::id()),
            ))
            .context("build the context-overflow-recovery core")?;

        Ok(Self {
            core,
            provider_calls,
            tool_bytes,
            _scratch: scratch,
            _attachments: attachments,
        })
    }

    async fn open(&self, session_id: &SessionId) -> Result<lash::LashSession> {
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

fn scripted_provider(script: Script, calls: Arc<AtomicUsize>) -> lash::provider::ProviderHandle {
    lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
        .kind("context-overflow-recovery")
        .complete(move |request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            // The plugin-owned recovery branch: the out-of-band summarizer
            // carries the standard compaction prompt. It is not a cell, so it
            // must be answered with plain assistant text.
            // The compaction prompt can sit behind the protocol's own trailing
            // messages, so the whole request is scanned for it rather than its
            // last message alone.
            let is_recovery_summarizer = request.messages.iter().any(|message| {
                message.blocks.iter().any(|block| match block {
                    lash::provider::LlmContentBlock::Text { text, .. } => {
                        text.contains("Provide a detailed summary of the conversation above")
                    }
                    _ => false,
                })
            });
            async move {
                if is_recovery_summarizer {
                    // The out-of-band summarizer runs in the plugin's compaction
                    // child session; the scripted answer is a terminal finish
                    // cell whose value becomes the recovered summary.
                    return Ok(text_response(
                        lash_restate_postgres_workers_e2e::scripted_finish_cell(
                            "\"Recovery summary: the user asked for the oversized report's \
                             verdict; the report body was elided and still needs stating.\"",
                        ),
                    ));
                }
                Ok(match (script, call) {
                    // Turn 1, call 1: reach for the oversized report.
                    (_, 0) => text_response(oversized_call_cell()),
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
                    // Turn 2, after the host compacted: the session continues.
                    (_, _) => {
                        text_response(lash_restate_postgres_workers_e2e::scripted_finish_cell(
                            "\"the report checks out\"",
                        ))
                    }
                })
            }
        })
        .build()
        .into_handle()
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
        }))
    }
}

struct OverflowPlugin {
    tool_bytes: Arc<AtomicUsize>,
}

impl SessionPlugin for OverflowPlugin {
    fn id(&self) -> &'static str {
        "context-overflow-recovery"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), lash::plugins::PluginError> {
        reg.context().compact(100, Arc::new(ReportCompactor));
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
