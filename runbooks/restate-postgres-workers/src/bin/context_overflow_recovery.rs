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
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolCall, ToolDefinition,
    ToolDefinitionBindingExt, ToolOutcome, ToolProvider,
};
use serde_json::{Value, json};

/// The oversized tool result. Large enough that no reader mistakes it for an
/// ordinary payload, small enough that the harness stays fast.
const OVERSIZED_BYTES: usize = 512 * 1024;

/// The tool the scripted cell calls to pull the oversized result into the
/// turn's context.
const OVERSIZED_TOOL: &str = "oversized_report";

#[tokio::main]
async fn main() -> Result<()> {
    let dialect = lash_restate_postgres_workers_e2e::runbook_rlm_dialect()
        .context("LASH_RUNBOOK_DIALECT names a registered dialect")?;
    let run_id = format!(
        "{:x}",
        lash_restate_postgres_workers_e2e::current_epoch_ms()
    );

    // Arm 1: the provider states the terminal reason itself.
    let overflow = overflow_and_recovery(
        dialect,
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
        dialect,
        &run_id,
        Script::ClassifiedOverflow,
        "classified_overflow_recovered",
        "classified",
    )
    .await?;
    emit(&classified);
    let control = provider_error_control(dialect, &run_id).await?;
    emit(&control);
    Ok(())
}

fn emit(checkpoint: &Value) {
    println!("{checkpoint}");
}

/// Phases 1-3: overflow, host recovery, continued session.
///
/// Runs for either overflow arm. The two differ only in how the provider states
/// the overflow -- a terminal reason on an accepted response, or a failure whose
/// text lash classifies itself -- and the point of running both is that the
/// outcome, the recovery and the continued session must be identical either way.
async fn overflow_and_recovery(
    dialect: lash::rlm::RlmDialect,
    run_id: &str,
    script: Script,
    checkpoint: &str,
    session_tag: &str,
) -> Result<Value> {
    let harness = Harness::new(dialect, script)?;
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

    // The host's own policy decision, taken on the outcome the kernel stated.
    let compacted = if overflow.result.is_context_overflow() {
        session
            .admin()
            .state()
            .compact_context(
                Some("keep the request and the report's verdict, drop the body".to_string()),
                harness
                    .core
                    .effect_host()
                    .scoped_static(lash::runtime::ExecutionScope::runtime_operation(format!(
                        "context-overflow-recovery:{run_id}"
                    )))
                    .map_err(|err| anyhow!("{err}"))?
                    .ok_or_else(|| anyhow!("effect host supplies no owned runtime scope"))?,
            )
            .await
            .map_err(|err| anyhow!("{err}"))
            .context("host recovery: compact the session context")?
    } else {
        false
    };

    let messages_after_compaction = session.read_view().messages().len();

    // The same session, not a new one: the claim is that the session continues.
    let continued = session
        .turn(lash::TurnInput::text("now give me the verdict"))
        .run()
        .await
        .map_err(|err| anyhow!("{err}"))
        .context("the post-recovery turn")?;

    Ok(json!({
        "checkpoint": checkpoint,
        "dialect": dialect.language_id(),
        "session_id": session_id.as_str(),
        "oversized_tool_result_bytes": tool_bytes,
        "provider_calls": harness.provider_calls(),
        "overflow_outcome": serde_json::to_value(&overflow.result.outcome)
            .context("serialize the overflow outcome")?,
        "overflow_stop": overflow_stop,
        "overflow_is_context_overflow": overflow.result.is_context_overflow(),
        "overflow_is_success": overflow.result.is_success(),
        "compacted": compacted,
        "messages_after_compaction": messages_after_compaction,
        "continued_stop": stop_tag(&continued.result.outcome)?,
        "continued_is_success": continued.result.is_success(),
        "continued_is_context_overflow": continued.result.is_context_overflow(),
        "continued_assistant_message": continued.result.assistant_message(),
        "continued_final_value": continued.result.final_value(),
    }))
}

/// The control: a plain provider error on the same harness must not produce
/// the overflow outcome.
async fn provider_error_control(dialect: lash::rlm::RlmDialect, run_id: &str) -> Result<Value> {
    let harness = Harness::new(dialect, Script::ProviderError)?;
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
        "dialect": dialect.language_id(),
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
    dialect: lash::rlm::RlmDialect,
    provider_calls: Arc<AtomicUsize>,
    tool_bytes: Arc<AtomicUsize>,
    _scratch: tempfile::TempDir,
    _attachments: tempfile::TempDir,
}

impl Harness {
    fn new(dialect: lash::rlm::RlmDialect, script: Script) -> Result<Self> {
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
            .provider(scripted_provider(
                dialect,
                script,
                Arc::clone(&provider_calls),
            ))
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
            dialect,
            provider_calls,
            tool_bytes,
            _scratch: scratch,
            _attachments: attachments,
        })
    }

    async fn open(&self, session_id: &SessionId) -> Result<lash::LashSession> {
        self.core
            .session(session_id)
            .plugin_option(
                lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash::rlm::RlmCreateExtras {
                    dialect: Some(self.dialect),
                    ..lash::rlm::RlmCreateExtras::default()
                },
            )
            .context("state the row's dialect")?
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

/// One cell, spelled for `dialect`.
fn cell(dialect: lash::rlm::RlmDialect, lashlang: &str, typescript: &str) -> String {
    let body = match dialect {
        lash::rlm::RlmDialect::Lashlang => lashlang,
        lash::rlm::RlmDialect::Typescript => typescript,
    };
    let tag = dialect.language_id();
    format!("<{tag}>\n{body}\n</{tag}>")
}

/// The cell that pulls the oversized tool result into this turn's context and
/// does *not* finish, so the protocol asks the provider again with the
/// oversized result in the request. That second request is the one that
/// overflows.
fn oversized_call_cell(dialect: lash::rlm::RlmDialect) -> String {
    cell(
        dialect,
        &format!("report = await tools.{OVERSIZED_TOOL}({{}})?"),
        &format!("const report = await tools.{OVERSIZED_TOOL}({{}});"),
    )
}

fn scripted_provider(
    dialect: lash::rlm::RlmDialect,
    script: Script,
    calls: Arc<AtomicUsize>,
) -> lash::provider::ProviderHandle {
    lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
        .kind("context-overflow-recovery")
        .complete(move |_request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(match (script, call) {
                    // Turn 1, call 1: reach for the oversized report.
                    (_, 0) => text_response(oversized_call_cell(dialect)),
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
                            dialect,
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
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let report = "lash-context-overflow-recovery-report "
            .repeat(OVERSIZED_BYTES / "lash-context-overflow-recovery-report ".len());
        self.tool_bytes.store(report.len(), Ordering::SeqCst);
        let _ = call;
        ToolOutcome::ok(json!({ "report": report }))
    }
}
