//! Web API method arguments and responses.
//!
//! Argument structs are all-`Option<String>` on purpose. Slack's Web API is
//! argument-string-shaped: a form-encoded call sends `limit=20` and
//! `inclusive=true` as text, and JSON-object arguments (`blocks`,
//! `attachments`, `metadata`) must be sent as *JSON-encoded strings* in a
//! form body. Normalizing everything to strings at the edge (see
//! `platform::args`) means one parse path instead of two, and it keeps the
//! platform accepting exactly what Slack accepts.

use serde::{Deserialize, Serialize};

use super::ResponseMetadata;

/// `chat.postMessage` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChatPostMessageArgs {
    /// Channel id (`C…`). Slack also accepts a channel name; the platform
    /// accepts both and resolves names for parity.
    pub channel: Option<String>,
    /// Message body. Required here — the platform has no Block Kit, so a
    /// text-less call is `no_text` exactly as it is on Slack.
    pub text: Option<String>,
    /// Parent message `ts` to reply in a thread.
    pub thread_ts: Option<String>,
    /// Override the display name of the posting app.
    pub username: Option<String>,
    /// JSON-encoded `{"event_type": "...", "event_payload": {...}}`.
    ///
    /// The bot uses this the way a production Slack app does: it stamps the
    /// originating `event_id` onto its reply so a crashed process can ask the
    /// platform whether the reply already landed instead of guessing.
    pub metadata: Option<String>,
}

/// `conversations.list` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConversationsListArgs {
    pub cursor: Option<String>,
    pub exclude_archived: Option<String>,
    pub limit: Option<String>,
    /// Accepted and ignored: the platform hosts exactly one workspace.
    pub team_id: Option<String>,
    /// Comma-separated. The platform only mints `public_channel`.
    pub types: Option<String>,
}

/// `conversations.history` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConversationsHistoryArgs {
    pub channel: Option<String>,
    pub cursor: Option<String>,
    pub inclusive: Option<String>,
    pub latest: Option<String>,
    pub limit: Option<String>,
    pub oldest: Option<String>,
    pub include_all_metadata: Option<String>,
}

/// `conversations.replies` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConversationsRepliesArgs {
    pub channel: Option<String>,
    /// Thread parent `ts`.
    pub ts: Option<String>,
    pub cursor: Option<String>,
    pub inclusive: Option<String>,
    pub latest: Option<String>,
    pub limit: Option<String>,
    pub oldest: Option<String>,
    pub include_all_metadata: Option<String>,
}

/// `users.list` arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct UsersListArgs {
    pub cursor: Option<String>,
    pub limit: Option<String>,
    pub include_locale: Option<String>,
    pub team_id: Option<String>,
}

/// Structured metadata attached to a message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub event_type: String,
    pub event_payload: serde_json::Value,
}

/// A `message` object, as returned by `chat.postMessage`,
/// `conversations.history` and `conversations.replies`.
///
/// One struct covers human and app messages because that is how Slack models
/// it: a human message carries `user`, an app message carries `bot_id`,
/// `username` and `subtype: "bot_message"`. Every optional field is omitted
/// rather than nulled, so the JSON a client sees matches Slack's byte-for-byte
/// in shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageObject {
    /// Always `"message"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `"bot_message"` for app posts, absent for human posts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// Author user id (`U…`), absent on app posts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Authoring app's bot id (`B…`), present only on app posts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    /// Display name an app posted under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub text: String,
    /// Message identity within the channel.
    pub ts: String,
    /// Thread parent `ts`. Present on the parent itself and on every reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    /// Author of the thread parent. Replies only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_user_id: Option<String>,
    /// Thread parents only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_count: Option<u32>,
    /// Thread parents only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_users_count: Option<u32>,
    /// Thread parents only: `ts` of the newest reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_reply: Option<String>,
    /// Returned only when the caller passes `include_all_metadata=true`,
    /// matching Slack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MessageMetadata>,
}

impl MessageObject {
    /// A human message.
    pub fn from_user(
        user: impl Into<String>,
        text: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            kind: "message".to_string(),
            subtype: None,
            user: Some(user.into()),
            bot_id: None,
            username: None,
            text: text.into(),
            ts: ts.into(),
            thread_ts: None,
            parent_user_id: None,
            reply_count: None,
            reply_users_count: None,
            latest_reply: None,
            metadata: None,
        }
    }

    /// An app message: `subtype: "bot_message"` plus `bot_id`, per Slack.
    pub fn from_bot(
        bot_id: impl Into<String>,
        username: impl Into<String>,
        text: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            kind: "message".to_string(),
            subtype: Some("bot_message".to_string()),
            user: None,
            bot_id: Some(bot_id.into()),
            username: Some(username.into()),
            text: text.into(),
            ts: ts.into(),
            thread_ts: None,
            parent_user_id: None,
            reply_count: None,
            reply_users_count: None,
            latest_reply: None,
            metadata: None,
        }
    }
}

/// A `channel` object from `conversations.list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelObject {
    pub id: String,
    pub name: String,
    pub is_channel: bool,
    pub is_group: bool,
    pub is_im: bool,
    /// Epoch *seconds*, unlike `ts`.
    pub created: u64,
    pub creator: String,
    pub is_archived: bool,
    pub is_general: bool,
    pub name_normalized: String,
    pub is_member: bool,
    pub is_private: bool,
    pub is_mpim: bool,
    pub topic: ChannelText,
    pub purpose: ChannelText,
    pub num_members: u32,
}

/// The `topic` / `purpose` sub-object shape.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChannelText {
    pub value: String,
    pub creator: String,
    pub last_set: u64,
}

/// A `member` object from `users.list`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserObject {
    pub id: String,
    pub team_id: String,
    /// Slack's handle, not the display name.
    pub name: String,
    pub deleted: bool,
    pub color: String,
    pub real_name: String,
    pub tz: String,
    pub tz_label: String,
    pub tz_offset: i32,
    pub profile: UserProfile,
    pub is_admin: bool,
    pub is_owner: bool,
    pub is_primary_owner: bool,
    pub is_restricted: bool,
    pub is_ultra_restricted: bool,
    pub is_bot: bool,
    /// Epoch seconds.
    pub updated: u64,
    pub is_app_user: bool,
}

/// The subset of Slack's `profile` object the platform can honestly populate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserProfile {
    pub real_name: String,
    pub display_name: String,
    pub real_name_normalized: String,
    pub display_name_normalized: String,
    pub team: String,
}

/// `chat.postMessage` success response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatPostMessageResponse {
    pub ok: bool,
    pub channel: String,
    pub ts: String,
    pub message: MessageObject,
}

/// `conversations.list` success response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationsListResponse {
    pub ok: bool,
    pub channels: Vec<ChannelObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata: Option<ResponseMetadata>,
}

/// `conversations.history` success response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationsHistoryResponse {
    pub ok: bool,
    /// Newest first, as Slack returns them.
    pub messages: Vec<MessageObject>,
    pub has_more: bool,
    pub pin_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata: Option<ResponseMetadata>,
}

/// `conversations.replies` success response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationsRepliesResponse {
    pub ok: bool,
    /// Thread parent first, then replies oldest-first — Slack's ordering for
    /// this method, which is the reverse of `conversations.history`.
    pub messages: Vec<MessageObject>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata: Option<ResponseMetadata>,
}

/// `users.list` success response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsersListResponse {
    pub ok: bool,
    pub members: Vec<UserObject>,
    /// Epoch seconds at which the roster snapshot was taken.
    pub cache_ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata: Option<ResponseMetadata>,
}

/// `auth.test` success response — how an app learns its own identity, and
/// therefore how the bot learns the `U…` id it must look for in `<@…>` mention
/// syntax.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthTestResponse {
    pub ok: bool,
    pub url: String,
    pub team: String,
    /// The app's bot *user* handle.
    pub user: String,
    pub team_id: String,
    /// The app's bot *user* id (`U…`) — the mention target.
    pub user_id: String,
    /// The app's bot id (`B…`) — stamped on messages it posts.
    pub bot_id: String,
}
