//! The bot's client for the platform's Slack-compatible Web API.
//!
//! **This is the seam that makes the example liftable.** Every method is named
//! after the Slack method it calls (`chat_post_message` for
//! `chat.postMessage`), takes the same arguments, and returns the same response
//! type from [`crate::wire::methods`]. Moving this bot onto real Slack is
//! therefore a contained swap: point [`SlackApi::new`] at `https://slack.com`,
//! hand it a real `xoxb-` token, and the transport needs no other change.
//!
//! Two behaviours here are load-bearing and easy to get wrong.
//!
//! **Encoding.** Slack accepts `application/x-www-form-urlencoded` for *every*
//! Web API method, and JSON for only some — mostly writes. So this client
//! form-encodes by default and uses JSON only for `chat.postMessage`, which needs
//! it for the `metadata` object. Sending JSON to `conversations.history` works
//! against a permissive server and fails against real Slack, which is the worst
//! possible failure mode for an example that advertises a migration.
//!
//! **Success.** Slack answers failures with **HTTP 200** and
//! `{"ok": false, "error": "..."}`, so checking the status code is not checking
//! for success. Every call goes through the same `ok` gate in [`SlackApi::call`].

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::wire::methods::{
    AuthTestResponse, ChatPostMessageResponse, ConversationsHistoryResponse,
    ConversationsListResponse, ConversationsRepliesResponse, MessageMetadata, UsersListResponse,
};

/// `event_type` the bot stamps on every reply's `metadata`.
///
/// This is not decoration. It is how a restarted bot answers "did I already
/// reply to this event?" without a distributed transaction: the originating
/// `event_id` travels with the reply into the platform's durable message store,
/// so recovery is a read (`conversations.history` with
/// `include_all_metadata=true`) rather than a guess.
pub const REPLY_METADATA_EVENT_TYPE: &str = "slack_clone_bot_reply";

/// Slack's maximum `limit` for `conversations.history`.
const MAX_HISTORY_LIMIT: u32 = 999;

/// A typed client for one workspace.
#[derive(Clone, Debug)]
pub struct SlackApi {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl SlackApi {
    /// Build a client. `base_url` is the API origin (`https://slack.com` for
    /// real Slack), `token` the bot token sent as `Authorization: Bearer …`.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .context("build Slack API HTTP client")?,
        })
    }

    /// The API origin this client talks to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `auth.test` — resolve the app's own identity.
    pub async fn auth_test(&self) -> Result<AuthTestResponse> {
        self.call_form("auth.test", &[]).await
    }

    /// `chat.postMessage`.
    ///
    /// The one JSON call. Slack accepts JSON for this method, and `metadata` is
    /// an object — form-encoding it would mean serializing the object into a
    /// string argument, which Slack allows but which is strictly more code and
    /// strictly less clear.
    pub async fn chat_post_message(
        &self,
        request: &ChatPostMessageRequest,
    ) -> Result<ChatPostMessageResponse> {
        self.call_json("chat.postMessage", request).await
    }

    /// `conversations.list`.
    pub async fn conversations_list(
        &self,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ConversationsListResponse> {
        let mut args = vec![
            ("exclude_archived".to_string(), "true".to_string()),
            ("limit".to_string(), limit.unwrap_or(100).to_string()),
        ];
        push_optional(&mut args, "cursor", cursor);
        self.call_form("conversations.list", &args).await
    }

    /// `conversations.history`.
    pub async fn conversations_history(
        &self,
        query: &HistoryQuery,
    ) -> Result<ConversationsHistoryResponse> {
        let mut args = vec![
            ("channel".to_string(), query.channel.clone()),
            (
                "limit".to_string(),
                query.limit.clamp(1, MAX_HISTORY_LIMIT).to_string(),
            ),
        ];
        if query.include_all_metadata {
            args.push(("include_all_metadata".to_string(), "true".to_string()));
        }
        if query.inclusive {
            args.push(("inclusive".to_string(), "true".to_string()));
        }
        push_optional(&mut args, "oldest", query.oldest.as_deref());
        push_optional(&mut args, "latest", query.latest.as_deref());
        push_optional(&mut args, "cursor", query.cursor.as_deref());
        self.call_form("conversations.history", &args).await
    }

    /// `conversations.replies`.
    ///
    /// Present and exercised by the wire tests, unused by the bot's reply path:
    /// the bot does not thread yet (see the README's deferred list). Keeping the
    /// method here means adding threads is a change to the bot's policy, not to
    /// its transport.
    pub async fn conversations_replies(
        &self,
        channel: &str,
        ts: &str,
        limit: u32,
    ) -> Result<ConversationsRepliesResponse> {
        self.conversations_replies_page(channel, ts, limit, None)
            .await
    }

    async fn conversations_replies_page(
        &self,
        channel: &str,
        ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ConversationsRepliesResponse> {
        let mut args = vec![
            ("channel".to_string(), channel.to_string()),
            ("ts".to_string(), ts.to_string()),
            ("limit".to_string(), limit.to_string()),
            ("include_all_metadata".to_string(), "true".to_string()),
        ];
        push_optional(&mut args, "cursor", cursor);
        self.call_form("conversations.replies", &args).await
    }

    /// `users.list`.
    pub async fn users_list(&self, cursor: Option<&str>) -> Result<UsersListResponse> {
        let mut args = vec![("limit".to_string(), "200".to_string())];
        push_optional(&mut args, "cursor", cursor);
        self.call_form("users.list", &args).await
    }

    /// POST a method with a form-encoded body — the encoding every Slack method
    /// accepts.
    async fn call_form<R>(&self, method: &str, args: &[(String, String)]) -> Result<R>
    where
        R: DeserializeOwned,
    {
        let response = self
            .http
            .post(self.method_url(method))
            .bearer_auth(&self.token)
            .form(args)
            .send()
            .await
            .with_context(|| format!("call {method}"))?;
        self.finish(method, response).await
    }

    /// POST a method with a JSON body.
    async fn call_json<A, R>(&self, method: &str, args: &A) -> Result<R>
    where
        A: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let response = self
            .http
            .post(self.method_url(method))
            .bearer_auth(&self.token)
            .json(args)
            .send()
            .await
            .with_context(|| format!("call {method}"))?;
        self.finish(method, response).await
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/api/{method}", self.base_url)
    }

    /// Enforce Slack's `ok` contract and decode.
    async fn finish<R>(&self, method: &str, response: reqwest::Response) -> Result<R>
    where
        R: DeserializeOwned,
    {
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .with_context(|| format!("decode {method} response"))?;
        // Slack signals failure in the body, not the status line. Check `ok`
        // first so a 200 carrying `invalid_auth` cannot be mistaken for success,
        // then still surface a non-2xx as a transport failure.
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            let error = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            bail!("{method} failed: {error}");
        }
        if !status.is_success() {
            bail!("{method} returned HTTP {status}");
        }
        serde_json::from_value(body).with_context(|| format!("decode {method} payload"))
    }
}

fn push_optional(args: &mut Vec<(String, String)>, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        args.push((name.to_string(), value.to_string()));
    }
}

/// Arguments for [`SlackApi::conversations_history`].
///
/// A struct rather than eight positional parameters because the bot's recovery
/// path needs the `ts` bounds and its tools do not, and a call site that reads
/// `HistoryQuery::since(channel, ts)` says what it means.
#[derive(Clone, Debug)]
pub struct HistoryQuery {
    pub channel: String,
    pub limit: u32,
    pub include_all_metadata: bool,
    pub oldest: Option<String>,
    pub latest: Option<String>,
    pub inclusive: bool,
    pub cursor: Option<String>,
}

impl HistoryQuery {
    /// The newest `limit` messages in a channel.
    pub fn latest(channel: impl Into<String>, limit: u32) -> Self {
        Self {
            channel: channel.into(),
            limit,
            include_all_metadata: false,
            oldest: None,
            latest: None,
            inclusive: false,
            cursor: None,
        }
    }

    /// Everything at or after `ts`, with metadata, one full page at a time.
    ///
    /// Bounding the scan by *message identity* rather than by a message count is
    /// what makes the recovery lookup sound: a fixed "newest 50" window can miss
    /// the bot's own reply in a busy channel and so cause the duplicate it was
    /// added to prevent.
    pub fn since(channel: impl Into<String>, ts: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            limit: MAX_HISTORY_LIMIT,
            include_all_metadata: true,
            oldest: Some(ts.into()),
            latest: None,
            inclusive: true,
            cursor: None,
        }
    }

    /// The same query continued from `cursor`.
    pub fn at_cursor(&self, cursor: impl Into<String>) -> Self {
        Self {
            cursor: Some(cursor.into()),
            ..self.clone()
        }
    }
}

/// Arguments for [`SlackApi::chat_post_message`], mirroring Slack's.
#[derive(Clone, Debug, Serialize)]
pub struct ChatPostMessageRequest {
    pub channel: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_broadcast: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MessageMetadata>,
}

impl ChatPostMessageRequest {
    /// A channel reply carrying the originating `event_id` in `metadata`.
    pub fn reply(channel: impl Into<String>, text: impl Into<String>, event_id: &str) -> Self {
        Self {
            channel: channel.into(),
            text: text.into(),
            thread_ts: None,
            reply_broadcast: None,
            metadata: Some(MessageMetadata {
                event_type: REPLY_METADATA_EVENT_TYPE.to_string(),
                event_payload: serde_json::json!({ "event_id": event_id }),
            }),
        }
    }

    /// A reply in a thread. Lash-side routing remains on the thread session even
    /// if a caller elects to broadcast the posted message into channel history.
    pub fn thread_reply(
        channel: impl Into<String>,
        text: impl Into<String>,
        event_id: &str,
        thread_ts: impl Into<String>,
    ) -> Self {
        Self {
            channel: channel.into(),
            text: text.into(),
            thread_ts: Some(thread_ts.into()),
            reply_broadcast: None,
            metadata: Some(MessageMetadata {
                event_type: REPLY_METADATA_EVENT_TYPE.to_string(),
                event_payload: serde_json::json!({ "event_id": event_id }),
            }),
        }
    }
}

/// Find the `ts` of a reply this bot already posted for `event_id`.
///
/// Used on recovery: a bot that crashed between committing a turn and recording
/// its post asks the platform what happened instead of assuming either way.
pub fn find_reply_for_event(
    history: &ConversationsHistoryResponse,
    bot_id: &str,
    event_id: &str,
) -> Option<String> {
    history
        .messages
        .iter()
        .filter(|message| message.bot_id.as_deref() == Some(bot_id))
        .find(|message| {
            message.metadata.as_ref().is_some_and(|metadata| {
                metadata.event_type == REPLY_METADATA_EVENT_TYPE
                    && metadata
                        .event_payload
                        .get("event_id")
                        .and_then(Value::as_str)
                        == Some(event_id)
            })
        })
        .map(|message| message.ts.clone())
}

/// Walk every page at or after `message_ts` looking for this bot's reply to
/// `event_id`.
///
/// Follows the pagination cursor to exhaustion within the `ts` window, so the
/// answer does not depend on how busy the channel has been since.
pub async fn find_posted_reply(
    api: &SlackApi,
    bot_id: &str,
    channel: &str,
    message_ts: &str,
    event_id: &str,
    thread_ts: Option<&str>,
) -> Result<Option<String>> {
    if let Some(thread_ts) = thread_ts {
        let mut page = api
            .conversations_replies(channel, thread_ts, MAX_HISTORY_LIMIT)
            .await?;
        loop {
            if let Some(reply_ts) = find_reply_in_messages(&page.messages, bot_id, event_id) {
                return Ok(Some(reply_ts));
            }
            let next = page
                .response_metadata
                .as_ref()
                .map(|metadata| metadata.next_cursor.clone())
                .filter(|cursor| !cursor.is_empty());
            let Some(cursor) = next.filter(|_| page.has_more) else {
                return Ok(None);
            };
            page = api
                .conversations_replies_page(channel, thread_ts, MAX_HISTORY_LIMIT, Some(&cursor))
                .await?;
        }
    }
    let query = HistoryQuery::since(channel, message_ts);
    let mut page = api.conversations_history(&query).await?;
    loop {
        if let Some(reply_ts) = find_reply_for_event(&page, bot_id, event_id) {
            return Ok(Some(reply_ts));
        }
        let next = page
            .response_metadata
            .as_ref()
            .map(|metadata| metadata.next_cursor.clone())
            .filter(|cursor| !cursor.is_empty());
        let Some(cursor) = next.filter(|_| page.has_more) else {
            return Ok(None);
        };
        page = api.conversations_history(&query.at_cursor(cursor)).await?;
    }
}

fn find_reply_in_messages(
    messages: &[crate::wire::methods::MessageObject],
    bot_id: &str,
    event_id: &str,
) -> Option<String> {
    messages
        .iter()
        .filter(|message| message.bot_id.as_deref() == Some(bot_id))
        .find(|message| {
            message.metadata.as_ref().is_some_and(|metadata| {
                metadata.event_type == REPLY_METADATA_EVENT_TYPE
                    && metadata
                        .event_payload
                        .get("event_id")
                        .and_then(Value::as_str)
                        == Some(event_id)
            })
        })
        .map(|message| message.ts.clone())
}
