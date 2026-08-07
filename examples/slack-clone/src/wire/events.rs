//! Events API payloads: the outer callback envelope, the `url_verification`
//! handshake, and the two event bodies the platform emits.
//!
//! Delivery is at-least-once and unordered-under-retry, so `event_id` — not
//! the event body — is the deduplication key. That is a property of real Slack,
//! not a simplification here, and the bot's ledger is built around it.

use serde::{Deserialize, Serialize};

/// Retry headers Slack sets on a redelivery. Mirrored exactly (lowercase on the
/// wire; HTTP header names are case-insensitive).
pub const RETRY_NUM_HEADER: &str = "x-slack-retry-num";
/// Companion to [`RETRY_NUM_HEADER`], carrying why the first attempt failed.
pub const RETRY_REASON_HEADER: &str = "x-slack-retry-reason";

/// Reasons Slack reports in [`RETRY_REASON_HEADER`]. The platform can only
/// produce `http_timeout`, `connection_failed` and `http_error`; the rest are
/// listed so a bot written against this example already knows the full real
/// vocabulary.
pub const RETRY_REASONS: &[&str] = &[
    "http_timeout",
    "too_many_redirects",
    "connection_failed",
    "ssl_error",
    "http_error",
    "unknown_error",
];

/// A request POSTed to an app's Events API request URL.
///
/// Slack multiplexes the handshake and real events onto one endpoint and
/// discriminates on the top-level `type`, so the receiver must be a tagged
/// enum rather than a single struct with optional fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventRequest {
    /// Endpoint ownership handshake, sent when the request URL is registered.
    UrlVerification(UrlVerification),
    /// A subscribed event.
    ///
    /// Boxed: the envelope is an order of magnitude larger than the handshake,
    /// and every handshake would otherwise carry its footprint.
    EventCallback(Box<EventCallback>),
}

/// The `url_verification` challenge body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UrlVerification {
    /// Deprecated shared verification token; still sent by Slack.
    pub token: String,
    /// Echo this value back to prove endpoint ownership.
    pub challenge: String,
}

/// The response to a `url_verification`. Slack accepts a plaintext echo or this
/// JSON form; the bot answers with JSON and the platform accepts either.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChallengeResponse {
    pub challenge: String,
}

/// The `event_callback` envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventCallback {
    /// Deprecated shared verification token. Real Slack apps must verify the
    /// `X-Slack-Signature` HMAC instead; see the README's "what real Slack
    /// adds".
    pub token: String,
    pub team_id: String,
    pub api_app_id: String,
    /// The event body.
    pub event: Event,
    /// Globally unique (`Ev…`). **The deduplication key.**
    pub event_id: String,
    /// Epoch seconds.
    pub event_time: u64,
    /// Which installation the event is authorized for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorizations: Vec<Authorization>,
}

/// An entry in the envelope's `authorizations` array.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Authorization {
    pub team_id: String,
    pub user_id: String,
    pub is_bot: bool,
    pub is_enterprise_install: bool,
}

/// The event bodies the platform emits.
///
/// Slack delivers **both** a `message` and an `app_mention` for a message that
/// mentions the app when both subscriptions are active, with two distinct
/// `event_id`s. The platform reproduces that, because "why did my bot answer
/// twice" is the single most common Slack-app bug and a reference host should
/// show the guard rather than paper over the cause.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Channel traffic. Includes the app's own posts, which carry `bot_id`.
    Message(MessageEvent),
    /// A message whose text contains `<@BOT_USER_ID>`.
    AppMention(AppMentionEvent),
}

impl Event {
    /// The channel the event happened in — the bot's session key.
    pub fn channel(&self) -> &str {
        match self {
            Event::Message(event) => &event.channel,
            Event::AppMention(event) => &event.channel,
        }
    }

    /// The originating message's `ts`, i.e. its identity in the channel.
    pub fn ts(&self) -> &str {
        match self {
            Event::Message(event) => &event.ts,
            Event::AppMention(event) => &event.ts,
        }
    }

    /// Thread parent, when this event is a reply rather than channel traffic.
    pub fn thread_ts(&self) -> Option<&str> {
        match self {
            Event::Message(event) => event.thread_ts.as_deref(),
            Event::AppMention(event) => event.thread_ts.as_deref(),
        }
    }

    /// The message text as authored, with `<@U…>` mention syntax intact.
    pub fn text(&self) -> &str {
        match self {
            Event::Message(event) => &event.text,
            Event::AppMention(event) => &event.text,
        }
    }

    /// Authoring user, absent when an app authored the message.
    pub fn user(&self) -> Option<&str> {
        match self {
            Event::Message(event) => event.user.as_deref(),
            Event::AppMention(event) => Some(&event.user),
        }
    }

    /// Authoring app's bot id. `Some` means "do not react to this".
    pub fn bot_id(&self) -> Option<&str> {
        match self {
            Event::Message(event) => event.bot_id.as_deref(),
            Event::AppMention(_) => None,
        }
    }
}

/// The `message` event body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageEvent {
    pub channel: String,
    /// Absent on app-authored messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Present on app-authored messages, alongside `subtype: "bot_message"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    pub text: String,
    pub ts: String,
    /// `"channel"` for public channels. The platform only has those.
    pub channel_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    pub event_ts: String,
}

/// The `app_mention` event body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppMentionEvent {
    /// Always a human on this platform: apps cannot mention apps here.
    pub user: String,
    pub text: String,
    pub ts: String,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    pub event_ts: String,
}

/// Slack's `<@U012AB3CD>` mention token for a user id.
pub fn mention_token(user_id: &str) -> String {
    format!("<@{user_id}>")
}

/// Whether `text` mentions `user_id` using Slack's `<@U…>` syntax.
///
/// Slack mention syntax also permits a label suffix (`<@U012AB3CD|name>`), so
/// matching on the bare token alone would miss real mentions.
pub fn mentions(text: &str, user_id: &str) -> bool {
    let opener = format!("<@{user_id}");
    text.match_indices(&opener).any(|(offset, matched)| {
        matches!(
            text[offset + matched.len()..].chars().next(),
            Some('>') | Some('|')
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_detection_accepts_slacks_labelled_form() {
        assert!(mentions("hey <@U0LAN0Z89> what's up", "U0LAN0Z89"));
        assert!(mentions("hey <@U0LAN0Z89|bot> hi", "U0LAN0Z89"));
        assert!(!mentions("hey <@U0LAN0Z8> hi", "U0LAN0Z89"));
        assert!(!mentions("no mention here", "U0LAN0Z89"));
    }

    #[test]
    fn the_envelope_discriminates_the_handshake_from_a_real_event() {
        let handshake: EventRequest = serde_json::from_value(serde_json::json!({
            "token": "Jhj5dZrVaK7ZwHHjRyZWjbDl",
            "challenge": "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P",
            "type": "url_verification"
        }))
        .expect("handshake decodes");
        let EventRequest::UrlVerification(handshake) = handshake else {
            panic!("expected a url_verification request");
        };
        assert_eq!(
            handshake.challenge,
            "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"
        );
    }

    #[test]
    fn an_app_mention_envelope_decodes_slacks_documented_payload() {
        // Slack's own app_mention reference payload, verbatim.
        let request: EventRequest = serde_json::from_value(serde_json::json!({
            "token": "ZZZZZZWSxiZZZ2yIvs3peJ",
            "team_id": "T123ABC456",
            "api_app_id": "A123ABC456",
            "event": {
                "type": "app_mention",
                "user": "U123ABC456",
                "text": "What is the hour of the pearl, <@U0LAN0Z89>?",
                "ts": "1515449522.000016",
                "channel": "C123ABC456",
                "event_ts": "1515449522000016"
            },
            "type": "event_callback",
            "event_id": "Ev123ABC456",
            "event_time": 1515449522,
            "authorizations": [{
                "team_id": "T123ABC456",
                "user_id": "U123ABC456",
                "is_bot": false,
                "is_enterprise_install": false
            }]
        }))
        .expect("app_mention envelope decodes");
        let EventRequest::EventCallback(callback) = request else {
            panic!("expected an event_callback");
        };
        assert_eq!(callback.event_id, "Ev123ABC456");
        assert_eq!(callback.event.channel(), "C123ABC456");
        assert_eq!(callback.event.ts(), "1515449522.000016");
        assert!(mentions(callback.event.text(), "U0LAN0Z89"));
    }
}
