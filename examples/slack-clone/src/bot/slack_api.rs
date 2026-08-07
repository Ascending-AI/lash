//! The bot's client for the platform's Slack-compatible Web API.
//!
//! **This is the seam that makes the example liftable.** Every method is named
//! after the Slack method it calls (`chat_post_message` for
//! `chat.postMessage`), takes the same arguments, and returns the same response
//! type from [`crate::wire::methods`]. Moving this bot onto real Slack is
//! therefore a contained swap: point [`SlackApi::base_url`] at
//! `https://slack.com`, hand it a real `xoxb-` token, and delete nothing else.
//!
//! The one behaviour worth reading carefully is [`SlackApi::call`]: Slack
//! answers failures with **HTTP 200** and `{"ok": false, "error": "..."}`, so
//! checking the status code is not checking for success. Every call goes through
//! the same `ok` gate.

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
        self.call("auth.test", &Value::Object(Default::default()))
            .await
    }

    /// `chat.postMessage`.
    pub async fn chat_post_message(
        &self,
        request: &ChatPostMessageRequest,
    ) -> Result<ChatPostMessageResponse> {
        self.call("chat.postMessage", request).await
    }

    /// `conversations.list`.
    pub async fn conversations_list(
        &self,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ConversationsListResponse> {
        self.call(
            "conversations.list",
            &serde_json::json!({
                "exclude_archived": true,
                "cursor": cursor.unwrap_or_default(),
                "limit": limit.unwrap_or(100),
            }),
        )
        .await
    }

    /// `conversations.history`.
    pub async fn conversations_history(
        &self,
        channel: &str,
        limit: u32,
        include_all_metadata: bool,
    ) -> Result<ConversationsHistoryResponse> {
        self.call(
            "conversations.history",
            &serde_json::json!({
                "channel": channel,
                "limit": limit,
                "include_all_metadata": include_all_metadata,
            }),
        )
        .await
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
        self.call(
            "conversations.replies",
            &serde_json::json!({ "channel": channel, "ts": ts, "limit": limit }),
        )
        .await
    }

    /// `users.list`.
    pub async fn users_list(&self, cursor: Option<&str>) -> Result<UsersListResponse> {
        self.call(
            "users.list",
            &serde_json::json!({ "cursor": cursor.unwrap_or_default(), "limit": 200 }),
        )
        .await
    }

    /// POST a Web API method and enforce Slack's `ok` contract.
    async fn call<A, R>(&self, method: &str, args: &A) -> Result<R>
    where
        A: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let url = format!("{}/api/{method}", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(args)
            .send()
            .await
            .with_context(|| format!("call {method}"))?;
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

/// Arguments for [`SlackApi::chat_post_message`], mirroring Slack's.
#[derive(Clone, Debug, Serialize)]
pub struct ChatPostMessageRequest {
    pub channel: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
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
