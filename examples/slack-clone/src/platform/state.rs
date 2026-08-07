//! Shared platform state: workspace identity, the store handle, the live UI
//! broadcast, and the one write path every message goes through.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::http::HeaderMap;
use serde::Serialize;
use tokio::sync::{Notify, broadcast};

use super::apps;
use super::args::ApiError;
use super::db::{self, Author, MessageRow};
use super::{PlatformConfig, ui};
use crate::ids::{IdMinter, Ts};
use crate::store::SqliteHandle;
use crate::wire::events::{
    self, Authorization, Event, EventCallback, EventRequest, MessageEvent, UrlVerification,
};

/// Capacity of the live UI broadcast. A slow browser tab is dropped rather than
/// stalling the writer; the tab reconnects and re-reads history.
const LIVE_BUFFER: usize = 256;

/// The workspace's stable identity, resolved once at boot.
#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceIdentity {
    pub team_id: String,
    pub team_name: String,
    /// The installed app (`A…`).
    pub app_id: String,
    /// The app's bot id (`B…`), stamped on messages it posts.
    pub bot_id: String,
    /// The app's bot *user* id (`U…`) — the `<@…>` mention target.
    pub bot_user_id: String,
    pub bot_handle: String,
}

/// One frame of the live UI stream. Not a Slack shape: this is the product's
/// own websocket-substitute, so it carries display names the UI needs and no
/// Slack envelope at all.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// Sent once when a stream opens, so a client can tell "connected" from
    /// "connected but quiet".
    Hello { channel: String },
    /// A new message landed in a channel.
    Message {
        channel: String,
        ts: String,
        author_id: String,
        author_name: String,
        is_bot: bool,
        text: String,
        thread_ts: Option<String>,
    },
    /// A channel was created.
    ChannelCreated { channel: String, name: String },
}

/// Everything a platform handler needs.
#[derive(Clone)]
pub struct PlatformState {
    config: Arc<PlatformConfig>,
    database: SqliteHandle,
    ids: Arc<IdMinter>,
    identity: Arc<WorkspaceIdentity>,
    live: broadcast::Sender<LiveEvent>,
    /// Woken whenever the outbox gains work, so delivery is prompt without the
    /// dispatcher polling tightly.
    delivery: Arc<Notify>,
    http: reqwest::Client,
}

impl PlatformState {
    /// Open a workspace, seeding identity, default channels and the app install
    /// on first boot and reusing them on every boot after.
    pub async fn seed(config: PlatformConfig, database: SqliteHandle) -> Result<Self> {
        let ids = Arc::new(IdMinter::new());
        let identity = {
            let ids = Arc::clone(&ids);
            let bot_handle = config.bot_handle.clone();
            let team_name = config.team_name.clone();
            database
                .call(move |connection| {
                    let team_id = db::ensure_workspace(connection, &ids.mint("T"), &team_name)?;
                    let bot_user = db::upsert_user(
                        connection,
                        &ids.mint("U"),
                        &bot_handle,
                        &bot_handle,
                        true,
                    )?;
                    let app =
                        apps::ensure_app(connection, &ids.mint("A"), &ids.mint("B"), &bot_user.id)?;
                    // Slack workspaces always have a #general; a second channel
                    // makes session-per-channel isolation visible in the UI.
                    db::upsert_channel(connection, &ids.mint("C"), "general", &bot_user.id, true)?;
                    db::upsert_channel(connection, &ids.mint("C"), "random", &bot_user.id, false)?;
                    Ok(WorkspaceIdentity {
                        team_id,
                        team_name: db::team_name(connection)?,
                        app_id: app.id,
                        bot_id: app.bot_id,
                        bot_user_id: app.bot_user_id,
                        bot_handle,
                    })
                })
                .await
                .context("seed workspace")?
        };
        let (live, _) = broadcast::channel(LIVE_BUFFER);
        Ok(Self {
            config: Arc::new(config),
            database,
            ids,
            identity: Arc::new(identity),
            live,
            delivery: Arc::new(Notify::new()),
            http: reqwest::Client::new(),
        })
    }

    /// Configuration.
    pub fn config(&self) -> &PlatformConfig {
        &self.config
    }

    /// Workspace identity.
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    /// The store handle.
    pub fn database(&self) -> &SqliteHandle {
        &self.database
    }

    /// The id minter.
    pub fn ids(&self) -> &IdMinter {
        &self.ids
    }

    /// HTTP client used for event delivery and the verification handshake.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Signal that the outbox has work.
    pub fn notify_delivery(&self) {
        self.delivery.notify_one();
    }

    /// Wait for outbox work, bounded so a restart still drains a backlog.
    pub async fn await_delivery_work(&self, timeout: std::time::Duration) {
        let _ = tokio::time::timeout(timeout, self.delivery.notified()).await;
    }

    /// Subscribe to the live UI stream.
    pub fn subscribe_live(&self) -> broadcast::Receiver<LiveEvent> {
        self.live.subscribe()
    }

    /// Publish a live UI frame. A send with no subscribers is not an error.
    pub fn publish_live(&self, event: LiveEvent) {
        let _ = self.live.send(event);
    }

    /// Enforce `Authorization: Bearer <bot token>`.
    ///
    /// Slack also accepts the token as a `token` argument. The platform requires
    /// the header, which is the only form current Slack SDKs use and the only
    /// form that keeps credentials out of query strings and access logs.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim);
        match presented {
            Some(token) if token == self.config.bot_token => Ok(()),
            Some(_) => Err(ApiError::new("invalid_auth")),
            None => Err(ApiError::new("not_authed")),
        }
    }

    /// Append a message and fan the resulting events out.
    ///
    /// This is the platform's single write path, and the ordering matters: the
    /// message is durable *before* any event is queued, so the bot can never be
    /// told about a message that a crash would have unwritten.
    pub async fn post_message(
        &self,
        channel_id: String,
        author: Author,
        text: String,
        thread_ts: Option<Ts>,
        metadata_json: Option<String>,
    ) -> Result<MessageRow> {
        let stored = {
            let channel_id = channel_id.clone();
            let author = author.clone();
            let text = text.clone();
            self.database
                .call(move |connection| {
                    db::append_message(
                        connection,
                        &channel_id,
                        author,
                        &text,
                        thread_ts,
                        metadata_json.as_deref(),
                    )
                })
                .await?
        };
        self.fan_out(&stored).await?;
        Ok(stored)
    }

    /// Queue the Events API deliveries for a stored message and publish the
    /// live UI frame.
    async fn fan_out(&self, stored: &MessageRow) -> Result<()> {
        let (author_id, author_name, is_bot) = self.describe_author(&stored.author).await?;
        self.publish_live(LiveEvent::Message {
            channel: stored.channel_id.clone(),
            ts: stored.ts.to_string(),
            author_id: author_id.clone(),
            author_name,
            is_bot,
            text: stored.text.clone(),
            thread_ts: stored.thread_ts.map(|ts| ts.to_string()),
        });

        let mut envelopes = Vec::new();
        // Slack delivers a `message` event for *every* message in a subscribed
        // channel, including an app's own posts (which carry `bot_id`). The
        // platform does the same rather than filtering here, so the bot has to
        // own the self-message guard — the real integration hazard.
        envelopes.push(self.envelope(Event::Message(MessageEvent {
            channel: stored.channel_id.clone(),
            user: match &stored.author {
                Author::User { user_id } => Some(user_id.clone()),
                Author::App { .. } => None,
            },
            bot_id: match &stored.author {
                Author::App { bot_id, .. } => Some(bot_id.clone()),
                Author::User { .. } => None,
            },
            subtype: matches!(stored.author, Author::App { .. }).then(|| "bot_message".to_string()),
            text: stored.text.clone(),
            ts: stored.ts.to_string(),
            channel_type: "channel".to_string(),
            thread_ts: stored.thread_ts.map(|ts| ts.to_string()),
            event_ts: stored.ts.to_string(),
        })));
        // …and, when both subscriptions are active, a *second* event with its
        // own `event_id` for a mention. Reproduced deliberately: "my bot
        // answered twice" is the classic consequence, and the guard belongs in
        // the reference bot.
        if let Author::User { user_id } = &stored.author
            && events::mentions(&stored.text, &self.identity.bot_user_id)
        {
            envelopes.push(self.envelope(Event::AppMention(events::AppMentionEvent {
                user: user_id.clone(),
                text: stored.text.clone(),
                ts: stored.ts.to_string(),
                channel: stored.channel_id.clone(),
                thread_ts: stored.thread_ts.map(|ts| ts.to_string()),
                event_ts: stored.ts.to_string(),
            })));
        }

        let app_id = self.identity.app_id.clone();
        let queued = envelopes
            .into_iter()
            .map(|envelope| {
                let payload =
                    serde_json::to_string(&EventRequest::EventCallback(Box::new(envelope.clone())))
                        .context("encode event envelope")?;
                Ok((envelope.event_id, payload))
            })
            .collect::<Result<Vec<_>>>()?;
        self.database
            .call(move |connection| {
                for (event_id, payload) in &queued {
                    apps::enqueue_event(connection, &app_id, event_id, payload)?;
                }
                Ok(())
            })
            .await
            .context("queue events")?;
        self.notify_delivery();
        Ok(())
    }

    /// Wrap an event body in the callback envelope.
    fn envelope(&self, event: Event) -> EventCallback {
        EventCallback {
            token: self.config.verification_token.clone(),
            team_id: self.identity.team_id.clone(),
            api_app_id: self.identity.app_id.clone(),
            event,
            event_id: self.ids.mint("Ev"),
            event_time: Ts::now().epoch_seconds(),
            authorizations: vec![Authorization {
                team_id: self.identity.team_id.clone(),
                user_id: self.identity.bot_user_id.clone(),
                is_bot: true,
                is_enterprise_install: false,
            }],
        }
    }

    /// Resolve an author to `(id, display name, is_bot)` for the UI stream.
    async fn describe_author(&self, author: &Author) -> Result<(String, String, bool)> {
        match author {
            Author::User { user_id } => {
                let lookup = user_id.clone();
                let user = self
                    .database
                    .call(move |connection| db::user_by_id(connection, &lookup))
                    .await?;
                let name = user
                    .map(|user| user.display_name)
                    .unwrap_or_else(|| user_id.clone());
                Ok((user_id.clone(), name, false))
            }
            Author::App { bot_id, username } => Ok((bot_id.clone(), username.clone(), true)),
        }
    }

    /// Perform Slack's `url_verification` handshake against a candidate request
    /// URL and, on success, record it.
    ///
    /// Verification is synchronous and gating: an unverified URL never receives
    /// events, which is exactly how Slack behaves when you paste a URL its
    /// challenge cannot reach.
    pub async fn verify_and_register(&self, request_url: &str) -> Result<()> {
        let challenge = self.ids.mint("Chal");
        let body = EventRequest::UrlVerification(UrlVerification {
            token: self.config.verification_token.clone(),
            challenge: challenge.clone(),
        });
        let response = self
            .http
            .post(request_url)
            .timeout(self.config.delivery_timeout)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("send url_verification to {request_url}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("url_verification returned HTTP {status}: {text}");
        }
        // Slack accepts either a plaintext echo or `{"challenge": "..."}`.
        let echoed = serde_json::from_str::<events::ChallengeResponse>(&text)
            .map(|body| body.challenge)
            .unwrap_or_else(|_| text.trim().to_string());
        if echoed != challenge {
            anyhow::bail!("url_verification challenge mismatch (got `{echoed}`)");
        }
        let app_id = self.identity.app_id.clone();
        let request_url = request_url.to_string();
        self.database
            .call(move |connection| apps::set_request_url(connection, &app_id, &request_url))
            .await
            .context("record verified request url")
    }

    /// The UI document, so the router can stay declarative.
    pub fn index_html(&self) -> &'static str {
        ui::INDEX_HTML
    }
}
