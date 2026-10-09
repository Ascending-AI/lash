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
//! body states everything the model answers from, and a send is handed the
//! body and its response context, never a request (FIG-5479).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use lash_core::LlmTerminalReason;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, ResponseContext,
};
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
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(Arc::downgrade(world)), metadata()?)
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lash-sim-deployment"),
            lash::persistence::LeaseIncarnationId::new("lash-sim-boot"),
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
            lash::plugins::SessionToolAccess::ambient(),
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
        .cache_retention(lash_core::provider::CacheRetention::Short)
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

const KIND: &str = "lash-sim-compaction";

#[expect(
    clippy::expect_used,
    reason = "the scripted body is a serialized JSON value, which is a valid literal template"
)]
fn model(world: Weak<World>) -> ProviderHandle {
    let generations = Arc::new(AtomicU64::new(0));
    let lowering = world.clone();
    lash_core::testing::TestProvider::builder()
        .kind(KIND)
        .template(move |request: &LlmRequest| {
            let generation = generations.fetch_add(1, Ordering::SeqCst) + 1;
            let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
            // Everything the model answers from is in the body: whether the
            // call is the summary, and for a pressure turn whether it
            // carries the second input, the summary and the first input.
            let body = serde_json::json!({
                "builder": generation,
                "summary": summarizes(request),
                "next": rendered.contains(NEXT),
                "seeded": rendered.contains(SUMMARY),
                "ask": rendered.contains(ASK),
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
            lash_core::RecordedRequestTemplate::literal(
                lash_core::ProviderRouteIdentity::new(KIND, KIND, request.model.wire_model()),
                request.stream_events.is_some(),
                None,
                body,
            )
            .expect("the scripted body is JSON")
        })
        .answer(move |context: ResponseContext, body: String| {
            let world = world.clone();
            async move {
                let sent = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
                let session = context.scope.session_id().cloned();
                let named = session
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let summary = sent["summary"] == true;
                if summary && let Some(world) = world.upgrade() {
                    world.note(format!("{SENT} {named} :: {body}"));
                }
                let answer = |text: &str| LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_owned(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                };
                if summary
                    || session.as_ref().and_then(TurnScript::of) != Some(TurnScript::Pressure)
                {
                    return Ok(answer(SUMMARY));
                }
                // A pressure session's turn: the second input's is answered,
                // the first's overflows.
                if sent["next"] != true {
                    return Ok(LlmResponse {
                        terminal_reason: LlmTerminalReason::ContextOverflow,
                        terminal_diagnostic: Some("context window exceeded".to_owned()),
                        ..LlmResponse::default()
                    });
                }
                if let Some(world) = world.upgrade() {
                    world.note(format!(
                        "{RECOVERED} {named} :: summary={} ask={}",
                        sent["seeded"] == true,
                        sent["ask"] == true
                    ));
                }
                Ok(answer(FINAL))
            }
        })
        .build()
        .into_handle()
}
