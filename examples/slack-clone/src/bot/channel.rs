//! Session-per-channel event handling: the part of the example worth copying.
//!
//! Three decisions carry the design.
//!
//! **A channel is a session.** `channel:<C…>` is the Lash session id, so the
//! bot's memory of a room is exactly as long-lived as the room, survives
//! restarts, and never leaks between channels.
//!
//! **Ambient traffic is context, not a turn.** Messages that do not mention the
//! bot are recorded in the ledger and no turn runs. When somebody finally does
//! mention the bot, the bot folds the route's accumulated ambient text *and*
//! the mention into one [`lash::LashSession::send`]; the session's engine runs
//! that turn as soon as it is accepted (FIG-3600), and the bot only waits on
//! it. The bot has been listening the whole time without saying a word or
//! spending a token. A thread starts the same way: its first send carries the
//! parent channel's folded context up to the thread root, labelled.
//!
//! **Deduplication is staged, not boolean, and every stage is resumable.** See
//! [`super::ledger`] for the record and [`ChannelBot::recover`] for what a new
//! boot does with it. The invariant that makes resumption safe is that every step
//! is idempotent: the admission and its turn by the send's id, and the post by
//! the `event_id` its `metadata` carries.

use lash::TurnId;
use lash::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use lash::messages::{MessageOrigin, MessageRole};
use lash::persistence::ChronologicalPayload;
use lash::{LashCore, LashSession, SendHandle, TurnInput, TurnOutcome, TurnStatus, TurnStop};
use tokio::sync::RwLock;

use super::ledger::{
    Claim, DetailWrite, EventLedger, EventReason, EventRecord, KIND_APP_MENTION, KIND_MESSAGE,
    ProviderFailure, Stage,
};
use super::runtime::session_id;
use super::slack_api::{ChatPostMessageRequest, SlackApi, find_posted_reply};
use super::threads;
use crate::log_err;
use crate::secrets::constant_time_eq;
use crate::wire::events::{self, Event, EventCallback};

type SessionLockRegistry = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// How long [`ChannelBot::retry_deferred`] waits between attempts.
///
/// Short relative to the engine's redelivery of the interrupted invocation it is
/// waiting on, so the mention is answered promptly once the lane clears, and long
/// enough that the poll costs nothing.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// How long a resumed mention waits for its turn before it is deferred to
/// the retry loop.
const RESUMED_TURN_WAIT: Duration = Duration::from_secs(2);

/// Default deadline for [`ChannelBot::retry_deferred`].
///
/// Long enough for the restate-server to re-drive an interrupted invocation on
/// the restarted endpoint and for transient admission contention to clear, and
/// finite so a genuinely stuck row is reported instead of retried forever.
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
pub enum DeliveryOutcome {
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
    /// Kept in the ledger as context for the route's next mention. No turn,
    /// no reply.
    Folded { event_id: String, channel: String },
    /// A reply was posted.
    Replied {
        event_id: String,
        channel: String,
        reply_ts: String,
        source: ReplySource,
    },
    /// The work is real and unfinished, but this attempt could not reach it: the
    /// session's admission is contended by another writer. **Never terminal** —
    /// the ledger row is left resumable and [`ChannelBot::retry_deferred`]
    /// re-attempts it.
    Deferred {
        event_id: String,
        channel: String,
        reason: &'static str,
    },
    /// A bounded wait could not find the thread root's durable route. The user
    /// was notified in-thread, while the event stays at the FIG-1008 non-terminal
    /// stage so the bot's own retry loop or a later boot can recover it.
    RecoverableFailure {
        event_id: String,
        channel: String,
        notified: bool,
        reason: &'static str,
    },
    /// A turn ran but produced no text to post.
    Silent {
        event_id: String,
        channel: String,
        reason: &'static str,
    },
    /// A turn reached a terminal provider failure. The typed failure is retained
    /// in the ledger and surfaced here without reducing it to a string reason.
    ProviderError {
        event_id: String,
        channel: String,
        failure: ProviderFailure,
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
    pub settled: Vec<DeliveryOutcome>,
    /// Events worth retrying in-process after this pass. This includes admissions
    /// contended by another writer and thread roots that exist but have not
    /// finished.
    /// A `thread_root_not_available` row is deliberately excluded: recovery has
    /// already given it one cheap probe this boot.
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
    /// One lock per routed session. Channels preserve admission order, while
    /// independent threads in the same channel remain fully parallel.
    session_locks: SessionLockRegistry,
    /// `U…` to display name, so `<@U…>` renders as something a model can reason
    /// about. Filled from `users.list` and refreshed on a miss.
    directory: Arc<RwLock<HashMap<String, String>>>,
    #[cfg(test)]
    root_wait: Arc<threads::RootWaitObserver>,
    #[cfg(test)]
    thread_root_wait_budget: Arc<Mutex<Duration>>,
}

impl ChannelBot {
    /// Assemble a bot over an already-built core.
    pub fn new(
        core: LashCore,
        api: Arc<SlackApi>,
        ledger: EventLedger,
        identity: BotIdentity,
        verification_token: String,
    ) -> Self {
        Self {
            core,
            api,
            ledger,
            identity,
            verification_token,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            directory: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            root_wait: Arc::new(threads::RootWaitObserver::default()),
            #[cfg(test)]
            thread_root_wait_budget: Arc::new(Mutex::new(threads::ROOT_ADMISSION_WAIT_BUDGET)),
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

    #[cfg(test)]
    pub fn session_lock_count(&self) -> usize {
        self.session_locks.lock_recover().len()
    }

    #[cfg(test)]
    pub async fn wait_for_missing_thread_root(&self) {
        self.root_wait.missing_root().await;
    }

    /// What the thread-root admission wait actually did: how much budget it
    /// spent and how many turns saw a missing root. Tests assert the
    /// fail-fast fact through these rather than through a wall-clock bound,
    /// which a loaded runner breaks for reasons the test does not care about.
    #[cfg(test)]
    pub fn thread_root_wait(&self) -> &threads::RootWaitObserver {
        &self.root_wait
    }

    #[cfg(test)]
    pub fn set_thread_root_wait_budget(&self, budget: Duration) {
        *self.thread_root_wait_budget.lock_recover() = budget;
    }

    fn thread_root_wait_budget(&self) -> Duration {
        #[cfg(test)]
        {
            *self.thread_root_wait_budget.lock_recover()
        }
        #[cfg(not(test))]
        {
            threads::ROOT_ADMISSION_WAIT_BUDGET
        }
    }

    /// Remaining background budget after a foreground thread-root wait.
    pub fn recoverable_retry_deadline(&self) -> Duration {
        DEFERRED_RETRY_DEADLINE.saturating_sub(self.thread_root_wait_budget())
    }

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
            let guard = self.session_lock(&record);
            let _held = guard.lock().await;
            let outcome = match record.stage {
                // The turn is done and its text is on record: only the post is
                // owed.
                Stage::ReplyPending => self.settle_reply_debt(&record).await?,
                // Accepted and then abandoned. The work is genuinely unfinished,
                // and every step of it is idempotent, so re-run it rather than
                // writing it off.
                _ => {
                    // Boot recovery probes thread routes once without parking the
                    // serial pass behind a 45s wait. A root that exists but is
                    // still processing is handed to the background retry loop;
                    // an unavailable root stays recoverable but cheap next boot.
                    let root_wait_budget = if record.thread_ts.is_some() {
                        Duration::ZERO
                    } else {
                        self.thread_root_wait_budget()
                    };
                    self.drive_accepted_with_root_budget(&record, true, root_wait_budget)
                        .await?
                }
            };
            log_err!(
                "slack-clone-bot recovered event {} ({}, {}): {outcome:?}",
                record.event_id,
                record.kind,
                record.stage.as_str()
            );
            match &outcome {
                DeliveryOutcome::Deferred { event_id, .. } => {
                    report.deferred.push(event_id.clone())
                }
                DeliveryOutcome::RecoverableFailure {
                    event_id, reason, ..
                } if *reason == EventReason::ThreadRootNotProcessed.as_str() => {
                    report.deferred.push(event_id.clone());
                }
                _ => {}
            }
            report.settled.push(outcome);
        }
        Ok(report)
    }

    /// Re-attempt a deferred event until it settles or `deadline` passes.
    ///
    /// Retryable events are either contended by another writer's in-flight
    /// admission — including the drive a restarted boot is still taking over —
    /// or waiting for a thread root that has not yet published its admission
    /// boundary. Both conditions are observed through durable state on every
    /// attempt.
    ///
    /// Each iteration is a real, idempotent attempt whose *result* is the state
    /// test — this polls typed runtime state, it does not sleep for a duration and
    /// then assume. [`RETRY_INTERVAL`] only keeps the loop from spinning.
    ///
    /// `deadline` covers every root wait entered by this loop, rather than
    /// restarting the 45s budget on each attempt. On exhaustion the row is still
    /// left resumable rather than terminalized: a later boot's recovery pass is
    /// a better outcome than a silently dropped mention.
    pub async fn retry_deferred(
        &self,
        event_id: String,
        deadline: Duration,
    ) -> Result<DeliveryOutcome> {
        let started = Instant::now();
        loop {
            let Some(record) = self.ledger.get(event_id.clone()).await? else {
                return Ok(DeliveryOutcome::Ignored {
                    event_id,
                    reason: "ledger_row_vanished",
                });
            };
            if record.stage.is_terminal() {
                return Ok(DeliveryOutcome::Duplicate {
                    event_id,
                    stage: record.stage,
                    reply_ts: record.reply_ts,
                });
            }
            let attempt = {
                let guard = self.session_lock(&record);
                let _held = guard.lock().await;
                match record.stage {
                    Stage::ReplyPending => self.settle_reply_debt(&record).await,
                    _ => {
                        let remaining = deadline.saturating_sub(started.elapsed());
                        let root_wait_budget =
                            if record.detail.as_deref().and_then(EventReason::parse)
                                == Some(EventReason::ThreadRootNotAvailable)
                            {
                                Duration::ZERO
                            } else {
                                self.thread_root_wait_budget().min(remaining)
                            };
                        self.drive_accepted_with_root_budget(&record, true, root_wait_budget)
                            .await
                    }
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
                    log_err!(
                        "slack-clone-bot retry attempt for event {event_id} failed \
                         (will retry until the deadline): {error:#}"
                    );
                    DeliveryOutcome::Deferred {
                        event_id: event_id.clone(),
                        channel: record.channel_id.clone(),
                        reason: "retry_attempt_failed",
                    }
                }
            };
            if !matches!(
                outcome,
                DeliveryOutcome::Deferred { .. } | DeliveryOutcome::RecoverableFailure { .. }
            ) {
                log_err!("slack-clone-bot settled deferred event {event_id}: {outcome:?}");
                return Ok(outcome);
            }
            if started.elapsed() >= deadline {
                log_err!(
                    "slack-clone-bot gave up retrying event {event_id} after {:?}; its ledger row \
                     stays resumable for the next boot",
                    started.elapsed()
                );
                return Ok(outcome);
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }

    /// `retry_num` is the value of `x-slack-retry-num`, recorded for observation
    /// only: correctness must not depend on it, because the first delivery and
    /// the third carry the same `event_id` and must be treated identically.
    pub async fn ingest(
        &self,
        envelope: EventCallback,
        retry_num: Option<u32>,
    ) -> Result<DeliveryOutcome> {
        if !self.accepts_token(&envelope.token) {
            return Ok(DeliveryOutcome::Rejected {
                reason: "bad_verification_token",
            });
        }
        if let Some(retry) = retry_num {
            log_err!(
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
            return Ok(DeliveryOutcome::Duplicate {
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
                DetailWrite::Set(reason.as_str().to_string()),
            )
            .await?;
            return Ok(DeliveryOutcome::Ignored {
                event_id: envelope.event_id,
                reason: reason.as_str(),
            });
        }

        let guard = self.session_lock(claim.record());
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
    async fn drive_accepted(
        &self,
        record: &EventRecord,
        resuming: bool,
    ) -> Result<DeliveryOutcome> {
        self.drive_accepted_with_root_budget(record, resuming, self.thread_root_wait_budget())
            .await
    }

    async fn drive_accepted_with_root_budget(
        &self,
        record: &EventRecord,
        resuming: bool,
        thread_root_wait_budget: Duration,
    ) -> Result<DeliveryOutcome> {
        let Some(text) = record.input_text.clone() else {
            // Only reachable for a row written before `input_text` existed. The
            // admission text is unrecoverable, so say so instead of guessing.
            self.settle(
                record,
                Stage::Ignored,
                None,
                DetailWrite::Set(EventReason::AdmissionTextUnavailable.as_str().to_string()),
            )
            .await?;
            return Ok(DeliveryOutcome::Ignored {
                event_id: record.event_id.clone(),
                reason: EventReason::AdmissionTextUnavailable.as_str(),
            });
        };
        let is_mention = record.kind == KIND_APP_MENTION;

        if is_mention
            && resuming
            && let Some(reply_ts) = self.already_posted(record).await?
        {
            self.settle(
                record,
                Stage::Replied,
                Some(reply_ts.clone()),
                DetailWrite::Clear,
            )
            .await?;
            return Ok(DeliveryOutcome::Duplicate {
                event_id: record.event_id.clone(),
                stage: Stage::Replied,
                reply_ts: Some(reply_ts),
            });
        }

        let (session, inherited_context) = if record.thread_ts.is_some() {
            match threads::open_thread_session(
                &self.core,
                &self.ledger,
                record,
                #[cfg(test)]
                &self.root_wait,
                thread_root_wait_budget,
            )
            .await?
            {
                threads::ThreadSessionOpen::Ready {
                    session,
                    inherited_context,
                } => (session, Some(inherited_context)),
                threads::ThreadSessionOpen::Retired => {
                    self.settle(
                        record,
                        Stage::Ignored,
                        None,
                        DetailWrite::Set(EventReason::ThreadSessionRetired.as_str().to_string()),
                    )
                    .await?;
                    return Ok(DeliveryOutcome::Ignored {
                        event_id: record.event_id.clone(),
                        reason: EventReason::ThreadSessionRetired.as_str(),
                    });
                }
                threads::ThreadSessionOpen::AdmissionContended => {
                    Self::log_turn_deferral(record, "the session lane is held elsewhere");
                    return Ok(DeliveryOutcome::Deferred {
                        event_id: record.event_id.clone(),
                        channel: record.channel_id.clone(),
                        reason: "session_admission_contended",
                    });
                }
                threads::ThreadSessionOpen::RootNotProcessed => {
                    return self
                        .fail_missing_thread_root(
                            record,
                            is_mention,
                            EventReason::ThreadRootNotProcessed,
                        )
                        .await;
                }
                threads::ThreadSessionOpen::RootNotAvailable => {
                    return self
                        .fail_missing_thread_root(
                            record,
                            is_mention,
                            EventReason::ThreadRootNotAvailable,
                        )
                        .await;
                }
            }
        } else {
            match self.open_session(&record.channel_id).await {
                Ok(session) => (session, None),
                Err(error) if threads::anyhow_session_admission_contended(&error) => {
                    Self::log_turn_deferral(record, "the session lane is held elsewhere");
                    return Ok(DeliveryOutcome::Deferred {
                        event_id: record.event_id.clone(),
                        channel: record.channel_id.clone(),
                        reason: "session_admission_contended",
                    });
                }
                Err(error) => return Err(error),
            }
        };
        if !is_mention {
            // Ambient traffic is context, not a turn input: it waits in the
            // ledger until a mention on its route folds it into that mention's
            // send, so no turn runs and no token is spent for it. A thread
            // rooted at this message forks from the channel head *before* it.
            if record.thread_ts.is_none() {
                threads::retain_admission_boundary(
                    &self.core,
                    &self.ledger,
                    &session,
                    &record.event_id,
                )
                .await?;
            }
            self.settle(record, Stage::Folded, None, DetailWrite::Keep)
                .await?;
            return Ok(DeliveryOutcome::Folded {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
            });
        }
        // One send per mention: the route's folded ambient context, then the
        // mention itself. The ledger binds the folded rows to this mention and
        // stores the composed text on first use, so a redelivery or a recovery
        // pass sends the same bytes under the same id and resolves to the
        // admission Lash already holds. `RunSpec.context` is the folded
        // block's home once a run definition reads it.
        let send_text = self
            .ledger
            .bind_mention_send(
                record.event_id.clone(),
                text,
                inherited_context.unwrap_or_default(),
            )
            .await
            .context("fold the route's ambient context into the mention")?;
        let handle = session
            .send(TurnInput::text(send_text))
            .id(format!(
                "mention:{}:{}",
                record.channel_id, record.message_ts
            ))
            .await
            .context("send the mention to its session")?;
        let input_id = handle.input_id().to_string();
        self.ledger
            .record_mention_input_id(record.event_id.clone(), input_id)
            .await
            .context("record Lash admission identity")?;
        self.run_mention_turn(&session, record, handle, resuming)
            .await
    }

    /// Notify a mention without terminalizing it, so authoritative root arrival
    /// plus an ordinary redelivery can run the exact same accepted row again.
    async fn fail_missing_thread_root(
        &self,
        record: &EventRecord,
        notify_user: bool,
        detail: EventReason,
    ) -> Result<DeliveryOutcome> {
        let copy: &str = if detail == EventReason::ThreadRootNotAvailable {
            "I can’t find the message this thread started from, so I can’t answer right \
             now. If it reaches me later, I’ll follow up here."
        } else {
            "I can’t answer this thread yet — I haven’t caught up with its \
             original message. I’ll follow up here once I have."
        };

        if !self
            .ledger
            .advance(
                record.event_id.clone(),
                record.stage,
                record.stage,
                None,
                DetailWrite::Set(detail.as_str().to_string()),
            )
            .await?
        {
            return self.observed_elsewhere(record).await;
        }

        let notified = if notify_user {
            let notification_id = format!("{}:thread-root-not-processed", record.event_id);
            if find_posted_reply(
                &self.api,
                &self.identity.bot_id,
                &record.channel_id,
                &record.message_ts,
                &notification_id,
                record.thread_ts.as_deref(),
            )
            .await
            .context("scan for missing-root notification")?
            .is_some()
            {
                false
            } else {
                let thread_ts = record
                    .thread_ts
                    .as_deref()
                    .context("missing-root failure has no thread route")?;
                let request = ChatPostMessageRequest::thread_reply(
                    &record.channel_id,
                    copy,
                    &notification_id,
                    thread_ts,
                );
                self.api
                    .chat_post_message(&request)
                    .await
                    .context("post missing-root notification")?;
                true
            }
        } else {
            false
        };
        Ok(DeliveryOutcome::RecoverableFailure {
            event_id: record.event_id.clone(),
            channel: record.channel_id.clone(),
            notified,
            reason: detail.as_str(),
        })
    }

    /// Wait for the mention's turn and post its reply. The session's engine
    /// runs the turn as soon as the mention is sent; the bot only waits on it.
    ///
    /// A resumed mention whose input a committed turn already answered is
    /// read back out of the transcript. A resumed mention whose turn has not
    /// settled within [`RESUMED_TURN_WAIT`] is deferred, never terminalized:
    /// its root may still be re-driving on the engine after a previous boot
    /// died, and the retry loop re-attaches to the same input until it settles.
    async fn run_mention_turn(
        &self,
        session: &LashSession,
        record: &EventRecord,
        handle: SendHandle,
        resuming: bool,
    ) -> Result<DeliveryOutcome> {
        let input_id = handle.input_id().to_string();
        let input_id = input_id.as_str();
        if resuming
            && session
                .durable()
                .turn_input_applications()
                .await
                .context("read the channel's applied inputs")?
                .iter()
                .any(|application| application.input_id.as_str() == input_id)
        {
            return self
                .settle_committed_mention(session, record, input_id)
                .await;
        }
        let outcome = if resuming {
            match tokio::time::timeout(RESUMED_TURN_WAIT, handle.outcome()).await {
                Ok(outcome) => outcome,
                Err(_) => return Ok(Self::defer_unsettled_turn(record)),
            }
        } else {
            handle.outcome().await
        }
        .context("run channel mention turn")?;
        let output = match (outcome.status, outcome.output) {
            (TurnStatus::Parked(_), _) | (_, None) => {
                return Ok(Self::defer_unsettled_turn(record));
            }
            (_, Some(output)) => output,
        };

        if record.thread_ts.is_none() {
            threads::retain_applied_turn_boundary(&self.core, &self.ledger, session, input_id)
                .await?;
        }

        if matches!(
            output.result.outcome,
            TurnOutcome::Stopped(TurnStop::ProviderError)
        ) && let Some(failure) = provider_failure(&output.result)
        {
            self.ledger
                .advance_provider_error(record.event_id.clone(), record.stage, failure.clone())
                .await?;
            return Ok(DeliveryOutcome::ProviderError {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
                failure,
            });
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
                DetailWrite::Set(EventReason::EmptyModelReply.as_str().to_string()),
            )
            .await?;
            return Ok(DeliveryOutcome::Silent {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
                reason: EventReason::EmptyModelReply.as_str(),
            });
        };
        self.owe_and_post(record, reply, ReplySource::Turn).await
    }

    /// Settle a mention whose input a committed turn already answered: the
    /// answer is in the transcript, so recovery reads it back instead of
    /// running the model again.
    async fn settle_committed_mention(
        &self,
        session: &LashSession,
        record: &EventRecord,
        input_id: &str,
    ) -> Result<DeliveryOutcome> {
        if record.thread_ts.is_none() {
            threads::retain_applied_turn_boundary(&self.core, &self.ledger, session, input_id)
                .await?;
        }
        // The application was read from the store, so the transcript comes
        // from the same authority: a handle opened before an engine-driven
        // turn committed legitimately lacks its messages.
        let view = session
            .durable()
            .read()
            .await
            .context("read the committed view for transcript replay")?
            .context("a committed mention implies a readable committed view")?;
        match reply_from_transcript(&view, input_id) {
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
                    DetailWrite::Set(EventReason::ReplyLostAfterCommit.as_str().to_string()),
                )
                .await?;
                Ok(DeliveryOutcome::ReplyLost {
                    event_id: record.event_id.clone(),
                    channel: record.channel_id.clone(),
                })
            }
        }
    }

    /// A mention whose turn has not settled yet stays at its non-terminal
    /// stage: terminalizing it is what made an interrupted mention permanently
    /// unanswered, because no redelivery and no later boot revisits a terminal
    /// row.
    fn defer_unsettled_turn(record: &EventRecord) -> DeliveryOutcome {
        Self::log_turn_deferral(record, "its turn has not settled");
        DeliveryOutcome::Deferred {
            event_id: record.event_id.clone(),
            channel: record.channel_id.clone(),
            reason: "turn_not_settled",
        }
    }

    fn log_turn_deferral(record: &EventRecord, why: &str) {
        log_err!("slack-clone-bot deferring event {}: {why}", record.event_id);
    }

    async fn owe_and_post(
        &self,
        record: &EventRecord,
        reply: String,
        source: ReplySource,
    ) -> Result<DeliveryOutcome> {
        // A failed post, an unreachable platform or a crash now all leave a row that says
        // exactly what is owed and to whom — which is what makes recovery a read rather than a
        // guess.
        if !self
            .ledger
            .advance(
                record.event_id.clone(),
                record.stage,
                Stage::ReplyPending,
                None,
                DetailWrite::Set(reply.clone()),
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
                DetailWrite::Clear,
            )
            .await?;
        Ok(DeliveryOutcome::Replied {
            event_id: record.event_id.clone(),
            channel: record.channel_id.clone(),
            reply_ts,
            source,
        })
    }

    /// Pay off a recorded reply debt, or discover it was already paid.
    async fn settle_reply_debt(&self, record: &EventRecord) -> Result<DeliveryOutcome> {
        if let Some(reply_ts) = self.already_posted(record).await? {
            self.settle(
                record,
                Stage::Replied,
                Some(reply_ts.clone()),
                DetailWrite::Clear,
            )
            .await?;
            return Ok(DeliveryOutcome::Duplicate {
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
                DetailWrite::Set(EventReason::ReplyLostAfterCommit.as_str().to_string()),
            )
            .await?;
            return Ok(DeliveryOutcome::ReplyLost {
                event_id: record.event_id.clone(),
                channel: record.channel_id.clone(),
            });
        };
        let reply_ts = self.post_reply(record, &reply).await?;
        self.settle(
            record,
            Stage::Replied,
            Some(reply_ts.clone()),
            DetailWrite::Clear,
        )
        .await?;
        Ok(DeliveryOutcome::Replied {
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
        detail: DetailWrite,
    ) -> Result<()> {
        if !self
            .ledger
            .advance(record.event_id.clone(), record.stage, to, reply_ts, detail)
            .await?
        {
            log_err!(
                "slack-clone-bot: event {} moved on before this handler settled it",
                record.event_id
            );
        }
        Ok(())
    }

    async fn observed_elsewhere(&self, record: &EventRecord) -> Result<DeliveryOutcome> {
        let current = self.ledger.get(record.event_id.clone()).await?;
        Ok(DeliveryOutcome::Duplicate {
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

    /// Open (or resume) the channel's session, creating it on first use.
    async fn open_session(&self, channel: &str) -> Result<LashSession> {
        threads::open_channel_session(&self.core, channel).await
    }

    fn classify(&self, event: &Event) -> (&'static str, Intent) {
        match event {
            Event::AppMention(_) => (KIND_APP_MENTION, Intent::Mention),
            Event::Message(message) => {
                if message.bot_id.is_some() {
                    // Including the bot's own replies. Without this guard the
                    // bot answers itself, forever.
                    return (
                        KIND_MESSAGE,
                        Intent::Ignore(EventReason::AppAuthoredMessage),
                    );
                }
                if message.user.is_none() {
                    return (KIND_MESSAGE, Intent::Ignore(EventReason::NoAuthor));
                }
                if events::mentions(&message.text, &self.identity.bot_user_id) {
                    // Slack sends both a `message` and an `app_mention` for a
                    // mention, under two `event_id`s. Deduplication cannot help
                    // here — the ids genuinely differ — so the bot picks the
                    // event whose meaning is unambiguous and drops the other.
                    return (
                        KIND_MESSAGE,
                        Intent::Ignore(EventReason::SupersededByAppMention),
                    );
                }
                (KIND_MESSAGE, Intent::Ambient)
            }
        }
    }

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

    fn session_lock(&self, record: &EventRecord) -> SessionLockLease {
        let key = record.thread_ts.as_deref().map_or_else(
            || session_id(&record.channel_id),
            |thread_ts| super::runtime::thread_session_id(&record.channel_id, thread_ts),
        );
        let mut locks = self.session_locks.lock_recover();
        let lock = Arc::clone(
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        );
        drop(locks);
        SessionLockLease {
            key,
            lock,
            registry: Arc::clone(&self.session_locks),
        }
    }
}

fn provider_failure(report: &lash::TurnReport) -> Option<ProviderFailure> {
    let issue = report.errors.iter().find(|issue| {
        issue.terminal_reason == Some(lash::direct::LlmTerminalReason::ProviderError)
    })?;
    Some(ProviderFailure {
        kind: issue.provider_failure_kind.unwrap_or_default(),
        code: issue.code.as_ref().map(|code| code.namespaced()),
        message: issue.message.clone(),
        retryable: issue.retryable.unwrap_or(false),
    })
}

/// One scoped reference to a routed-session lock.
///
/// The registry entry disappears after the last holder or waiter finishes, so
/// a long-lived bot does not retain one allocation for every thread ever seen.
struct SessionLockLease {
    key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    registry: SessionLockRegistry,
}

impl SessionLockLease {
    async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }
}

impl Drop for SessionLockLease {
    fn drop(&mut self) {
        let mut registry = self.registry.lock_recover();
        let is_current = registry
            .get(&self.key)
            .is_some_and(|registered| Arc::ptr_eq(registered, &self.lock));
        // The registry plus this lease are the final two strong references.
        // Holding the registry mutex closes the race with a new clone.
        if is_current && Arc::strong_count(&self.lock) == 2 {
            registry.remove(&self.key);
        }
    }
}

/// Used when a resumed mention's input was already answered by a committed
/// turn: a previous process ran it and died before its reply was recorded. Correlation is by the
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
fn reply_from_transcript(
    read_view: &lash::persistence::SessionReadView,
    input_id: &str,
) -> Option<String> {
    let mut turn_id: Option<TurnId> = None;
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
                turn_id = Some(TurnId::from(turn.to_string()));
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
    Mention,
    Ambient,
    Ignore(EventReason),
}
