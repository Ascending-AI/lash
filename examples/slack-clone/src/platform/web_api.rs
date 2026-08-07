//! The Slack-compatible Web API.
//!
//! Every handler here is named after the Slack method it implements and answers
//! with the field names Slack answers with. Divergences are called out in
//! comments and collected in the README; silent divergence would make the
//! example worse than useless, because the migration it advertises would fail
//! in production rather than here.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;

use super::args::{ApiError, SlackArgs, flag, limit};
use super::db::{self, Author, MessageRow, TsWindow};
use super::state::PlatformState;
use crate::ids::Ts;
use crate::wire::methods::{
    AuthTestResponse, ChannelObject, ChannelText, ChatPostMessageArgs, ChatPostMessageResponse,
    ConversationsHistoryArgs, ConversationsHistoryResponse, ConversationsListArgs,
    ConversationsListResponse, ConversationsRepliesArgs, ConversationsRepliesResponse,
    MessageMetadata, MessageObject, UserObject, UserProfile, UsersListArgs, UsersListResponse,
};
use crate::wire::{ResponseMetadata, cursor};

/// Slack's default and maximum `limit` for `conversations.history`.
const HISTORY_DEFAULT_LIMIT: usize = 100;
const HISTORY_MAX_LIMIT: usize = 999;
/// Slack's default and maximum `limit` for the list methods.
const LIST_DEFAULT_LIMIT: usize = 100;
const LIST_MAX_LIMIT: usize = 1_000;

/// `auth.test` — how an app discovers its own identity.
///
/// The bot calls this at boot to learn the `U…` id it must look for in `<@…>`
/// mention syntax. Hardcoding that id instead is the single most common reason a
/// Slack bot silently stops answering after a reinstall.
pub async fn auth_test(
    State(state): State<PlatformState>,
    headers: HeaderMap,
) -> Result<Json<AuthTestResponse>, ApiError> {
    state.authorize(&headers)?;
    let identity = state.identity();
    Ok(Json(AuthTestResponse {
        ok: true,
        url: format!("http://{}/", state.config().addr),
        team: identity.team_name.clone(),
        user: identity.bot_handle.clone(),
        team_id: identity.team_id.clone(),
        user_id: identity.bot_user_id.clone(),
        bot_id: identity.bot_id.clone(),
    }))
}

/// `chat.postMessage` — the app's only write.
pub async fn chat_post_message(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    SlackArgs(args): SlackArgs<ChatPostMessageArgs>,
) -> Result<Json<ChatPostMessageResponse>, ApiError> {
    state.authorize(&headers)?;
    let channel = resolve_channel(&state, args.channel.as_deref()).await?;
    let text = args.text.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(ApiError::new("no_text"));
    }
    // Object arguments arrive JSON-encoded, per Slack's form conventions.
    let metadata = match args
        .metadata
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
    {
        Some(raw) => Some(
            serde_json::from_str::<MessageMetadata>(raw)
                .map_err(|_| ApiError::new("invalid_metadata"))?,
        ),
        None => None,
    };
    let thread_ts = match args.thread_ts.as_deref().filter(|raw| !raw.is_empty()) {
        Some(raw) => Some(Ts::parse(raw).ok_or_else(|| ApiError::new("invalid_thread_ts"))?),
        None => None,
    };
    let identity = state.identity();
    let username = args
        .username
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| identity.bot_handle.clone());
    let metadata_json = metadata
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| ApiError::internal("encode metadata", error))?;

    let stored = state
        .post_message(
            channel.id.clone(),
            Author::App {
                bot_id: identity.bot_id.clone(),
                username: username.clone(),
            },
            text,
            thread_ts,
            metadata_json,
        )
        .await
        .map_err(|error| map_write_error("chat.postMessage", error))?;

    Ok(Json(ChatPostMessageResponse {
        ok: true,
        channel: channel.id,
        ts: stored.ts.to_string(),
        message: message_object(&stored, false),
    }))
}

/// `conversations.list`.
pub async fn conversations_list(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    SlackArgs(args): SlackArgs<ConversationsListArgs>,
) -> Result<Json<ConversationsListResponse>, ApiError> {
    state.authorize(&headers)?;
    let exclude_archived = flag(args.exclude_archived.as_ref());
    let page_size = limit(args.limit.as_ref(), LIST_DEFAULT_LIMIT, LIST_MAX_LIMIT);
    let after = decode_id_cursor(cursor::TEAM, args.cursor.as_deref())?;
    let (rows, members) = state
        .database()
        .call(move |connection| {
            Ok((
                db::list_channels(connection, exclude_archived)?,
                db::member_count(connection)?,
            ))
        })
        .await
        .map_err(|error| ApiError::internal("list channels", error))?;

    let creator = state.identity().bot_user_id.clone();
    let (page, next) = paginate(rows, after.as_deref(), page_size, |row| row.id.clone());
    Ok(Json(ConversationsListResponse {
        ok: true,
        channels: page
            .into_iter()
            .map(|row| ChannelObject {
                name_normalized: row.name.clone(),
                id: row.id,
                name: row.name,
                is_channel: true,
                is_group: false,
                is_im: false,
                created: row.created_at,
                creator: if row.creator.is_empty() {
                    creator.clone()
                } else {
                    row.creator
                },
                is_archived: row.is_archived,
                is_general: row.is_general,
                // Every member is in every channel here: the platform has no
                // per-channel membership, so this is honestly `true` rather
                // than a guess.
                is_member: true,
                is_private: false,
                is_mpim: false,
                topic: ChannelText {
                    value: row.topic,
                    ..ChannelText::default()
                },
                purpose: ChannelText {
                    value: row.purpose,
                    ..ChannelText::default()
                },
                num_members: members,
            })
            .collect(),
        response_metadata: next.map(|id| ResponseMetadata {
            next_cursor: cursor::encode(cursor::TEAM, &id),
        }),
    }))
}

/// `users.list`.
pub async fn users_list(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    SlackArgs(args): SlackArgs<UsersListArgs>,
) -> Result<Json<UsersListResponse>, ApiError> {
    state.authorize(&headers)?;
    let page_size = limit(args.limit.as_ref(), LIST_DEFAULT_LIMIT, LIST_MAX_LIMIT);
    let after = decode_id_cursor(cursor::USER, args.cursor.as_deref())?;
    let rows = state
        .database()
        .call(|connection| db::list_users(connection))
        .await
        .map_err(|error| ApiError::internal("list users", error))?;
    let team_id = state.identity().team_id.clone();
    let (page, next) = paginate(rows, after.as_deref(), page_size, |row| row.id.clone());
    Ok(Json(UsersListResponse {
        ok: true,
        members: page
            .into_iter()
            .map(|row| UserObject {
                profile: UserProfile {
                    real_name: row.display_name.clone(),
                    display_name: row.display_name.clone(),
                    real_name_normalized: row.display_name.clone(),
                    display_name_normalized: row.display_name.clone(),
                    team: team_id.clone(),
                },
                id: row.id,
                team_id: team_id.clone(),
                name: row.handle,
                deleted: false,
                // Slack derives a per-user avatar colour; a fixed one keeps the
                // field present without inventing a palette.
                color: "9f69e7".to_string(),
                real_name: row.display_name,
                tz: "Etc/UTC".to_string(),
                tz_label: "Coordinated Universal Time".to_string(),
                tz_offset: 0,
                is_admin: false,
                is_owner: false,
                is_primary_owner: false,
                is_restricted: false,
                is_ultra_restricted: false,
                is_bot: row.is_bot,
                updated: row.created_at,
                is_app_user: row.is_bot,
            })
            .collect(),
        cache_ts: Ts::now().epoch_seconds(),
        response_metadata: next.map(|id| ResponseMetadata {
            next_cursor: cursor::encode(cursor::USER, &id),
        }),
    }))
}

/// `conversations.history` — newest-first top-level messages.
pub async fn conversations_history(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    SlackArgs(args): SlackArgs<ConversationsHistoryArgs>,
) -> Result<Json<ConversationsHistoryResponse>, ApiError> {
    state.authorize(&headers)?;
    let channel = resolve_channel(&state, args.channel.as_deref()).await?;
    let page_size = limit(
        args.limit.as_ref(),
        HISTORY_DEFAULT_LIMIT,
        HISTORY_MAX_LIMIT,
    );
    let include_metadata = flag(args.include_all_metadata.as_ref());
    let mut window = TsWindow {
        oldest: parse_bound(args.oldest.as_deref(), "invalid_ts_oldest")?,
        latest: parse_bound(args.latest.as_deref(), "invalid_ts_latest")?,
        inclusive: flag(args.inclusive.as_ref()),
    };
    // A cursor continues strictly older than the previous page's oldest row, so
    // it overrides `latest` and suppresses `inclusive` on that bound.
    if let Some(raw) = args.cursor.as_deref().filter(|raw| !raw.is_empty()) {
        window.latest =
            Some(cursor::decode_ts(raw).ok_or_else(|| ApiError::new("invalid_cursor"))?);
        window.inclusive = false;
    }

    let channel_id = channel.id.clone();
    let rows = state
        .database()
        .call(move |connection| db::channel_history(connection, &channel_id, window, page_size + 1))
        .await
        .map_err(|error| ApiError::internal("read channel history", error))?;

    let has_more = rows.len() > page_size;
    let page: Vec<MessageRow> = rows.into_iter().take(page_size).collect();
    let next_cursor = has_more
        .then(|| page.last().map(|row| cursor::encode_ts(row.ts)))
        .flatten();
    Ok(Json(ConversationsHistoryResponse {
        ok: true,
        messages: page
            .iter()
            .map(|row| message_object(row, include_metadata))
            .collect(),
        has_more,
        // The platform has no pins. The field is part of the response contract,
        // so it is reported honestly as zero rather than omitted.
        pin_count: 0,
        response_metadata: next_cursor.map(|next_cursor| ResponseMetadata { next_cursor }),
    }))
}

/// `conversations.replies` — thread parent followed by its replies, oldest-first.
///
/// Implemented to the full wire shape, including thread statistics on the
/// parent, even though the platform UI does not render threads. A bot that grows
/// thread support later should not have to wait on the product's UI.
pub async fn conversations_replies(
    State(state): State<PlatformState>,
    headers: HeaderMap,
    SlackArgs(args): SlackArgs<ConversationsRepliesArgs>,
) -> Result<Json<ConversationsRepliesResponse>, ApiError> {
    state.authorize(&headers)?;
    let channel = resolve_channel(&state, args.channel.as_deref()).await?;
    let parent_ts = args
        .ts
        .as_deref()
        .filter(|raw| !raw.is_empty())
        .and_then(Ts::parse)
        .ok_or_else(|| ApiError::new("invalid_arguments"))?;
    let page_size = limit(args.limit.as_ref(), HISTORY_MAX_LIMIT, HISTORY_MAX_LIMIT);
    let include_metadata = flag(args.include_all_metadata.as_ref());
    let mut window = TsWindow {
        oldest: parse_bound(args.oldest.as_deref(), "invalid_ts_oldest")?,
        latest: parse_bound(args.latest.as_deref(), "invalid_ts_latest")?,
        inclusive: flag(args.inclusive.as_ref()),
    };
    // Replies page forward, so a cursor overrides `oldest` and is inclusive of
    // the row it names.
    let paging = args.cursor.as_deref().filter(|raw| !raw.is_empty());
    if let Some(raw) = paging {
        window.oldest =
            Some(cursor::decode_ts(raw).ok_or_else(|| ApiError::new("invalid_cursor"))?);
        window.inclusive = true;
    }

    let channel_id = channel.id.clone();
    let (parent, replies, summary) = state
        .database()
        .call(move |connection| {
            let parent = db::message_by_ts(connection, &channel_id, parent_ts)?;
            let replies =
                db::thread_replies(connection, &channel_id, parent_ts, window, page_size + 1)?;
            let summary = db::thread_summary(connection, &channel_id, parent_ts)?;
            Ok((parent, replies, summary))
        })
        .await
        .map_err(|error| ApiError::internal("read thread replies", error))?;
    let Some(parent) = parent else {
        return Err(ApiError::new("thread_not_found"));
    };

    let has_more = replies.len() > page_size;
    let next_cursor = has_more
        .then(|| replies.get(page_size).map(|row| cursor::encode_ts(row.ts)))
        .flatten();
    let mut messages = Vec::with_capacity(page_size + 1);
    if paging.is_none() {
        let mut parent_object = message_object(&parent, include_metadata);
        parent_object.thread_ts = Some(parent.ts.to_string());
        parent_object.reply_count = Some(summary.reply_count);
        parent_object.reply_users_count = Some(summary.reply_users_count);
        parent_object.latest_reply = summary.latest_reply.map(|ts| ts.to_string());
        messages.push(parent_object);
    }
    let parent_author = match &parent.author {
        Author::User { user_id } => Some(user_id.clone()),
        Author::App { .. } => None,
    };
    messages.extend(replies.iter().take(page_size).map(|row| {
        let mut object = message_object(row, include_metadata);
        object.thread_ts = Some(parent.ts.to_string());
        object.parent_user_id = parent_author.clone();
        object
    }));

    Ok(Json(ConversationsRepliesResponse {
        ok: true,
        messages,
        has_more,
        response_metadata: next_cursor.map(|next_cursor| ResponseMetadata { next_cursor }),
    }))
}

/// Project a stored row onto the wire `message` object.
pub(crate) fn message_object(row: &MessageRow, include_metadata: bool) -> MessageObject {
    let mut object = match &row.author {
        Author::User { user_id } => {
            MessageObject::from_user(user_id, &row.text, row.ts.to_string())
        }
        Author::App { bot_id, username } => {
            MessageObject::from_bot(bot_id, username, &row.text, row.ts.to_string())
        }
    };
    object.thread_ts = row.thread_ts.map(|ts| ts.to_string());
    if include_metadata {
        object.metadata = row
            .metadata_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok());
    }
    object
}

/// Resolve Slack's `channel` argument, which accepts an id or a name.
async fn resolve_channel(
    state: &PlatformState,
    raw: Option<&str>,
) -> Result<db::ChannelRow, ApiError> {
    let raw = raw.map(str::trim).filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return Err(ApiError::new("channel_not_found"));
    };
    let lookup = raw.trim_start_matches('#').to_string();
    let by_id = raw.to_string();
    let found = state
        .database()
        .call(move |connection| {
            if let Some(row) = db::channel_by_id(connection, &by_id)? {
                return Ok(Some(row));
            }
            db::channel_by_name(connection, &lookup)
        })
        .await
        .map_err(|error| ApiError::internal("resolve channel", error))?;
    found.ok_or_else(|| ApiError::new("channel_not_found"))
}

/// Parse an `oldest`/`latest` bound.
fn parse_bound(raw: Option<&str>, error: &str) -> Result<Option<Ts>, ApiError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Ts::parse(value)
            .map(Some)
            .ok_or_else(|| ApiError::new(error)),
        None => Ok(None),
    }
}

/// Decode an id-keyed cursor (`team:` / `user:`).
fn decode_id_cursor(kind: &str, raw: Option<&str>) -> Result<Option<String>, ApiError> {
    match raw.filter(|value| !value.is_empty()) {
        Some(value) => cursor::decode(kind, value)
            .map(Some)
            .ok_or_else(|| ApiError::new("invalid_cursor")),
        None => Ok(None),
    }
}

/// Page an id-ordered list, returning the page and the id to resume after.
fn paginate<T, F>(
    rows: Vec<T>,
    after: Option<&str>,
    page_size: usize,
    key: F,
) -> (Vec<T>, Option<String>)
where
    F: Fn(&T) -> String,
{
    let start = after
        .map(|after| {
            rows.iter()
                .position(|row| key(row).as_str() > after)
                .unwrap_or(rows.len())
        })
        .unwrap_or(0);
    let remaining: Vec<T> = rows.into_iter().skip(start).collect();
    let has_more = remaining.len() > page_size;
    let page: Vec<T> = remaining.into_iter().take(page_size).collect();
    let next = has_more.then(|| page.last().map(&key)).flatten();
    (page, next)
}

/// Translate a write failure into the Slack error code a client expects.
fn map_write_error(context: &str, error: anyhow::Error) -> ApiError {
    match error.to_string().as_str() {
        "channel_not_found" => ApiError::new("channel_not_found"),
        "thread_not_found" => ApiError::new("thread_not_found"),
        _ => ApiError::internal(context, error),
    }
}
