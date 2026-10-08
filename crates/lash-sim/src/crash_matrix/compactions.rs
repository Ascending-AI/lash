//! The deployment's compaction sessions: a [`TurnScript::Compaction`]
//! session runs behind the lash facade, on the production session services,
//! the standard protocol and the standard compaction plugin. The host
//! appends a conversation to it, and its `CompactContext` command then
//! summarizes it under the command's run: an owned call admitted under
//! `completion.start` (ADR 0133 §8, FIG-5259). A [`TurnScript::Pressure`]
//! session runs on the same core: the host sends it two inputs, the model
//! overflows on the first turn, and the plugin's context-pressure hook
//! summarizes the history the same way while it prepares the second, which
//! then runs in the recovery frame the summary seeds (FIG-5355). Only the
//! model is the simulator's.
//!
//! The model's builder lowers every request to a body of its own
//! generation, so a summary body lowered twice is told apart from one
//! lowered once. Each summary lowering and send is noted in the world. The
//! model answers a send from the body it is sent: the summary instruction is
//! a prompt section (FIG-5432), composed into the request once at admission,
//! so a resend's request is the caller's own and only its admitted body
//! says it is the summary.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use lash_core::LlmTerminalReason;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse};
use lash_sansio::{SessionId, TurnId};

use super::services::TurnScript;
use super::world::World;

/// The model every compaction session names.
const MODEL: &str = "lash-sim-compaction-model";
/// What a summary lowering notes: `{LOWERED} {session} :: {body}`.
pub const LOWERED: &str = "compaction.lowered";
/// What a summary send notes: `{SENT} {session} :: {body}`.
pub const SENT: &str = "compaction.sent";
/// The summary the model answers.
pub const SUMMARY: &str = "the turn was answered";
/// A pressure session's first input, whose turn overflows.
pub const ASK: &str = "read the whole repository";
/// A pressure session's second input, which its recovered turn answers.
pub const NEXT: &str = "now answer briefly";
/// What a pressure session's recovered turn answers.
pub const FINAL: &str = "a brief answer";
/// What a pressure session's recovered request notes:
/// `{RECOVERED} {session} :: summary={bool} ask={bool}`, whether it carried
/// the summary and the first input.
pub const RECOVERED: &str = "pressure.recovered";

/// The run's core for its compaction sessions, built once over `world`'s
/// backend.
///
/// # Errors
///
/// The run has no backend yet, or the core does not build.
pub fn compaction_core(world: &Arc<World>) -> Result<lash::LashCore, String> {
    if let Some(core) = world.compaction_core().get() {
        return Ok(core.clone());
    }
    let built = lash::LashCore::standard_builder(world.backend()?)
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .serve_test_llm_profile(model(Arc::downgrade(world)), metadata()?)
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lash-sim-deployment",
            "lash-sim-boot",
        ))
        .map_err(|error| format!("build the compaction core: {error}"))?;
    Ok(world.compaction_core().get_or_init(|| built).clone())
}

/// Create `session` through `core`.
///
/// # Errors
///
/// The facade refused.
pub async fn create(core: &lash::LashCore, session: &SessionId) -> Result<(), String> {
    created(core, session).await.map(drop)
}

/// Create the pressure session `session` through `core` and send it its
/// inputs: [`ASK`] as `first`, then [`NEXT`] as `second`.
///
/// # Errors
///
/// The facade refused.
pub async fn send_pressure(
    core: &lash::LashCore,
    session: &SessionId,
    first: &TurnId,
    second: &TurnId,
) -> Result<(), String> {
    let handle = created(core, session).await?;
    for (input, run) in [(ASK, first), (NEXT, second)] {
        handle
            .send(lash::TurnInput::text(input))
            .id(run.clone())
            .await
            .map(drop)
            .map_err(|error| format!("send {input}: {error}"))?;
    }
    Ok(())
}

async fn created(
    core: &lash::LashCore,
    session: &SessionId,
) -> Result<lash::DurableSession, String> {
    core.session(session.clone())
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(8),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .map_err(|error| format!("create the compaction session: {error}"))
}

fn metadata() -> Result<lash_core::LlmProfileMetadata, String> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| error.to_string())
}

/// Whether `request`, as composed at admission, is the compaction's summary
/// request: it ends asking for the summary of the conversation above.
fn summarizes(request: &LlmRequest) -> bool {
    request
        .messages
        .last()
        .into_iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| {
            matches!(block, LlmContentBlock::Text { text, .. } if text.contains("detailed summary"))
        })
}

fn model(world: Weak<World>) -> ProviderHandle {
    let generations = Arc::new(AtomicU64::new(0));
    let lowering = world.clone();
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-compaction")
        .lower(move |request: &LlmRequest| {
            let generation = generations.fetch_add(1, Ordering::SeqCst) + 1;
            let body = serde_json::json!({
                "builder": generation,
                "summary": summarizes(request),
            })
            .to_string();
            if summarizes(request)
                && let Some(world) = lowering.upgrade()
            {
                world.note(format!(
                    "{LOWERED} {} :: {body}",
                    request
                        .session_id()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                ));
            }
            body
        })
        .send(move |request: LlmRequest, body| {
            let world = world.clone();
            async move {
                let summary = serde_json::from_str::<serde_json::Value>(&body.body)
                    .is_ok_and(|body| body["summary"] == true);
                if summary && let Some(world) = world.upgrade() {
                    world.note(format!(
                        "{SENT} {} :: {}",
                        request
                            .session_id()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                        body.body
                    ));
                }
                let answer = |text: &str| LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_owned(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                };
                if summary
                    || request.session_id().and_then(TurnScript::of) != Some(TurnScript::Pressure)
                {
                    return Ok(answer(SUMMARY));
                }
                // A pressure session's turn: the second input's is answered,
                // the first's overflows.
                let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
                if !rendered.contains(NEXT) {
                    return Ok(LlmResponse {
                        terminal_reason: LlmTerminalReason::ContextOverflow,
                        terminal_diagnostic: Some("context window exceeded".to_owned()),
                        ..LlmResponse::default()
                    });
                }
                if let Some(world) = world.upgrade() {
                    world.note(format!(
                        "{RECOVERED} {} :: summary={} ask={}",
                        request
                            .session_id()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                        rendered.contains(SUMMARY),
                        rendered.contains(ASK)
                    ));
                }
                Ok(answer(FINAL))
            }
        })
        .build()
        .into_handle()
}
