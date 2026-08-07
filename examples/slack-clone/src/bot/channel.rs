//! Session-per-channel event handling: the part of the example worth copying.
//!
//! Three decisions carry the design.
//!
//! **A channel is a session.** `channel:<C…>` is the Lash session id, so the
//! bot's memory of a room is exactly as long-lived as the room, survives
//! restarts, and never leaks between channels.
//!
//! **Ambient traffic is queued turn input, not a turn.** Messages that do not
//! mention the bot are admitted with [`lash::LashSession::enqueue`] — durable,
//! ordered, model-visible — and no turn runs. When somebody finally does mention
//! the bot, one queued drain folds the accumulated room context *and* the mention
//! into a single turn. The bot has been listening the whole time without saying a
//! word or spending a token.
//!
//! **Deduplication is staged, not boolean.** See [`super::ledger`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use lash::persistence::LeaseOwnerIdentity;
use lash::{LashCore, LashSession, TurnInput};
use tokio::sync::RwLock;

use super::ledger::{Claim, EventLedger, Stage};
use super::runtime::session_id;
use super::slack_api::{ChatPostMessageRequest, SlackApi, find_reply_for_event};
use crate::wire::events::{self, Event, EventCallback};

/// How much channel history the recovery check scans for an already-posted reply.
const RECOVERY_SCAN: u32 = 50;

/// The app's own identity in the workspace, from `auth.test`.
#[derive(Clone, Debug)]
pub struct BotIdentity {
    /// Bot *user* id (`U…`) — what `<@…>` mentions name.
    pub bot_user_id: String,
    /// Bot id (`B…`) — what the app's own messages carry.
    pub bot_id: String,
    pub handle: String,
    pub team_id: String,
}

/// What the bot did with one delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The envelope's verification token did not match.
    Rejected { reason: &'static str },
    /// Already handled to completion; nothing was done.
    Duplicate {
        event_id: String,
        stage: Stage,
        reply_ts: Option<String>,
    },
    /// Deliberately not acted on.
    Ignored {
        event_id: String,
        reason: &'static str,
    },
    /// Folded into the channel session as context. No turn, no reply.
    Folded {
        event_id: String,
        channel: String,
        input_id: String,
    },
    /// A reply was posted. `turn_ran` is false when a recovered reply was posted
    /// from the ledger without re-running the model.
    Replied {
        event_id: String,
        channel: String,
        reply_ts: String,
        turn_ran: bool,
    },
    /// A turn ran but produced no text to post.
    Silent {
        event_id: String,
        channel: String,
        reason: &'static str,
    },
    /// A turn committed in a previous process and its reply text was lost with
    /// that process. Surfaced rather than swallowed; see the README's durability
    /// section for the window and the fix.
    ReplyLost { event_id: String, channel: String },
}

/// The bot.
#[derive(Clone)]
pub struct ChannelBot {
    core: LashCore,
    api: Arc<SlackApi>,
    ledger: EventLedger,
    identity: BotIdentity,
    verification_token: String,
    session_owner: LeaseOwnerIdentity,
    /// One lock per channel. Deliveries for one room are handled in order so an
    /// ambient message cannot interleave with the drain that should have
    /// included it; different rooms stay fully parallel.
    channel_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// `U…` to display name, so `<@U…>` renders as something a model can reason
    /// about. Filled from `users.list` and refreshed on a miss.
    directory: Arc<RwLock<HashMap<String, String>>>,
}

impl ChannelBot {
    /// Assemble a bot over an already-built core.
    pub fn new(
        core: LashCore,
        api: Arc<SlackApi>,
        ledger: EventLedger,
        identity: BotIdentity,
        verification_token: String,
        session_owner: LeaseOwnerIdentity,
    ) -> Self {
        Self {
            core,
            api,
            ledger,
            identity,
            verification_token,
            session_owner,
            channel_locks: Arc::new(Mutex::new(HashMap::new())),
            directory: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The app's identity.
    pub fn identity(&self) -> &BotIdentity {
        &self.identity
    }

    /// The event ledger, for `/healthz` and tests.
    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    /// The core, for the shutdown trace flush.
    pub fn core(&self) -> &LashCore {
        &self.core
    }

    /// Populate the display-name directory from `users.list`.
    pub async fn refresh_directory(&self) -> Result<()> {
        let mut cursor: Option<String> = None;
        let mut names = HashMap::new();
        loop {
            let page = self.api.users_list(cursor.as_deref()).await?;
            for member in page.members {
                let name = if member.profile.display_name.is_empty() {
                    member.name
                } else {
                    member.profile.display_name
                };
                names.insert(member.id, name);
            }
            cursor = page
                .response_metadata
                .map(|metadata| metadata.next_cursor)
                .filter(|next| !next.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        *self.directory.write().await = names;
        Ok(())
    }

    /// Finish anything a previous process left half-done.
    ///
    /// Called once at boot, before the webhook endpoint accepts traffic. The
    /// platform's retries are bounded and its ledger rows make later
    /// redeliveries look handled, so without this pass an event accepted a
    /// moment before a crash would never be finished by anyone.
    pub async fn recover(&self) -> Result<Vec<Disposition>> {
        let unfinished = self.ledger.unfinished().await?;
        let mut outcomes = Vec::with_capacity(unfinished.len());
        for record in unfinished {
            let guard = self.channel_lock(&record.channel_id).await;
            let _held = guard.lock().await;
            let outcome = match record.stage {
                Stage::ReplyPending => {
                    self.settle_reply_debt(&record.event_id, &record.channel_id, record.detail)
                        .await?
                }
                _ => {
                    // Accepted but nothing recorded: the mention's queued input
                    // may or may not have been consumed. `settle_reply_debt`
                    // with no recorded text does exactly the right thing — it
                    // looks for an already-posted reply first.
                    self.settle_reply_debt(&record.event_id, &record.channel_id, None)
                        .await?
                }
            };
            eprintln!(
                "slack-clone-bot recovered event {}: {outcome:?}",
                record.event_id
            );
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// Handle one Events API delivery.
    ///
    /// `retry_num` is the value of `x-slack-retry-num`, recorded for observation
    /// only: correctness must not depend on it, because the first delivery and
    /// the third carry the same `event_id` and must be treated identically.
    pub async fn ingest(
        &self,
        envelope: EventCallback,
        retry_num: Option<u32>,
    ) -> Result<Disposition> {
        if envelope.token != self.verification_token {
            return Ok(Disposition::Rejected {
                reason: "bad_verification_token",
            });
        }
        if let Some(retry) = retry_num {
            eprintln!(
                "slack-clone-bot redelivery {retry} of event {}",
                envelope.event_id
            );
        }

        let channel = envelope.event.channel().to_string();
        let message_ts = envelope.event.ts().to_string();
        let (kind, intent) = self.classify(&envelope.event);
        if let Intent::Ignore(reason) = intent {
            // Recorded before returning, so an ignored event's redeliveries are
            // also cheap and the reason stays auditable.
            let claim = self
                .ledger
                .claim(
                    envelope.event_id.clone(),
                    channel,
                    message_ts,
                    kind.to_string(),
                )
                .await?;
            if let Claim::Settled(record) = claim {
                return Ok(Disposition::Duplicate {
                    event_id: record.event_id,
                    stage: record.stage,
                    reply_ts: record.reply_ts,
                });
            }
            self.ledger
                .advance(
                    envelope.event_id.clone(),
                    Stage::Ignored,
                    None,
                    Some(reason.to_string()),
                )
                .await?;
            return Ok(Disposition::Ignored {
                event_id: envelope.event_id,
                reason,
            });
        }

        let claim = self
            .ledger
            .claim(
                envelope.event_id.clone(),
                channel.clone(),
                message_ts.clone(),
                kind.to_string(),
            )
            .await?;
        if let Claim::Settled(record) = &claim {
            return Ok(Disposition::Duplicate {
                event_id: record.event_id.clone(),
                stage: record.stage,
                reply_ts: record.reply_ts.clone(),
            });
        }

        let guard = self.channel_lock(&channel).await;
        let _held = guard.lock().await;

        // A redelivery of an event that already owes a reply must not run the
        // model again: the text is on record and the only open question is
        // whether it reached the channel.
        if let Claim::Resume(record) = &claim
            && record.stage == Stage::ReplyPending
        {
            return self
                .settle_reply_debt(&record.event_id, &channel, record.detail.clone())
                .await;
        }

        let composed = self.compose(&envelope).await;
        let session = self.open_session(&channel).await?;
        // The source key is derived from the message's `ts`, not the `event_id`:
        // `ts` *is* the message's identity, so a redelivery — or the same message
        // arriving under a second event — resolves to the one admission record
        // Lash already holds instead of a duplicate context line.
        let receipt = session
            .enqueue(TurnInput::text(composed))
            .id(format!(
                "{}:{}:{}",
                intent.source_prefix(),
                channel,
                message_ts
            ))
            .send()
            .await
            .context("admit channel message as queued turn input")?;

        match intent {
            Intent::Ignore(_) => unreachable!("ignored intents return above"),
            Intent::Ambient => {
                self.ledger
                    .advance(envelope.event_id.clone(), Stage::Folded, None, None)
                    .await?;
                Ok(Disposition::Folded {
                    event_id: envelope.event_id,
                    channel,
                    input_id: receipt.input_id,
                })
            }
            Intent::Mention => {
                self.run_mention_turn(&session, &envelope.event_id, &channel)
                    .await
            }
        }
    }

    /// Drain every queued input for the channel into one turn and post the reply.
    async fn run_mention_turn(
        &self,
        session: &LashSession,
        event_id: &str,
        channel: &str,
    ) -> Result<Disposition> {
        // The drain id is stable per event, so the queue-drain effect scope is
        // the same on a redelivery as it was on the first attempt.
        let output = session
            .queued_turn()
            .drain_id(format!("mention:{event_id}"))
            .run()
            .await
            .context("run channel mention turn")?;
        let Some(output) = output else {
            // Nothing was pending: a previous process already drained this
            // mention's input, committed the turn, and died before recording the
            // reply text. The transcript is intact and the room is one reply
            // short. Say so.
            self.ledger
                .advance(
                    event_id.to_string(),
                    Stage::Ignored,
                    None,
                    Some("reply_lost_after_commit".to_string()),
                )
                .await?;
            return Ok(Disposition::ReplyLost {
                event_id: event_id.to_string(),
                channel: channel.to_string(),
            });
        };
        let reply = output
            .result
            .assistant_message()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let Some(reply) = reply else {
            self.ledger
                .advance(
                    event_id.to_string(),
                    Stage::Folded,
                    None,
                    Some("empty_model_reply".to_string()),
                )
                .await?;
            return Ok(Disposition::Silent {
                event_id: event_id.to_string(),
                channel: channel.to_string(),
                reason: "empty_model_reply",
            });
        };

        // Record the debt before incurring it. A failed post, an unreachable
        // platform or a crash now all leave a row that says exactly what is owed
        // and to whom — which is what makes recovery a read rather than a guess.
        self.ledger
            .advance(
                event_id.to_string(),
                Stage::ReplyPending,
                None,
                Some(reply.clone()),
            )
            .await?;
        let reply_ts = self.post_reply(channel, &reply, event_id).await?;
        self.ledger
            .advance(
                event_id.to_string(),
                Stage::Replied,
                Some(reply_ts.clone()),
                None,
            )
            .await?;
        Ok(Disposition::Replied {
            event_id: event_id.to_string(),
            channel: channel.to_string(),
            reply_ts,
            turn_ran: true,
        })
    }

    /// Pay off a recorded reply debt, or discover it was already paid.
    async fn settle_reply_debt(
        &self,
        event_id: &str,
        channel: &str,
        recorded_reply: Option<String>,
    ) -> Result<Disposition> {
        // The reply's own `metadata` carries the originating `event_id` into the
        // platform's durable message store, so "did I already post this?" is a
        // question the platform can answer. That is what closes the
        // crash-between-post-and-record window without an idempotency key Slack
        // does not have.
        let history = self
            .api
            .conversations_history(channel, RECOVERY_SCAN, true)
            .await
            .context("scan channel history for an already-posted reply")?;
        if let Some(reply_ts) = find_reply_for_event(&history, &self.identity.bot_id, event_id) {
            self.ledger
                .advance(
                    event_id.to_string(),
                    Stage::Replied,
                    Some(reply_ts.clone()),
                    None,
                )
                .await?;
            return Ok(Disposition::Duplicate {
                event_id: event_id.to_string(),
                stage: Stage::Replied,
                reply_ts: Some(reply_ts),
            });
        }
        let Some(reply) = recorded_reply.filter(|text| !text.trim().is_empty()) else {
            self.ledger
                .advance(
                    event_id.to_string(),
                    Stage::Ignored,
                    None,
                    Some("reply_lost_after_commit".to_string()),
                )
                .await?;
            return Ok(Disposition::ReplyLost {
                event_id: event_id.to_string(),
                channel: channel.to_string(),
            });
        };
        let reply_ts = self.post_reply(channel, &reply, event_id).await?;
        self.ledger
            .advance(
                event_id.to_string(),
                Stage::Replied,
                Some(reply_ts.clone()),
                None,
            )
            .await?;
        Ok(Disposition::Replied {
            event_id: event_id.to_string(),
            channel: channel.to_string(),
            reply_ts,
            turn_ran: false,
        })
    }

    async fn post_reply(&self, channel: &str, text: &str, event_id: &str) -> Result<String> {
        let posted = self
            .api
            .chat_post_message(&ChatPostMessageRequest::reply(channel, text, event_id))
            .await
            .context("post bot reply")?;
        Ok(posted.ts)
    }

    /// Open (or resume) the channel's session.
    async fn open_session(&self, channel: &str) -> Result<LashSession> {
        self.core
            .session(session_id(channel))
            .session_execution_owner(self.session_owner.clone())
            .open()
            .await
            .with_context(|| format!("open session for channel {channel}"))
    }

    /// Decide what an event means to this bot.
    fn classify(&self, event: &Event) -> (&'static str, Intent) {
        match event {
            Event::AppMention(_) => ("app_mention", Intent::Mention),
            Event::Message(message) => {
                if message.bot_id.is_some() {
                    // Including the bot's own replies. Without this guard the
                    // bot answers itself, forever.
                    return ("message", Intent::Ignore("app_authored_message"));
                }
                if message.user.is_none() {
                    return ("message", Intent::Ignore("no_author"));
                }
                if events::mentions(&message.text, &self.identity.bot_user_id) {
                    // Slack sends both a `message` and an `app_mention` for a
                    // mention, under two `event_id`s. Deduplication cannot help
                    // here — the ids genuinely differ — so the bot picks the
                    // event whose meaning is unambiguous and drops the other.
                    return ("message", Intent::Ignore("superseded_by_app_mention"));
                }
                ("message", Intent::Ambient)
            }
        }
    }

    /// Render an event as the line the model should see.
    async fn compose(&self, envelope: &EventCallback) -> String {
        let author = match envelope.event.user() {
            Some(user_id) => self.display_name(user_id).await,
            None => "someone".to_string(),
        };
        let text = self.resolve_mentions(envelope.event.text()).await;
        format!("{author}: {text}")
    }

    /// Strip the bot's own mention and turn other `<@U…>` tokens into names.
    async fn resolve_mentions(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("<@") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('>') else {
                out.push_str(&rest[start..]);
                return out.trim().to_string();
            };
            let token = &after[..end];
            // Slack allows `<@U012AB3CD|label>`; the id is the part before `|`.
            let user_id = token.split('|').next().unwrap_or(token);
            if user_id != self.identity.bot_user_id {
                out.push('@');
                out.push_str(&self.display_name(user_id).await);
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        // Collapse the whitespace that stripping a leading mention leaves behind.
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Display name for a user id, refreshing the directory once on a miss.
    async fn display_name(&self, user_id: &str) -> String {
        if let Some(name) = self.directory.read().await.get(user_id) {
            return name.clone();
        }
        if self.refresh_directory().await.is_ok()
            && let Some(name) = self.directory.read().await.get(user_id)
        {
            return name.clone();
        }
        user_id.to_string()
    }

    async fn channel_lock(&self, channel: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .channel_locks
            .lock()
            .expect("channel lock map is never poisoned");
        Arc::clone(
            locks
                .entry(channel.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

/// What the bot should do with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    /// Answer it.
    Mention,
    /// Remember it.
    Ambient,
    /// Neither.
    Ignore(&'static str),
}

impl Intent {
    /// Prefix for the turn-input source key, so a mention and an ambient message
    /// with the same `ts` could never collide.
    fn source_prefix(self) -> &'static str {
        match self {
            Intent::Mention => "mention",
            Intent::Ambient => "ambient",
            Intent::Ignore(_) => "ignored",
        }
    }
}
