//! The deployment's compaction sessions: a [`TurnScript::Compaction`]
//! session runs behind the lash facade, on the production session services,
//! the standard protocol and the standard compaction plugin. The host
//! appends a conversation to it, and its `CompactContext` command then
//! summarizes it under the command's run: an owned call admitted under
//! `completion.start` (ADR 0133 §8, FIG-5259). Only the model is the
//! simulator's.
//!
//! The model's builder lowers every request to a body of its own
//! generation, so a summary body lowered twice is told apart from one
//! lowered once. Each summary lowering and send is noted in the world.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse};
use lash_sansio::SessionId;

use super::world::World;

/// The model every compaction session names.
const MODEL: &str = "lash-sim-compaction-model";
/// What a summary lowering notes: `{LOWERED} {session} :: {body}`.
pub const LOWERED: &str = "compaction.lowered";
/// What a summary send notes: `{SENT} {session} :: {body}`.
pub const SENT: &str = "compaction.sent";
/// The summary the model answers.
pub const SUMMARY: &str = "the turn was answered";

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
    core.session(session.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await
        .map(drop)
        .map_err(|error| format!("create the compaction session: {error}"))
}

fn metadata() -> Result<lash_core::LlmProfileMetadata, String> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| error.to_string())
}

/// Whether `request` is the compaction's summary request: it ends asking
/// for the summary of the conversation above.
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
                world.note(format!("{LOWERED} {} :: {body}", request.scope.session_id));
            }
            body
        })
        .send(move |request: LlmRequest, body| {
            let world = world.clone();
            async move {
                let summary = summarizes(&request);
                if summary && let Some(world) = world.upgrade() {
                    world.note(format!(
                        "{SENT} {} :: {}",
                        request.scope.session_id, body.body
                    ));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: SUMMARY.to_owned(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}
