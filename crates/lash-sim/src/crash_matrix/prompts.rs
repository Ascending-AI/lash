//! The deployment's prompt sessions: a [`TurnScript::Prompt`] session runs
//! behind the lash facade, on the production turn driver and the standard
//! protocol, and every one of its model calls composes plugin prompt
//! sections (ADR 0133, FIG-5255). Only the model is the simulator's.
//!
//! The `prompt-memo` plugin owns the memory T, a section that renders it
//! with the count of the turn's checkpoints and whether the cut's session
//! view shows `set_t`'s outcome, and two `Repeatable` tools.
//! `set_t` sets T.
//! `speculate` proposes another T that the plugin's own result check denies,
//! so it never publishes. Its `AfterWork` checkpoint callback counts the
//! checkpoints through a reducer: a decision `model.start` commits with the
//! call after it. Its before-turn callback notes each run. The
//! `prompt-frame` plugin wraps the memo section.
//!
//! The scripted model calls `set_t` first, then `speculate`, then answers
//! [`FINAL`]. Each render, each wrapping, each before-turn callback and each
//! request the model receives, with its exact body, is noted in the world.

use std::sync::{Arc, Weak};

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::plugin::prompt::{
    PromptInput, PromptRenderError, PromptSectionSpec, PromptWrapSpec, PromptWrapTarget,
    SectionText,
};
use lash_core::prompt_sections::{
    PromptPlacement, PromptSectionId, PromptSectionKey, PromptWrapKey,
};
use lash_core::{ExecutionPolicy, ToolCall};
use lash_sansio::{SessionId, TurnId};

use super::services::{FINAL, session_id};
use super::world::World;

/// The model every prompt session names.
const MODEL: &str = "lash-sim-prompt-model";
/// The plugin that owns T, its section and its tools.
pub const MEMO: &str = "prompt-memo";
/// The plugin that wraps the memo section.
const FRAME: &str = "prompt-frame";
/// The tool that sets T.
pub const SET_T: &str = "set_t";
/// The tool whose T its plugin's result check denies.
pub const SPECULATE: &str = "speculate";
/// The T `set_t` sets.
pub const NEXT_T: &str = "two";
/// What a render notes: `{RENDERED} {session} call={call}`.
pub const RENDERED: &str = "prompt.render";
/// What the frame's wrapping notes: `{WRAPPED} {session} call={call}`.
pub const WRAPPED: &str = "prompt.wrap";
/// What the memo's before-turn callback notes: `{BEFORE_TURN} {session}`.
pub const BEFORE_TURN: &str = "prompt.before-turn";
/// What a model request notes: `{SENT} {session} call={call}
/// attempt={attempt} body={digest} :: {request_json}`, the digest that of
/// the exact body the call sends.
pub const SENT: &str = "prompt.sent";

/// The memo section, as the session's calls render it.
#[must_use]
pub fn memo_section() -> PromptSectionId {
    PromptSectionId::new(MEMO, key("current"))
}

#[expect(
    clippy::expect_used,
    reason = "the simulator's prompt keys are valid literals"
)]
fn key(key: &str) -> PromptSectionKey {
    PromptSectionKey::new(key).expect("a valid prompt key")
}

/// The prompt the memo section renders for T and the count of checkpoints,
/// through the frame's wrapper.
#[must_use]
pub fn memo_prompt(t: &str, checkpoints: u64, outcome_seen: bool) -> String {
    format!("T={t} checkpoints={checkpoints} set_t_seen={outcome_seen} (framed)")
}

/// The run's core for its prompt sessions, built once over `world`'s
/// backend.
///
/// # Errors
///
/// The run has no backend yet, or the core does not build.
pub fn prompt_core(world: &Arc<World>) -> Result<lash::LashCore, String> {
    if let Some(core) = world.prompt_core().get() {
        return Ok(core.clone());
    }
    let backend = world.backend()?;
    let built = lash::LashCore::standard_builder(backend)
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(model(Arc::downgrade(world)), metadata()?)
        .plugin(Arc::new(Memo {
            world: Arc::downgrade(world),
        }))
        .plugin(Arc::new(Frame {
            world: Arc::downgrade(world),
        }))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lash-sim-deployment",
            "lash-sim-boot",
        ))
        .map_err(|error| format!("build the prompt core: {error}"))?;
    Ok(world.prompt_core().get_or_init(|| built).clone())
}

/// Create `session` through `core` and send it the input `run` takes.
///
/// # Errors
///
/// The facade refused.
pub async fn send(core: &lash::LashCore, session: &SessionId, run: &TurnId) -> Result<(), String> {
    core.session(session.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await
        .map_err(|error| format!("create the prompt session: {error}"))?
        .send(lash::TurnInput::text("remember"))
        .id(run.clone())
        .await
        .map(drop)
        .map_err(|error| format!("send the prompt turn: {error}"))
}

fn metadata() -> Result<lash_core::LlmProfileMetadata, String> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| error.to_string())
}

/// Which of the turn's model calls `request` is, from 1: one more than the
/// tool results it carries.
fn call_of(request: &LlmRequest) -> usize {
    1 + request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
        .count()
}

/// The digest of the exact body a call sends.
fn body_digest(body: &lash_core::ProviderRequestBody) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.body.hash(&mut hasher);
    hasher.finish()
}

/// The scripted model: `set_t` on the first call, `speculate` on the
/// second, [`FINAL`] after. It notes every request it receives with the
/// digest of the body it was sent.
#[expect(
    clippy::expect_used,
    reason = "the simulator's model requests must serialize for its oracle"
)]
fn model(world: Weak<World>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("lash-sim-prompt")
        .send(move |request: LlmRequest, body| {
            let world = world.clone();
            async move {
                let call = call_of(&request);
                if let Some(world) = world.upgrade() {
                    world.note(format!(
                        "{SENT} {} call={call} attempt={} body={:016x} :: {}",
                        request
                            .session_id()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                        request.scope.attempt.unwrap_or(0),
                        body_digest(&body),
                        serde_json::to_string(&request).expect("record the received request")
                    ));
                }
                let part = |tool: &str| LlmOutputPart::ToolCall {
                    call_id: format!("{tool}-{call}"),
                    tool_name: tool.to_owned(),
                    input_json: "{}".to_owned(),
                    replay: None,
                };
                let parts = match call {
                    1 => vec![part(SET_T)],
                    2 => vec![part(SPECULATE)],
                    _ => vec![LlmOutputPart::Text {
                        text: FINAL.to_owned(),
                        response_meta: None,
                    }],
                };
                Ok(LlmResponse {
                    parts,
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// The memo plugin: T, its section, its tools, its result check and its
/// checkpoint count.
#[derive(Clone)]
struct Memo {
    /// Weak: the world holds the core that holds this plugin.
    world: Weak<World>,
}

impl lash_core::plugin::PluginDefinition for Memo {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(MEMO)
    }
}

impl lash_core::plugin::PluginFactory for Memo {
    fn id(&self) -> &'static str {
        MEMO
    }

    fn build(
        &self,
        _: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::SessionPlugin for Memo {
    fn id(&self) -> &'static str {
        MEMO
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        let world = self.world.clone();
        let before = self.world.clone();
        reg.turn().before(
            lash_core::hook_key!("note-turn"),
            Arc::new(move |context| {
                if let Some(world) = before.upgrade() {
                    world.note(format!("{BEFORE_TURN} {}", context.session_id));
                }
                Box::pin(async { Ok(lash_core::plugin::TurnContributions::default()) })
            }),
        )?;
        reg.prompt().section(
            PromptSectionSpec::new(key("current"), PromptPlacement::CurrentContext),
            Arc::new(move |input: &PromptInput<'_>| {
                if let Some(world) = world.upgrade() {
                    world.note(format!(
                        "{RENDERED} {} call={}",
                        input.call().session_id,
                        input.call().call
                    ));
                }
                let t = input.state().get_as::<String>("t")?;
                let checkpoints = input.state().get_as::<u64>("checkpoints")?.unwrap_or(0);
                // The previous round's outcome, as the cut's session view
                // shows it.
                let seen = input.session().is_some_and(|view| {
                    view.messages()
                        .iter()
                        .flat_map(|message| message.parts.iter())
                        .any(|part| {
                            part.kind() == lash_core::PartKind::ToolResult
                                && part.tool_name() == Some(SET_T)
                        })
                });
                Ok::<_, PromptRenderError>(SectionText::Text(format!(
                    "T={} checkpoints={checkpoints} set_t_seen={seen}",
                    t.as_deref().unwrap_or("none")
                )))
            }),
        )?;
        reg.state_reducer(
            "count",
            Arc::new(|reduction| {
                let current = reduction
                    .current
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                Ok(Some(serde_json::json!(current + 1)))
            }),
        )?;
        reg.turn().checkpoint(
            lash_core::hook_key!("count-checkpoints"),
            Arc::new(|context| {
                Box::pin(async move {
                    let mut contributions = lash_core::plugin::TurnContributions::default();
                    if context.checkpoint == lash_core::CheckpointKind::AfterWork {
                        contributions.state = lash_core::plugin::StateCommands::new().apply(
                            "checkpoints",
                            "count",
                            serde_json::Value::Null,
                        );
                    }
                    Ok(contributions)
                })
            }),
        )?;
        reg.tool_calls().check_result(
            lash_core::hook_key!("deny-speculation"),
            Arc::new(|input| {
                Box::pin(async move {
                    if input.context.tool_name != SPECULATE {
                        return Ok(lash_core::plugin::AfterToolContributions::default());
                    }
                    Ok(lash_core::plugin::AfterToolDecision::Deny(
                        lash_core::ToolFailure::invalid_request(
                            "speculation_denied",
                            "the memo keeps the T it has",
                        ),
                    )
                    .into())
                })
            }),
        )?;
        reg.tools().provider(tools()?)
    }
}

/// The memo's tools: `set_t` sets T, `speculate` proposes another T.
fn tools() -> Result<Arc<dyn lash_core::ToolProvider>, lash_core::PluginError> {
    let definition = |name: &str, description: &str| {
        lash_core::ToolDefinition::raw(
            name,
            name,
            description,
            serde_json::json!({ "type": "object", "additionalProperties": false }),
            serde_json::json!({ "type": "object" }),
        )
        // Repeatable: a crash before a call's outcome commits runs it again
        // at its ordinal, so every cell's T is the one the model asked for.
        .map(|definition| {
            definition
                .with_execution(std::time::Duration::from_secs(120))
                .with_execution_policy(ExecutionPolicy::repeatable(
                    std::num::NonZeroU32::MIN.saturating_add(2),
                    100,
                    1_000,
                ))
        })
        .map_err(|error| lash_core::PluginError::Registration(error.to_string()))
    };
    Ok(Arc::new(StaticToolProvider::new(
        vec![
            definition(SET_T, "Sets the memo's T.")?,
            definition(SPECULATE, "Proposes another T.")?,
        ],
        MemoTools,
    )))
}

struct MemoTools;

#[async_trait::async_trait]
impl StaticToolExecute for MemoTools {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let t = if call.name() == SET_T {
            NEXT_T
        } else {
            "speculative"
        };
        lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({ "t": t }))
                .with_state(lash_core::plugin::StateCommands::new().set("t", serde_json::json!(t))),
        )
    }
}

/// The frame plugin: a trusted wrapper over another plugin's section.
#[derive(Clone)]
struct Frame {
    /// Weak: the world holds the core that holds this plugin.
    world: Weak<World>,
}

impl lash_core::plugin::PluginDefinition for Frame {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(FRAME)
    }
}

impl lash_core::plugin::PluginFactory for Frame {
    fn id(&self) -> &'static str {
        FRAME
    }

    fn build(
        &self,
        _: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::SessionPlugin for Frame {
    fn id(&self) -> &'static str {
        FRAME
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        #[expect(
            clippy::expect_used,
            reason = "the simulator's prompt keys are valid literals"
        )]
        let wrap = PromptWrapKey::new("frame").expect("a valid prompt key");
        let world = self.world.clone();
        reg.prompt().wrap(
            PromptWrapSpec::new(wrap, memo_section()),
            Arc::new(
                move |input: &PromptInput<'_>, _: PromptWrapTarget<'_>, previous: SectionText| {
                    if let Some(world) = world.upgrade() {
                        world.note(format!(
                            "{WRAPPED} {} call={}",
                            input.call().session_id,
                            input.call().call
                        ));
                    }
                    Ok(match previous {
                        SectionText::Text(text) => SectionText::Text(format!("{text} (framed)")),
                        SectionText::Omit => SectionText::Omit,
                    })
                },
            ),
        )
    }
}

/// The T and checkpoint count `session`'s head committed for the memo.
///
/// # Errors
///
/// The head does not load.
pub async fn committed_memo(world: &World, session: &str) -> Result<(Option<String>, u64), String> {
    let view = lash_core::store::SessionStore::new(
        world.backend()?.session_store_factory(),
        session_id(session),
    )
    .map_err(|error| error.to_string())?;
    let loaded = lash_core::store::load_session_window_state(
        &view,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| format!("session {session} has no head"))?;
    let memo = loaded
        .state
        .plugin_state()
        .and_then(|state| state.plugins.get(MEMO))
        .map(|namespace| namespace.values.clone())
        .unwrap_or_default();
    Ok((
        memo.get("t")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        memo.get("checkpoints")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    ))
}
