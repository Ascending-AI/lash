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
//! **Deduplication is staged, not boolean, and every stage is resumable.** See
//! [`super::ledger`] for the record and [`ChannelBot::recover`] for what a new
//! boot does with it. The invariant that makes resumption safe is that every step
//! is idempotent: the admission by its Lash source key, the drain by its
//! `drain_id`, and the post by the `event_id` its `metadata` carries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use lash::messages::{MessageOrigin, MessageRole};
use lash::persistence::ChronologicalPayload;
use lash::persistence::LeaseOwnerIdentity;
use lash::{LashCore, LashSession, TurnInput};
use tokio::sync::RwLock;

use super::ledger::{Claim, EventLedger, EventRecord, KIND_APP_MENTION, KIND_MESSAGE, Stage};
use super::runtime::session_id;
use super::slack_api::{ChatPostMessageRequest, SlackApi, find_posted_reply};
use super::threads;
use crate::secrets::constant_time_eq;
use crate::wire::events::{self, Event, EventCallback};

/// How long [`ChannelBot::retry_deferred`] waits between attempts.
///
/// Short relative to the lease TTL it is waiting out, so the mention is answered
/// promptly once the lease lapses, and long enough that the poll costs nothing.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Default deadline for [`ChannelBot::retry_deferred`].
///
/// Four times the default 30s session-execution lease TTL: enough for a lease
/// whose owner died just after a renewal, with margin, and finite so a genuinely
/// stuck row is reported instead of retried forever.
pub const DEFERRED_RETRY_DEADLINE: Duration = Duration::from_secs(120);

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

/// Where a posted reply's text came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplySource {
    /// A turn ran now and produced it.
    Turn,
    /// It was already on record in the ledger; only the post was owed.
    Ledger,
    /// It was read back from the channel session's committed transcript after a
    /// crash lost the in-memory turn result.
    Transcript,
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
    /// A reply was posted.
    Replied {
        event_id: String,
        channel: String,
        reply_ts: String,
        source: ReplySource,
    },
    /// The work is real and unfinished, but this attempt could not reach it: the
    /// admission is claimed under a session-execution lease generation this boot
    /// cannot take yet. **Never terminal** — the ledger row is left resumable and
    /// [`ChannelBot::retry_deferred`] re-attempts it.
    Deferred {
        event_id: String,
        channel: String,
        reason: &'static str,
    },
    /// A turn ran but produced no text to post.
    Silent {
        event_id: String,
        channel: String,
        reason: &'static str,
    },
    /// A turn provably committed this admission — there is a turn-input
    /// application record for it — and neither the ledger nor the committed
    /// transcript holds any assistant text. Surfaced rather than swallowed; see
    /// the README's durability section.
    ReplyLost { event_id: String, channel: String },
}

/// What a boot recovery pass settled, and what it could not.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Events this pass finished, for logging.
    pub settled: Vec<Disposition>,
    /// Events whose admission is still claimed under a lease generation this boot
    /// cannot take. Their ledger rows remain resumable; hand these to
    /// [`ChannelBot::retry_deferred`].
    pub deferred: Vec<String>,
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
    /// One lock per routed session. Channels preserve admission order, while
    /// independent threads in the same channel remain fully parallel.
    session_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
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
            session_locks: Arc::new(Mutex::new(HashMap::new())),
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

    /// Whether an envelope's `token` is the one this bot expects.
    ///
    /// Exposed so the HTTP layer can reject a forged request before spawning any
    /// work for it.
    pub fn accepts_token(&self, token: &str) -> bool {
        constant_time_eq(token, &self.verification_token)
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
    /// Called once at boot from [`super::run`], just before the Events API
    /// request URL is registered. The endpoint is technically already listening —
    /// a platform that verified the URL on an earlier boot may be redelivering
    /// right now — which is safe: recovery and [`Self::ingest`] both take the
    /// per-channel lock and both go through the ledger's compare-and-set.
    ///
    /// This pass is not a formality. The platform's retries are bounded, and a
    /// ledger row makes every later redelivery look handled, so an event accepted
    /// a moment before a crash is finished here or never.
    pub async fn recover(&self) -> Result<RecoveryReport> {
        let unfinished = self.ledger.unfinished().await?;
        let mut report = RecoveryReport::default();
        for record in unfinished {
            let guard = self.session_lock(&record).await;
            let _held = guard.lock().await;
            let outcome = match record.stage {
                // The turn is done and its text is on record: only the post is
                // owed.
                Stage::ReplyPending => self.settle_reply_debt(&record).await?,
                // Accepted and then abandoned. The work is genuinely unfinished,
                // and every step of it is idempotent, so re-run it rather than
                // writing it off.
                _ => self.drive_accepted(&record, true).await?,
            };
            eprintln!(
                "slack-clone-bot recovered event {} ({}, {}): {outcome:?}",
                record.event_id,
                record.kind,
                record.stage.as_str()
            );
            if let Disposition::Deferred { event_id, .. } = &outcome {
                report.deferred.push(event_id.clone());
            }
            report.settled.push(outcome);
        }
        Ok(report)
    }

    /// Re-attempt a deferred event until it settles or `deadline` passes.
    ///
    /// The blocker is always the same: a previous boot's session-execution lease
    /// is still live, so this boot cannot acquire it and the interrupted turn's
    /// admission stays fenced to that lease's generation. Only when the lease
    /// *lapses* does an acquisition bump the generation, which is what makes the
    /// stale claim stealable — so the wait is bounded by the lease TTL
    /// ([`lash::persistence::LeaseTimings`], 30s by default) and nothing shorter
    /// will do.
    ///
    /// Each iteration is a real, idempotent attempt whose *result* is the state
    /// test — this polls typed runtime state, it does not sleep for a duration and
    /// then assume. [`RETRY_INTERVAL`] only keeps the loop from spinning.
    ///
    /// On deadline exhaustion the row is still left resumable rather than
    /// terminalized: a later boot's recovery pass is a better outcome than a
    /// silently dropped mention.
    pub async fn retry_deferred(
        &self,
        event_id: String,
        deadline: Duration,
    ) -> Result<Disposition> {
        let started = Instant::now();
        loop {
            let Some(record) = self.ledger.get(event_id.clone()).await? else {
                return Ok(Disposition::Ignored {
                    event_id,
                    reason: "ledger_row_vanished",
                });
            };
            if record.stage.is_terminal() {
                return Ok(Disposition::Duplicate {
                    event_id,
                    stage: record.stage,
                    reply_ts: record.reply_ts,
                });
            }
            let attempt = {
                let guard = self.session_lock(&record).await;
                let _held = guard.lock().await;
                match record.stage {
                    Stage::ReplyPending => self.settle_reply_debt(&record).await,
                    _ => self.drive_accepted(&record, true).await,
                }
            };
            // A transient failure (an unreachable platform, a network blip on
            // the history scan) is a reason to try again, not to abandon the
            // bounded persistence this loop exists to provide. A failed attempt
            // either leaves the ledger row where it was or has advanced it to a
            // stage whose handler the next iteration selects (a post that failed
            // after `advance(→ReplyPending)` is settled as reply debt), so
            // retrying is safe; only the deadline ends the loop.
            let outcome = match attempt {
                Ok(outcome) => outcome,
                Err(error) => {
                    eprintln!(
                        "slack-clone-bot retry attempt for event {event_id} failed \
                         (will retry until the deadline): {error:#}"
                    );
                    Disposition::Deferred {
                        event_id: event_id.clone(),
                        channel: record.channel_id.clone(),
                        reason: "retry_attempt_failed",
                    }
                }
            };
            if !matches!(outcome, Disposition::Deferred { .. }) {
                eprintln!("slack-clone-bot settled deferred event {event_id}: {outcome:?}");
                return Ok(outcome);
            }
            if started.elapsed() >= deadline {
                eprintln!(
                    "slack-clone-bot gave up retrying event {event_id} after {:?}; its ledger row \
                     stays resumable for the next boot",
                    started.elapsed()
                );
                return Ok(outcome);
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
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
        if !self.accepts_token(&envelope.token) {
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
        let thread_ts = envelope.event.thread_ts().map(str::to_string);
        let (kind, intent) = self.classify(&envelope.event);

        // Compose before claiming so the admission text is recorded with the row:
        // a recovery pass replays it verbatim rather than recomposing, which is
        // what keeps the Lash source key idempotent instead of conflicting.
        let admission = match intent {
            Intent::Ignore(_) => None,
            Intent::Ambient | Intent::Mention => Some(self.compose(&envelope).await),
        };
        let claim = self
            .ledger
            .claim(
                envelope.event_id.clone(),
                channel.clone(),
                message_ts,
                kind.to_string(),
                admission,
                thread_ts,
            )
            .await?;
        if let Claim::Settled(record) = &claim {
            return Ok(Disposition::Duplicate {
                event_id: record.event_id.clone(),
                stage: record.stage,
                reply_ts: record.reply_ts.clone(),
            });
        }

        if let Intent::Ignore(reason) = intent {
            self.settle(
                claim.record(),
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

        let guard = self.session_lock(claim.record()).await;
        let _held = guard.lock().await;

        let record = claim.record();
        let resuming = matches!(claim, Claim::Resume(_));
        // A redelivery of an event that already owes a reply must not run the
        // model again: the text is on record and the only open question is
        // whether it reached the channel.
        if resuming && record.stage == Stage::ReplyPending {
            return self.settle_reply_debt(record).await;
        }
        self.drive_accepted(record, resuming).await
    }

    /// Do the work an `accepted` row describes: admit the message, and for a
    /// mention, run the turn and post.
    ///
    /// `resuming` means "this row may already have been worked on", which costs
    /// one `conversations.history` scan to rule out a reply that was posted
    /// before the crash. A first delivery skips it: there cannot be a prior reply
    /// to an event nobody has seen.
    async fn drive_accepted(&self, record: &EventRecord, resuming: bool) -> Result<Disposition> {
        let Some(text) = record.input_text.clone() else {
            // Only reachable for a row written before `input_text` existed. The
            // admission text is unrecoverable, so say so instead of guessing.
            self.settle(
                record,
                Stage::Ignored,
                None,
                Some("admission_text_unavailable".to_string()),
            )
            .await?;
            return Ok(Disposition::Ignored {
                event_id: record.event_id.clone(),
                reason: "admission_text_unavailable",
            });
        };
        let is_mention = record.kind == KIND_APP_MENTION;

        if is_mention
            && resuming
            && let Some(reply_ts) = self.already_posted(record).await?
        {
            self.settle(record, Stage::Replied, Some(reply_ts.clone()), None)
                .await?;
            return Ok(Disposition::Duplicate {
                event_id: record.event_id.clone(),
                stage: Stage::Replied,
                reply_ts: Some(reply_ts),
            });
        }

        let session = if record.thread_ts.is_some() {
            let Some(session) =
                threads::open_thread_session(&self.core, &self.ledger, &self.session_owner, record)
                    .await?
            else {
                self.settle(
                    record,
                    Stage::Ignored,
                    None,
                    Some("thread_session_retired".to_string()),
                )
                .await?;
                return Ok(Disposition::Ignored {
                    event_id: record.event_id.clone(),
                    reason: "thread_session_retired",
                });
            };
            session
        } else {
            self.open_session(&record.channel_id).await?
        };
        // The source key is derived from the message's `ts`, not the `event_id`:
        // `ts` *is* the message's identity, so a redelivery — or the same message
        // arriving under a second event — resolves to the one admission record
        // Lash already holds instead of a duplicate context line. A consumed
        // input keeps its row, so this stays idempotent even after the turn that
        // drained it committed.
        let prefix = if is_mention { "mention" } else { "ambient" };
        let receipt = session
            .enqueue(TurnInput::text(text))
            .id(format!(
                "{prefix}:{}:{}",
                record.channel_id, record.message_ts
            ))
            .send()
            .await
            .context("admit routed message as queued turn input")?;
        self.ledger
            .record_input_id(record.event_id.clone(), receipt.input_id.clone())
            .await
            .context("record Lash admission identity")?;

        if !is_mention {
            self.settle(record, Stage::Folded, None, None).await?;
            return Ok(Disposition::Folded {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
                input_id: receipt.input_id,
            });
        }
        self.run_mention_turn(&session, record, &receipt.input_id)
            .await
    }

    /// Drain every queued input for the channel into one turn and post the reply.
    async fn run_mention_turn(
        &self,
        session: &LashSession,
        record: &EventRecord,
        input_id: &str,
    ) -> Result<Disposition> {
        // The drain id is stable per event, so the queue-drain effect scope is
        // the same on a redelivery as it was on the first attempt.
        let output = session
            .queued_turn()
            .drain_id(format!("mention:{}", record.event_id))
            .run()
            .await
            .context("run channel mention turn")?;

        let Some(output) = output else {
            // `None` is ambiguous and must not be guessed at. It means either
            // "a committed turn already consumed this input" or "this drain could
            // not reach the input", and the two demand opposite responses.
            return self.settle_empty_drain(session, record, input_id).await;
        };

        if record.thread_ts.is_none() {
            threads::retain_applied_turn_boundary(&self.core, &self.ledger, session, input_id)
                .await?;
        }

        let Some(reply) = output
            .result
            .assistant_message()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
        else {
            self.settle(
                record,
                Stage::Folded,
                None,
                Some("empty_model_reply".to_string()),
            )
            .await?;
            return Ok(Disposition::Silent {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
                reason: "empty_model_reply",
            });
        };
        self.owe_and_post(record, reply, ReplySource::Turn).await
    }

    /// Decide what an empty queued drain means, using committed-turn evidence.
    ///
    /// A drain returns `None` for two unrelated reasons, and reading one as the
    /// other is how a mention gets abandoned:
    ///
    /// * **A committed turn already consumed the input.** The answer exists (or
    ///   provably does not) in the transcript. Terminal either way.
    /// * **This drain never reached the input.** `stream_queued_work` requires the
    ///   session-execution lease and returns `Ok(None)` when it is busy, and a
    ///   claim pinned to a still-live lease generation is not stealable. A boot
    ///   that restarts inside the previous boot's lease TTL hits exactly this.
    ///   Nothing was consumed, so the work is **retryable and never terminal**.
    ///
    /// The discriminator is [`LashSession::turn_input_applications`]: an
    /// application record is written only when a turn *commits*, and it names the
    /// `input_id` it consumed. Its absence is positive proof that no turn has
    /// taken this admission. (`pending_turn_inputs` cannot answer this — it hides
    /// rows whose claim is pinned to a currently-live lease generation, which is
    /// precisely the state under investigation.)
    async fn settle_empty_drain(
        &self,
        session: &LashSession,
        record: &EventRecord,
        input_id: &str,
    ) -> Result<Disposition> {
        let applications = session
            .turn_input_applications()
            .await
            .context("read committed turn-input applications")?;
        let consumed = applications
            .iter()
            .any(|application| application.input_id == input_id);
        if !consumed {
            // Retryable. The ledger row is deliberately left at its current
            // non-terminal stage: terminalizing here is what made an interrupted
            // mention permanently unanswered, because no redelivery and no later
            // boot ever revisits a terminal row.
            eprintln!(
                "slack-clone-bot deferring event {}: its admission is claimed by an \
                 execution lease this boot cannot take yet",
                record.event_id
            );
            return Ok(Disposition::Deferred {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
                reason: "input_claimed_by_live_lease_generation",
            });
        }
        if record.thread_ts.is_none() {
            threads::retain_applied_turn_boundary(&self.core, &self.ledger, session, input_id)
                .await?;
        }
        match reply_from_transcript(session, input_id) {
            Some(reply) => {
                self.owe_and_post(record, reply, ReplySource::Transcript)
                    .await
            }
            None => {
                // A turn really did commit this input and left no assistant text.
                // This is the only route to `ReplyLost`.
                self.settle(
                    record,
                    Stage::Ignored,
                    None,
                    Some("reply_lost_after_commit".to_string()),
                )
                .await?;
                Ok(Disposition::ReplyLost {
                    event_id: record.event_id.clone(),
                    channel: record.channel_id.clone(),
                })
            }
        }
    }

    /// Record the reply debt, post it, and mark the event replied.
    async fn owe_and_post(
        &self,
        record: &EventRecord,
        reply: String,
        source: ReplySource,
    ) -> Result<Disposition> {
        // Record the debt before incurring it. A failed post, an unreachable
        // platform or a crash now all leave a row that says exactly what is owed
        // and to whom — which is what makes recovery a read rather than a guess.
        if !self
            .ledger
            .advance(
                record.event_id.clone(),
                record.stage,
                Stage::ReplyPending,
                None,
                Some(reply.clone()),
            )
            .await?
        {
            return self.observed_elsewhere(record).await;
        }
        let reply_ts = self.post_reply(record, &reply).await?;
        self.ledger
            .advance(
                record.event_id.clone(),
                Stage::ReplyPending,
                Stage::Replied,
                Some(reply_ts.clone()),
                None,
            )
            .await?;
        Ok(Disposition::Replied {
            event_id: record.event_id.clone(),
            channel: record.channel_id.clone(),
            reply_ts,
            source,
        })
    }

    /// Pay off a recorded reply debt, or discover it was already paid.
    async fn settle_reply_debt(&self, record: &EventRecord) -> Result<Disposition> {
        if let Some(reply_ts) = self.already_posted(record).await? {
            self.settle(record, Stage::Replied, Some(reply_ts.clone()), None)
                .await?;
            return Ok(Disposition::Duplicate {
                event_id: record.event_id.clone(),
                stage: Stage::Replied,
                reply_ts: Some(reply_ts),
            });
        }
        let Some(reply) = record.detail.clone().filter(|text| !text.trim().is_empty()) else {
            self.settle(
                record,
                Stage::Ignored,
                None,
                Some("reply_lost_after_commit".to_string()),
            )
            .await?;
            return Ok(Disposition::ReplyLost {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
            });
        };
        let reply_ts = self.post_reply(record, &reply).await?;
        self.settle(record, Stage::Replied, Some(reply_ts.clone()), None)
            .await?;
        Ok(Disposition::Replied {
            event_id: record.event_id.clone(),
            channel: record.channel_id.clone(),
            reply_ts,
            source: ReplySource::Ledger,
        })
    }

    /// Has this bot already posted a reply for `record`'s event?
    ///
    /// The reply's own `metadata` carries the originating `event_id` into the
    /// platform's durable message store, so "did I already post this?" is a
    /// question the platform can answer. That is what closes the
    /// crash-between-post-and-record window without an idempotency key Slack does
    /// not have. The scan is bounded by the triggering message's `ts` rather than
    /// by a message count, so a busy channel cannot push the reply out of view.
    async fn already_posted(&self, record: &EventRecord) -> Result<Option<String>> {
        find_posted_reply(
            &self.api,
            &self.identity.bot_id,
            &record.channel_id,
            &record.message_ts,
            &record.event_id,
            record.thread_ts.as_deref(),
        )
        .await
        .context("scan channel history for an already-posted reply")
    }

    /// Advance a row to a terminal stage, tolerating a concurrent winner.
    async fn settle(
        &self,
        record: &EventRecord,
        to: Stage,
        reply_ts: Option<String>,
        detail: Option<String>,
    ) -> Result<()> {
        if !self
            .ledger
            .advance(record.event_id.clone(), record.stage, to, reply_ts, detail)
            .await?
        {
            eprintln!(
                "slack-clone-bot: event {} moved on before this handler settled it",
                record.event_id
            );
        }
        Ok(())
    }

    /// Report what the ledger now says, for the case where a compare-and-set lost.
    async fn observed_elsewhere(&self, record: &EventRecord) -> Result<Disposition> {
        let current = self.ledger.get(record.event_id.clone()).await?;
        Ok(Disposition::Duplicate {
            event_id: record.event_id.clone(),
            stage: current.as_ref().map_or(record.stage, |row| row.stage),
            reply_ts: current.and_then(|row| row.reply_ts),
        })
    }

    async fn post_reply(&self, record: &EventRecord, text: &str) -> Result<String> {
        let request = match record.thread_ts.as_deref() {
            Some(thread_ts) => ChatPostMessageRequest::thread_reply(
                &record.channel_id,
                text,
                &record.event_id,
                thread_ts,
            ),
            None => ChatPostMessageRequest::reply(&record.channel_id, text, &record.event_id),
        };
        let posted = self
            .api
            .chat_post_message(&request)
            .await
            .context("post bot reply")?;
        Ok(posted.ts)
    }

    /// Open (or resume) the channel's session.
    async fn open_session(&self, channel: &str) -> Result<LashSession> {
        let session = self
            .core
            .session(session_id(channel))
            .session_execution_owner(self.session_owner.clone())
            .open()
            .await
            .with_context(|| format!("open session for channel {channel}"))?;
        threads::ensure_forkable_channel_head(&self.core, &session).await?;
        Ok(session)
    }

    /// Decide what an event means to this bot.
    fn classify(&self, event: &Event) -> (&'static str, Intent) {
        match event {
            Event::AppMention(_) => (KIND_APP_MENTION, Intent::Mention),
            Event::Message(message) => {
                if message.bot_id.is_some() {
                    // Including the bot's own replies. Without this guard the
                    // bot answers itself, forever.
                    return (KIND_MESSAGE, Intent::Ignore("app_authored_message"));
                }
                if message.user.is_none() {
                    return (KIND_MESSAGE, Intent::Ignore("no_author"));
                }
                if events::mentions(&message.text, &self.identity.bot_user_id) {
                    // Slack sends both a `message` and an `app_mention` for a
                    // mention, under two `event_id`s. Deduplication cannot help
                    // here — the ids genuinely differ — so the bot picks the
                    // event whose meaning is unambiguous and drops the other.
                    return (KIND_MESSAGE, Intent::Ignore("superseded_by_app_mention"));
                }
                (KIND_MESSAGE, Intent::Ambient)
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

    async fn session_lock(&self, record: &EventRecord) -> Arc<tokio::sync::Mutex<()>> {
        let key = record.thread_ts.as_deref().map_or_else(
            || session_id(&record.channel_id),
            |thread_ts| super::runtime::thread_session_id(&record.channel_id, thread_ts),
        );
        let mut locks = self
            .session_locks
            .lock()
            .expect("session lock map is never poisoned");
        Arc::clone(
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

/// Read the assistant answer for the turn that consumed `input_id` out of the
/// session's committed transcript.
///
/// Used when a queued drain returns nothing because a previous process already
/// ran the turn and died before its reply was recorded. Correlation is by the
/// typed provenance Lash publishes on committed messages
/// ([`MessageOrigin::TurnInput`]) — not by parsing id strings:
///
/// 1. find the committed message whose origin names `input_id`, and take its
///    `turn_id`;
/// 2. walk forward, remembering the last `Assistant` message, and stop at the
///    first message admitted by a *different* turn.
///
/// Step 2's stop condition is what prevents misattribution when later turns
/// exist, and "last, not first" is what skips the intermediate assistant messages
/// that carry tool calls in a standard-mode loop. Returns `None` when the turn
/// committed no assistant text at all, which the caller reports honestly rather
/// than papering over.
fn reply_from_transcript(session: &LashSession, input_id: &str) -> Option<String> {
    let read_view = session.read_view();
    let mut turn_id: Option<String> = None;
    let mut answer: Option<String> = None;
    for entry in read_view.chronological_projection().into_entries() {
        let ChronologicalPayload::Message(message) = entry.payload else {
            continue;
        };
        let admitted_by = match message.origin.as_ref() {
            Some(MessageOrigin::TurnInput {
                turn_id,
                input_id: admitted,
            }) => Some((turn_id.as_str(), admitted.as_deref())),
            _ => None,
        };
        match (&turn_id, admitted_by) {
            // Our input's committed copy: remember which turn consumed it.
            (None, Some((turn, Some(admitted)))) if admitted == input_id => {
                turn_id = Some(turn.to_string());
            }
            // Nothing found yet; keep scanning.
            (None, _) => {}
            // A later turn begins: whatever we have is our turn's answer.
            (Some(ours), Some((turn, _))) if turn != ours => break,
            // Inside our turn (including its sibling admissions).
            (Some(_), _) => {
                if message.role == MessageRole::Assistant {
                    answer = Some(lash::message_text(&message));
                }
            }
        }
    }
    answer
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
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
