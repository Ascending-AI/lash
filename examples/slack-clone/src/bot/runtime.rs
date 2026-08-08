//! Building the bot's `LashCore` — the standard-mode embedding.
//!
//! `LashCore::standard_builder()` gives a native tool loop and plain chat turns:
//! the model answers in prose and calls host tools directly. That is the classic
//! chat-bot shape and the reason this example, not `agent-workbench`, is the
//! repo's standard-mode reference. Nothing here touches Lashlang, code cells,
//! processes or triggers — those are RLM-mode concerns and their absence is
//! deliberate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use lash::persistence::LeaseOwnerIdentity;
use lash::prompt::{PromptContribution, PromptLayer};
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::tracing::{JsonlTraceSink, StderrTraceSink, TeeTraceSink, TraceLevel, TraceSink};
use lash::{LashCore, ModelSpec, SessionSpec};
use lash_plugin_mcp::{McpPluginFactory, McpServerConfig};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

use super::slack_api::SlackApi;
use super::tools;
use crate::mcp_server::{API_BASE_URL_ENV, BOT_TOKEN_ENV};

const DEMO_MCP_SERVER_NAME: &str = "slack_clone";
const DEMO_MCP_SERVER_BINARY: &str = "slack-clone-mcp-server";

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
    /// MCP servers registered into the bot's standard tool catalog.
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl RuntimeConfig {
    /// A config rooted at `data_dir` with a fresh incarnation.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            trace_path: None,
            incarnation: fresh_incarnation(),
            trace_to_stderr: true,
            mcp_servers: BTreeMap::new(),
        }
    }

    /// Wire the bundled stdio server to the platform API used by this bot.
    pub fn with_demo_mcp_server(mut self, api_base_url: &str, bot_token: &str) -> Result<Self> {
        let command = match std::env::var("SLACK_CLONE_MCP_SERVER") {
            Ok(command) => PathBuf::from(command),
            Err(std::env::VarError::NotPresent) => demo_mcp_server_binary()?,
            Err(error) => return Err(error).context("read SLACK_CLONE_MCP_SERVER"),
        };
        let mut env = BTreeMap::new();
        env.insert(API_BASE_URL_ENV.to_string(), api_base_url.to_string());
        env.insert(BOT_TOKEN_ENV.to_string(), bot_token.to_string());
        self.mcp_servers.insert(
            DEMO_MCP_SERVER_NAME.to_string(),
            McpServerConfig::Stdio {
                command: command.display().to_string(),
                args: Vec::new(),
                env,
                cwd: None,
                startup_timeout_ms: 10_000,
                call_timeout_ms: 20_000,
                binary_content_attachments: false,
            },
        );
        Ok(self)
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
    validate_stdio_commands(&config.mcp_servers)?;

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

    let mcp = if config.mcp_servers.is_empty() {
        None
    } else {
        Some(Arc::new(
            McpPluginFactory::new(config.mcp_servers.clone())
                .await
                .context("connect slack-clone MCP servers")?,
        ))
    };
    let mut builder = LashCore::standard_builder()
        .provider(provider)
        // `session_spec` replaces the builder's whole spec, so it must precede
        // `model`, which writes into that same spec.
        .session_spec(SessionSpec::new().prompt_layer(bot_prompt(
            config.mcp_servers.contains_key(DEMO_MCP_SERVER_NAME),
        )))
        .model(model)
        .store_factory(store_factory)
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .process_env_store(process_env_store)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .tools(tools::workspace_tools(api))
        .trace_sink(trace_sink(config))
        .trace_level(TraceLevel::Extended);
    if let Some(mcp) = mcp {
        builder = builder.plugin(mcp);
    }
    builder
        // Ambient channel traffic is admitted as queued turn input but must NOT
        // provoke a reply. The default inline queued-work driver would drain that
        // input on its own schedule and run a turn nobody asked for, so the bot
        // takes the decision back: every turn in this host starts because a human
        // mentioned the bot.
        .disable_queued_work_driver()
        .build()
        .context("build slack-clone bot Lash core")
}

fn demo_mcp_server_binary() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locate slack-clone bot binary")?;
    let extension = current.extension().map(|value| value.to_owned());
    let mut binary = current.with_file_name(DEMO_MCP_SERVER_BINARY);
    if let Some(extension) = extension {
        binary.set_extension(extension);
    }
    Ok(binary)
}

fn validate_stdio_commands(servers: &BTreeMap<String, McpServerConfig>) -> Result<()> {
    for (server_name, config) in servers {
        let McpServerConfig::Stdio { command, cwd, .. } = config else {
            continue;
        };
        if resolve_stdio_command(command, cwd.as_deref()).is_none() {
            bail!(
                "MCP server `{server_name}` executable `{command}` does not exist; \
                 build it or correct SLACK_CLONE_MCP_SERVER before starting the bot"
            );
        }
    }
    Ok(())
}

fn resolve_stdio_command(command: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        let candidate = if command_path.is_absolute() {
            command_path.to_owned()
        } else {
            cwd.unwrap_or_else(|| Path::new(".")).join(command_path)
        };
        return candidate.is_file().then_some(candidate);
    }

    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path)
        .map(|directory| directory.join(command_path))
        .find(|candidate| candidate.is_file())
}

/// The bot's system prompt, expressed as a session prompt layer.
fn bot_prompt(include_demo_mcp: bool) -> PromptLayer {
    let mut prompt = PromptLayer::new()
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
        ));
    if include_demo_mcp {
        prompt = prompt.with_contribution(PromptContribution::guidance(
            "MCP workspace tools",
            "The bundled MCP server exposes `mcp__slack_clone__list_channels_summary` and \
             `mcp__slack_clone__workspace_stats`; use those when the question asks for a \
             compact channel summary or aggregate workspace counts.",
        ));
    }
    prompt
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
/// anything shorter-lived — a mention or a process lifetime — would throw the
/// room's memory away every time somebody asked a question. Threads branch from
/// this session; they do not replace it.
pub fn session_id(channel_id: &str) -> String {
    format!("channel:{channel_id}")
}

/// Stable id for the forked session behind one Slack thread.
pub fn thread_session_id(channel_id: &str, thread_ts: &str) -> String {
    format!("thread:{channel_id}:{thread_ts}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_stdio_command_is_rejected_before_boot() {
        let missing = std::env::temp_dir().join(format!(
            "slack-clone-missing-mcp-server-{}",
            std::process::id()
        ));
        let servers = BTreeMap::from([(
            DEMO_MCP_SERVER_NAME.to_string(),
            McpServerConfig::stdio(missing.display().to_string(), Vec::new()),
        )]);

        let error = validate_stdio_commands(&servers).expect_err("missing command must fail boot");
        let message = error.to_string();
        assert!(message.contains(DEMO_MCP_SERVER_NAME));
        assert!(message.contains("does not exist"));
        assert!(message.contains("SLACK_CLONE_MCP_SERVER"));
    }
}
