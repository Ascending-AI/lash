//! The platform's own product surface: identity, channels, posting, live stream.
//!
//! None of this is Slack. It is the product's private API, kept under
//! `/platform/*` so the Slack-compatible half stays unambiguous. Identity is a
//! name picker with no authentication at all — enough for several browser tabs to
//! be several people, and clearly not enough for anything else.

use std::convert::Infallible;

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use super::args::ApiError;
use super::db::{self, Author};
use super::state::{LiveEvent, PlatformState};
use super::{apps, web_api};
use crate::ids::Ts;
use crate::log_err;

/// Liveness, workspace identity and delivery counters.
pub async fn healthz(
    State(state): State<PlatformState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stats = state
        .database()
        .call(|connection| {
            Ok((
                apps::delivery_stats(connection)?,
                apps::installed_app(connection)?,
            ))
        })
        .await
        .map_err(|error| ApiError::internal("read health", error))?;
    let (delivery, app) = stats;
    Ok(Json(json!({
        "service": "slack-clone-platform",
        "identity": state.identity(),
        "events_request_url": app.as_ref().and_then(|app| app.request_url.clone()),
        "events_verified": app.is_some_and(|app| app.verified_at.is_some()),
        "delivery": delivery,
    })))
}

/// Register the app's Events API request URL.
#[derive(Debug, Deserialize)]
pub struct RegisterAppRequest {
    pub request_url: String,
}

/// `POST /platform/apps` — verify and store an Events API request URL.
pub async fn register_app(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    Json(request): Json<RegisterAppRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.authorize(&headers)?;
    state
        .verify_and_register(&request.request_url)
        .await
        .map_err(|error| {
            // Verification failure is the caller's problem, not an internal one:
            // it means the URL did not echo the challenge.
            log_err!("slack-clone-platform url_verification failed: {error:#}");
            ApiError::new("request_url_verification_failed")
        })?;
    let identity = state.identity();
    Ok(Json(json!({
        "ok": true,
        "app_id": identity.app_id,
        "bot_id": identity.bot_id,
        "bot_user_id": identity.bot_user_id,
        "request_url": request.request_url,
        "verified": true,
    })))
}

/// Claim a display name.
#[derive(Debug, Deserialize)]
pub struct IdentifyRequest {
    pub name: String,
}

/// The identity a browser tab is acting as.
#[derive(Debug, Serialize)]
pub struct IdentifyResponse {
    pub user_id: String,
    pub handle: String,
    pub display_name: String,
}

/// `POST /platform/identify` — claim or reclaim a name.
///
/// Reclaiming is deliberate: reloading a tab must land back on the same `U…`, or
/// the workspace fills up with ghosts of the same person and the bot's mention
/// resolution starts naming strangers.
pub async fn identify(
    State(state): State<PlatformState>,
    Json(request): Json<IdentifyRequest>,
) -> Result<Json<IdentifyResponse>, ApiError> {
    let display_name = request.name.trim().to_string();
    if display_name.is_empty() {
        return Err(ApiError::new("name_required"));
    }
    let handle = normalize_handle(&display_name);
    if handle.is_empty() {
        return Err(ApiError::new("name_required"));
    }
    if handle == state.identity().bot_handle {
        return Err(ApiError::new("name_taken"));
    }
    let id = state.ids().mint("U");
    let user = state
        .database()
        .call(move |connection| db::upsert_user(connection, &id, &handle, &display_name, false))
        .await
        .map_err(|error| ApiError::internal("claim identity", error))?;
    Ok(Json(IdentifyResponse {
        user_id: user.id,
        handle: user.handle,
        display_name: user.display_name,
    }))
}

/// Everything the UI needs on load.
pub async fn bootstrap(
    State(state): State<PlatformState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (channels, users) = state
        .database()
        .call(|connection| {
            Ok((
                db::list_channels(connection, true)?,
                db::list_users(connection)?,
            ))
        })
        .await
        .map_err(|error| ApiError::internal("bootstrap", error))?;
    Ok(Json(json!({
        "identity": state.identity(),
        "channels": channels
            .into_iter()
            .map(|channel| json!({ "id": channel.id, "name": channel.name }))
            .collect::<Vec<_>>(),
        "users": users
            .into_iter()
            .map(|user| json!({
                "id": user.id,
                "display_name": user.display_name,
                "is_bot": user.is_bot,
            }))
            .collect::<Vec<_>>(),
    })))
}

/// Create a channel.
#[derive(Debug, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    pub user_id: String,
}

/// `POST /platform/channels`.
pub async fn create_channel(
    State(state): State<PlatformState>,
    Json(request): Json<CreateChannelRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = normalize_handle(request.name.trim_start_matches('#'));
    if name.is_empty() {
        return Err(ApiError::new("invalid_name_specials"));
    }
    let id = state.ids().mint("C");
    let creator = request.user_id;
    let channel = state
        .database()
        .call(move |connection| db::upsert_channel(connection, &id, &name, &creator, false))
        .await
        .map_err(|error| ApiError::internal("create channel", error))?;
    state.publish_live(LiveEvent::ChannelCreated {
        channel: channel.id.clone(),
        name: channel.name.clone(),
    });
    Ok(Json(json!({ "id": channel.id, "name": channel.name })))
}

/// Post as a human.
#[derive(Debug, Deserialize)]
pub struct PostAsUserRequest {
    pub channel: String,
    pub user_id: String,
    pub text: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
}

/// `POST /platform/messages` — the UI's send button.
///
/// Goes through the same [`PlatformState::post_message`] write path as
/// `chat.postMessage`, so a human message and an app message fan out identically.
/// A second write path is how a product ends up with messages the bot never hears
/// about.
pub async fn post_as_user(
    State(state): State<PlatformState>,
    Json(request): Json<PostAsUserRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(ApiError::new("no_text"));
    }
    let user_id = request.user_id.clone();
    let known = state
        .database()
        .call(move |connection| db::user_by_id(connection, &user_id))
        .await
        .map_err(|error| ApiError::internal("resolve author", error))?;
    if known.is_none() {
        return Err(ApiError::new("user_not_found"));
    }
    let thread_ts = match request.thread_ts.as_deref().filter(|raw| !raw.is_empty()) {
        Some(raw) => Some(Ts::parse(raw).ok_or_else(|| ApiError::new("invalid_thread_ts"))?),
        None => None,
    };
    let stored = state
        .post_message(
            request.channel,
            Author::User {
                user_id: request.user_id,
            },
            text,
            thread_ts,
            false,
            None,
        )
        .await
        .map_err(|error| match error.to_string().as_str() {
            "channel_not_found" => ApiError::new("channel_not_found"),
            "thread_not_found" => ApiError::new("thread_not_found"),
            _ => ApiError::internal("post message", error),
        })?;
    Ok(Json(json!({
        "ok": true,
        "ts": stored.ts.to_string(),
        "message": web_api::message_object(&stored, false),
    })))
}

/// Which channel to read.
#[derive(Debug, Deserialize)]
pub struct ChannelQuery {
    pub channel: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
}

/// `GET /platform/history` — the backlog a tab renders on load.
///
/// The UI does **not** read `conversations.history` for this. That method needs
/// the bot token, and shipping a bot token to a browser is how workspaces get
/// taken over. The product's own surface resolves author display names anyway,
/// which the Slack shape deliberately does not.
pub async fn history(
    State(state): State<PlatformState>,
    Query(query): Query<ChannelQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let channel = query.channel;
    let thread_ts = query
        .thread_ts
        .as_deref()
        .filter(|raw| !raw.is_empty())
        .map(|raw| Ts::parse(raw).ok_or_else(|| ApiError::new("invalid_thread_ts")))
        .transpose()?;
    let (rows, users, summaries) = state
        .database()
        .call(move |connection| {
            if db::channel_by_id(connection, &channel)?.is_none() {
                anyhow::bail!("channel_not_found");
            }
            let rows = if let Some(parent_ts) = thread_ts {
                let Some(parent) = db::message_by_ts(connection, &channel, parent_ts)? else {
                    anyhow::bail!("thread_not_found");
                };
                let mut rows = vec![parent];
                rows.extend(db::thread_replies(
                    connection,
                    &channel,
                    parent_ts,
                    db::TsWindow::default(),
                    200,
                )?);
                rows
            } else {
                let mut rows =
                    db::channel_history(connection, &channel, db::TsWindow::default(), 200)?;
                rows.reverse();
                rows
            };
            let summaries = rows
                .iter()
                .filter(|row| row.thread_ts.is_none())
                .map(|row| Ok((row.ts, db::thread_summary(connection, &channel, row.ts)?)))
                .collect::<anyhow::Result<std::collections::HashMap<_, _>>>()?;
            Ok((rows, db::list_users(connection)?, summaries))
        })
        .await
        .map_err(|error| match error.to_string().as_str() {
            "channel_not_found" => ApiError::new("channel_not_found"),
            "thread_not_found" => ApiError::new("thread_not_found"),
            _ => ApiError::internal("read history", error),
        })?;
    let names: std::collections::HashMap<String, String> = users
        .into_iter()
        .map(|user| (user.id, user.display_name))
        .collect();
    let messages = rows
        .into_iter()
        .map(|row| {
            let (author_id, author_name, is_bot) = match &row.author {
                Author::User { user_id } => (
                    user_id.clone(),
                    names
                        .get(user_id)
                        .cloned()
                        .unwrap_or_else(|| user_id.clone()),
                    false,
                ),
                Author::App { bot_id, username } => (bot_id.clone(), username.clone(), true),
            };
            let summary = summaries.get(&row.ts);
            json!({
                "ts": row.ts.to_string(),
                "author_id": author_id,
                "author_name": author_name,
                "is_bot": is_bot,
                "text": row.text,
                "thread_ts": row.thread_ts.map(|ts| ts.to_string()),
                "reply_broadcast": row.reply_broadcast,
                "reply_count": summary.map(|summary| summary.reply_count).unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "messages": messages })))
}

/// `GET /platform/stream` — NDJSON of live channel activity.
///
/// NDJSON rather than SSE, matching the other examples in this repo: one JSON
/// object per line, no framing to get wrong, and `fetch` can read it in a
/// browser without a second protocol.
pub async fn stream(
    State(state): State<PlatformState>,
    Query(query): Query<ChannelQuery>,
) -> Response {
    let channel = query.channel;
    let receiver = state.subscribe_live();
    let hello = LiveEvent::Hello {
        channel: channel.clone(),
    };
    let follow = BroadcastStream::new(receiver).filter_map(move |event| {
        let channel = channel.clone();
        async move {
            match event {
                Ok(event) if event_matches(&event, &channel) => Some(event),
                // A lagged subscriber has missed frames; dropping the marker is
                // right because the client re-reads history on reconnect.
                Ok(_) | Err(_) => None,
            }
        }
    });
    let body = tokio_stream::once(hello).chain(follow).map(|event| {
        let mut line = serde_json::to_string(&event)
            .unwrap_or_else(|_| json!({ "type": "unavailable" }).to_string());
        line.push('\n');
        Ok::<Bytes, Infallible>(Bytes::from(line))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(body))
        .expect("valid streaming response")
}

fn event_matches(event: &LiveEvent, channel: &str) -> bool {
    match event {
        LiveEvent::Message {
            channel: event_channel,
            ..
        } => event_channel == channel,
        // Channel creation is workspace-wide: every open tab needs it to keep its
        // sidebar current, whichever channel it is reading.
        LiveEvent::ChannelCreated { .. } => true,
        LiveEvent::Hello { .. } => false,
    }
}

/// Slack's channel- and handle-name rules: lowercase, no spaces or dots.
fn normalize_handle(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_follow_slacks_naming_rules() {
        assert_eq!(normalize_handle("Ada Lovelace"), "ada-lovelace");
        assert_eq!(normalize_handle("#Product Team!"), "product-team");
        assert_eq!(normalize_handle("  --  "), "");
    }
}
