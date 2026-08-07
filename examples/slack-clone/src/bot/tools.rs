//! Native host tools, exposed to the standard-mode tool loop.
//!
//! The bot is the repo's standard-mode reference host, so the tool loop has to
//! be genuinely exercised rather than declared. Both tools are backed by real
//! `conversations.*` calls against the platform, which means a turn that uses
//! one performs a real round trip out of the runtime and back — the thing a
//! decorative echo tool would not prove.
//!
//! They also make the bot *useful*: "what channels are there" and "what did
//! people say in #random" are the two questions a workspace bot is actually
//! asked, and neither can be answered from the session transcript alone.

use std::sync::Arc;

use async_trait::async_trait;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition, ToolProvider, ToolResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::slack_api::SlackApi;

/// Name of the channel-listing tool, as the model sees it.
pub const LIST_CHANNELS: &str = "list_channels";
/// Name of the history-reading tool, as the model sees it.
pub const CHANNEL_HISTORY: &str = "channel_history";

/// How many messages `channel_history` will return at most.
const MAX_HISTORY: u32 = 50;

/// `list_channels` takes no arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListChannelsArgs {}

/// `list_channels` output.
#[derive(Debug, Serialize, JsonSchema)]
struct ListChannelsOutput {
    channels: Vec<ChannelSummary>,
}

/// One channel in `list_channels` output.
#[derive(Debug, Serialize, JsonSchema)]
struct ChannelSummary {
    id: String,
    name: String,
    topic: String,
    num_members: u32,
}

/// `channel_history` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ChannelHistoryArgs {
    /// Channel id (`C…`) or name, with or without a leading `#`.
    channel: String,
    /// Newest-first message count. Defaults to 20, capped at 50.
    limit: Option<u32>,
}

/// `channel_history` output.
#[derive(Debug, Serialize, JsonSchema)]
struct ChannelHistoryOutput {
    channel: String,
    /// Oldest first, so the model reads the excerpt in conversation order even
    /// though the API returns it newest-first.
    messages: Vec<HistoryEntry>,
}

/// One message in `channel_history` output.
#[derive(Debug, Serialize, JsonSchema)]
struct HistoryEntry {
    ts: String,
    author: String,
    text: String,
}

/// Tool implementations bound to one workspace client.
struct WorkspaceTools {
    api: Arc<SlackApi>,
}

#[async_trait]
impl StaticToolExecute for WorkspaceTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        match call.name {
            LIST_CHANNELS => self.list_channels().await,
            CHANNEL_HISTORY => match serde_json::from_value(call.args.clone()) {
                Ok(args) => self.channel_history(args).await,
                Err(error) => ToolResult::err_fmt(format_args!("invalid arguments: {error}")),
            },
            other => ToolResult::err_fmt(format_args!("unknown tool: {other}")),
        }
    }
}

impl WorkspaceTools {
    async fn list_channels(&self) -> ToolResult {
        // One page is the whole workspace for an example-sized platform. A tool
        // that silently truncated a paginated list would teach the wrong thing,
        // so the cursor is followed to exhaustion.
        let mut channels = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = match self
                .api
                .conversations_list(cursor.as_deref(), Some(100))
                .await
            {
                Ok(page) => page,
                Err(error) => return ToolResult::err_fmt(format_args!("{error}")),
            };
            channels.extend(page.channels.into_iter().map(|channel| ChannelSummary {
                id: channel.id,
                name: channel.name,
                topic: channel.topic.value,
                num_members: channel.num_members,
            }));
            cursor = page
                .response_metadata
                .map(|metadata| metadata.next_cursor)
                .filter(|next| !next.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        into_result(&ListChannelsOutput { channels })
    }

    async fn channel_history(&self, args: ChannelHistoryArgs) -> ToolResult {
        let limit = args.limit.unwrap_or(20).clamp(1, MAX_HISTORY);
        let history = match self
            .api
            .conversations_history(&args.channel, limit, false)
            .await
        {
            Ok(history) => history,
            Err(error) => return ToolResult::err_fmt(format_args!("{error}")),
        };
        let mut messages: Vec<HistoryEntry> = history
            .messages
            .into_iter()
            .map(|message| HistoryEntry {
                ts: message.ts,
                author: message
                    .username
                    .or(message.user)
                    .or(message.bot_id)
                    .unwrap_or_else(|| "unknown".to_string()),
                text: message.text,
            })
            .collect();
        messages.reverse();
        into_result(&ChannelHistoryOutput {
            channel: args.channel,
            messages,
        })
    }
}

fn into_result<T: Serialize>(output: &T) -> ToolResult {
    match serde_json::to_value(output) {
        Ok(value) => ToolResult::ok(value),
        Err(error) => ToolResult::err_fmt(format_args!("serialize tool output: {error}")),
    }
}

/// Build the bot's tool provider.
pub fn workspace_tools(api: Arc<SlackApi>) -> Arc<dyn ToolProvider> {
    let definitions = vec![
        ToolDefinition::typed::<ListChannelsArgs, ListChannelsOutput>(
            "tool:slack_clone.list_channels",
            LIST_CHANNELS,
            "List the channels in this workspace, with their topics and member counts.",
        ),
        ToolDefinition::typed::<ChannelHistoryArgs, ChannelHistoryOutput>(
            "tool:slack_clone.channel_history",
            CHANNEL_HISTORY,
            "Read recent messages from a channel by id or name, oldest first.",
        ),
    ];
    Arc::new(StaticToolProvider::new(definitions, WorkspaceTools { api })) as Arc<dyn ToolProvider>
}
