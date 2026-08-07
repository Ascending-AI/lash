//! Building the bot's `LashCore` — the standard-mode embedding.
//!
//! `LashCore::standard_builder()` gives a native tool loop and plain chat turns:
//! the model answers in prose and calls host tools directly. That is the classic
//! chat-bot shape and the reason this example, not `agent-workbench`, is the
//! repo's standard-mode reference. Nothing here touches Lashlang, code cells,
//! processes or triggers — those are RLM-mode concerns and their absence is
//! deliberate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use lash::persistence::LeaseOwnerIdentity;
use lash::prompt::{PromptContribution, PromptLayer};
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::tracing::{JsonlTraceSink, StderrTraceSink, TeeTraceSink, TraceLevel, TraceSink};
use lash::{LashCore, ModelSpec, SessionSpec};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

use super::slack_api::SlackApi;
use super::tools;

/// Where the bot's durable Lash state lives, and how this boot identifies itself.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Root for the session stores and the trace file.
    pub data_dir: PathBuf,
    /// JSONL trace destination. Defaults to `<data_dir>/trace.jsonl`.
    pub trace_path: Option<PathBuf>,
    /// Distinguishes this boot from the previous one for lease reclaim.
    pub incarnation: String,
    /// Whether to mirror trace records to stderr.
    pub trace_to_stderr: bool,
}

impl RuntimeConfig {
    /// A config rooted at `data_dir` with a fresh incarnation.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            trace_path: None,
            incarnation: fresh_incarnation(),
            trace_to_stderr: true,
        }
    }
}

/// A stable, per-boot session-execution owner.
///
/// The owner id is stable across restarts and the incarnation is not, which is
/// what lets a new boot reclaim the leases a crashed boot left behind instead of
/// deadlocking against its own ghost. A bot restarted mid-conversation depends on
/// this: without it, the channel session stays locked to a process that is gone.
pub fn session_owner(incarnation: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque("slack-clone-bot", incarnation)
}

/// Build the standard-mode core.
///
/// Durability choices, all of them deliberate for an example:
///
/// * **SQLite session stores** — the committed transcript, and any queued turn
///   input not yet drained, survive a restart. This is the load-bearing one.
/// * **Inline effect host** — process-local effect journalling. Enough to make
///   the bot correct within a boot; not enough to make a turn interrupted
///   mid-flight resume itself. The README documents the Restate upgrade.
/// * **No queued-work driver** — see [`LashCore::disable_queued_work_driver`]
///   below; the bot alone decides when a turn runs.
pub async fn build_core(
    config: &RuntimeConfig,
    provider: ProviderHandle,
    model: ModelSpec,
    api: Arc<SlackApi>,
) -> Result<LashCore> {
    let data_dir = &config.data_dir;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create bot data dir {}", data_dir.display()))?;

    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let process_env_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
            .await
            .map_err(|error| anyhow::anyhow!("open process env store: {error}"))?,
    );

    LashCore::standard_builder()
        .provider(provider)
        // `session_spec` replaces the builder's whole spec, so it must precede
        // `model`, which writes into that same spec.
        .session_spec(SessionSpec::new().prompt_layer(bot_prompt()))
        .model(model)
        .store_factory(store_factory)
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .process_env_store(process_env_store)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .tools(tools::workspace_tools(api))
        .trace_sink(trace_sink(config))
        .trace_level(TraceLevel::Extended)
        // Ambient channel traffic is admitted as queued turn input but must NOT
        // provoke a reply. The default inline queued-work driver would drain that
        // input on its own schedule and run a turn nobody asked for, so the bot
        // takes the decision back: every turn in this host starts because a human
        // mentioned the bot.
        .disable_queued_work_driver()
        .build()
        .context("build slack-clone bot Lash core")
}

/// The bot's system prompt, expressed as a session prompt layer.
fn bot_prompt() -> PromptLayer {
    PromptLayer::new()
        .with_contribution(PromptContribution::intro(
            "Role",
            "You are a helpful assistant in a team chat workspace. Each conversation \
             belongs to one channel, and you see the channel's traffic as it happens: \
             messages that do not mention you arrive as context you should remember but \
             not answer. Reply only to the message that mentions you.",
        ))
        .with_contribution(PromptContribution::guidance(
            "Chat style",
            "Answer in one or two short paragraphs of plain text. There is no rich \
             formatting in this client, so avoid headings, tables and long bullet lists. \
             Refer to people by the display names you see in the transcript.",
        ))
        .with_contribution(PromptContribution::guidance(
            "Workspace tools",
            "Use `list_channels` and `channel_history` when a question is about the \
             workspace itself rather than about this channel's conversation. Do not guess \
             at channel names or at what was said somewhere else.",
        ))
}

/// Tee stderr and a JSONL file, matching the other examples' trace idiom.
fn trace_sink(config: &RuntimeConfig) -> Arc<dyn TraceSink> {
    let path = config
        .trace_path
        .clone()
        .unwrap_or_else(|| config.data_dir.join("trace.jsonl"));
    if config.trace_to_stderr {
        eprintln!("slack-clone-bot trace: {}", path.display());
        Arc::new(TeeTraceSink::new([
            Arc::new(StderrTraceSink::default()) as Arc<dyn TraceSink>,
            Arc::new(JsonlTraceSink::new(path)),
        ]))
    } else {
        Arc::new(JsonlTraceSink::new(path))
    }
}

/// Resolve the live provider and model from the environment.
///
/// Kept separate from [`build_core`] so tests can hand in
/// `lash::testing::TestProvider` and never reach for a network or a key.
pub fn provider_from_env() -> Result<(ProviderHandle, ModelSpec)> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY is required to run the bot against a real model")?;
    let model = std::env::var("OPENROUTER_MODEL")
        .unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string());
    let provider = ProviderHandle::new(
        OpenAiCompatibleProvider::new(api_key, OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_options(ProviderOptions {
                expose_thinking: false,
                ..ProviderOptions::default()
            })
            .into_components(),
    );
    let spec = ModelSpec::from_token_limits(model, Default::default(), 200_000, None)
        .map_err(|error| anyhow::anyhow!("invalid OPENROUTER_MODEL metadata: {error}"))?
        .with_capability(lash::provider::ModelCapability {
            cache_control: Some(lash::provider::CacheControlDialect::Anthropic),
            ..Default::default()
        });
    Ok((provider, spec))
}

/// The session id for a channel.
///
/// **Session-per-channel is the mapping doctrine of this example.** A channel is
/// a durable, long-lived conversation with a stable id that the platform already
/// guarantees is unique, so it maps one-to-one onto a Lash session. Keying on
/// anything shorter-lived — a mention, a thread, a process lifetime — would
/// throw the room's memory away every time somebody asked a question.
pub fn session_id(channel_id: &str) -> String {
    format!("channel:{channel_id}")
}

/// Trace/store root under a data directory, used by the dev script and tests.
pub fn store_root(data_dir: &Path) -> PathBuf {
    data_dir.join("lash-sessions")
}

fn fresh_incarnation() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
