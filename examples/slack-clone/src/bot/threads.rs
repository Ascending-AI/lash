//! Thread-session lifecycle: a Slack thread is a forked Lash session.
//!
//! A fork happens lazily on the first reply in a thread (mention or ambient),
//! at the channel revision that precedes the thread root: the revision the
//! committed turn that carried the root published, or the channel head recorded
//! when an ambient root was folded. Both are pinned, so a collection keeps
//! them. Turn-input application provenance correlates a Slack message to the
//! turn that committed it.
//!
//! The thread starts with its parent's folded context: the channel messages up
//! to the root that the fork boundary does not carry, with the root labelled,
//! lead the thread's first batch, ahead of the thread's own folded ambient
//! replies and its first mention.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use lash::persistence::StoreError;
use lash::{DurableSession, LashCore, Target};

use super::ledger::{EventLedger, EventRecord, IgnoreReason, Stage};
use super::runtime::{session_id, thread_session_id};

/// Root admission normally takes well under a second. Forty-five seconds leaves
/// ample room for scheduler and store contention while remaining a bounded wait
/// inside Slack's redelivery window.
pub const ROOT_ADMISSION_WAIT_BUDGET: Duration = Duration::from_secs(45);
/// Label the host puts in front of the thread root when it seeds the child.
///
/// It is prose because it is context for a model, and it is a constant because
/// the acceptance gates and the deterministic full-host driver both read it.
pub(crate) const THREAD_ROOT_SEED_PREFIX: &str =
    "Thread root (the channel message this thread replies to): ";
const ROOT_ADMISSION_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const ROOT_ADMISSION_MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Wall-clock time is a poor proxy for it: on a loaded runner a scheduling stall is
/// indistinguishable from a real wait, so a tight `tokio::time::timeout` around the call
/// reddens for the one reason the test does not care about.
/// These counters make the fact directly observable — `probes` counts loop turns that found no
/// authoritative root, and `budget` accumulates the wait each turn asked for (the requested
/// nap, never the observed elapsed time, so runner load cannot inflate it).
#[cfg(test)]
#[derive(Default)]
pub struct RootWaitObserver {
    missing_root_observed: tokio::sync::Notify,
    probes: std::sync::atomic::AtomicU64,
    budget_spent_nanos: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl RootWaitObserver {
    fn observe_missing_root(&self) {
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.missing_root_observed.notify_one();
    }

    fn observe_budget_spent(&self, nap: Duration) {
        self.budget_spent_nanos.fetch_add(
            u64::try_from(nap.as_nanos()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub async fn missing_root(&self) {
        self.missing_root_observed.notified().await;
    }

    /// How many loop turns found no authoritative root.
    pub fn probes(&self) -> u64 {
        self.probes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How much of the root-admission wait budget was asked for.
    pub fn budget_spent(&self) -> Duration {
        Duration::from_nanos(
            self.budget_spent_nanos
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// Result of opening the deterministic child behind a Slack thread.
pub enum ThreadSessionOpen {
    /// The thread's durable handle and the parent context its first batch carries.
    Ready {
        session: Box<DurableSession>,
        inherited_context: String,
    },
    /// Another writer holds the lane, so the contended admission must retry.
    AdmissionContended,
    /// Deterministic child ids are single-use; a deleted child stays retired.
    Retired,
    /// A root row exists and may still acquire an authoritative boundary.
    RootNotProcessed,
    /// No root row arrived within the bounded wait, or a terminal row proves
    /// that this bot will never admit the root.
    RootNotAvailable,
}

enum RootRoute {
    Ready(u64),
    Pending,
    NotSeen,
    PermanentlyUnavailable,
}

/// A missing authoritative root boundary is polled with bounded exponential
/// backoff. It is never replaced with the channel's current leaf: that leaf may
/// already include messages and turns posted after the thread root.
pub async fn open_thread_session(
    core: &LashCore,
    session_spec: &tokio::sync::RwLock<lash::SessionSpec>,
    ledger: &EventLedger,
    record: &EventRecord,
    #[cfg(test)] root_wait: &RootWaitObserver,
    root_wait_budget: Duration,
) -> Result<ThreadSessionOpen> {
    let thread_ts = record
        .thread_ts
        .as_deref()
        .context("thread route has no thread_ts")?;
    let thread_id = thread_session_id(&record.channel_id, thread_ts);

    let child_exists = core
        .session(lash::SessionId::fixture(thread_id.clone()))
        .durable()
        .await
        .context("durable handle for the thread child session")?
        .exists()
        .await
        .context("check whether the thread child session exists")?;
    let _channel = if !child_exists {
        let started = tokio::time::Instant::now();
        let mut backoff = ROOT_ADMISSION_INITIAL_BACKOFF;
        let (fork_revision, channel) = loop {
            let channel = match open_channel_session(core, session_spec, &record.channel_id).await {
                Ok(session) => session,
                Err(error) if error.is_contended() => {
                    #[cfg(test)]
                    root_wait.observe_missing_root();
                    let elapsed = started.elapsed();
                    if elapsed >= root_wait_budget {
                        return Ok(ThreadSessionOpen::RootNotProcessed);
                    }
                    let remaining = root_wait_budget.saturating_sub(elapsed);
                    let nap = backoff.min(remaining);
                    #[cfg(test)]
                    root_wait.observe_budget_spent(nap);
                    tokio::time::sleep(nap).await;
                    backoff = backoff.saturating_mul(2).min(ROOT_ADMISSION_MAX_BACKOFF);
                    continue;
                }
                Err(error) => return Err(error).context("open the channel session"),
            };
            let route = root_route(core, session_spec, ledger, record, thread_ts).await?;
            if let RootRoute::Ready(fork_revision) = route {
                break (fork_revision, channel);
            }
            if matches!(route, RootRoute::PermanentlyUnavailable) {
                return Ok(ThreadSessionOpen::RootNotAvailable);
            }
            #[cfg(test)]
            root_wait.observe_missing_root();

            let elapsed = started.elapsed();
            if elapsed >= root_wait_budget {
                return Ok(match route {
                    RootRoute::Pending => ThreadSessionOpen::RootNotProcessed,
                    RootRoute::NotSeen => ThreadSessionOpen::RootNotAvailable,
                    RootRoute::Ready(_) | RootRoute::PermanentlyUnavailable => {
                        unreachable!("handled before the deadline check")
                    }
                });
            }
            let remaining = root_wait_budget.saturating_sub(elapsed);
            let nap = backoff.min(remaining);
            #[cfg(test)]
            root_wait.observe_budget_spent(nap);
            tokio::time::sleep(nap).await;
            backoff = backoff.saturating_mul(2).min(ROOT_ADMISSION_MAX_BACKOFF);
        };
        let parent_id = lash::SessionId::fixture(session_id(&record.channel_id));
        let observed_processes = core
            .processes()
            .list_observed_by(
                &lash::process::SessionScope::new(parent_id.clone()),
                &lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .map(|record| record.process_id)
            .collect();
        // The recorded revision was pinned when it was recorded. The lineage
        // names no node: lash records the forked revision's leaf, and a
        // channel that has never run a turn has none.
        let creation = session_spec.read().await;
        match core
            .fork_at(
                &parent_id,
                Target::Revision(fork_revision),
                lash::ForkRequest {
                    session_id: lash::SessionId::fixture(thread_id.clone()),
                    relation: lash::persistence::SessionRelation::Fork {
                        source_session_id: parent_id.clone(),
                        source_node_id: None,
                    },
                    observed_processes,
                },
            )
            .await
        {
            Ok(_) => {}
            Err(lash::EmbedError::Store(StoreError::ForkSessionAlreadyExists { .. })) => {
                // Another process won the deterministic fork race. Opening the
                // existing child is the idempotent outcome.
            }
            Err(lash::EmbedError::Store(StoreError::SessionDeleted { .. })) => {
                return Ok(ThreadSessionOpen::Retired);
            }
            Err(error) => return Err(error).context("fork thread session"),
        }
        drop(creation);
        channel
    } else {
        match open_channel_session(core, session_spec, &record.channel_id).await {
            Ok(session) => session,
            Err(error) if error.is_contended() => {
                return Ok(ThreadSessionOpen::AdmissionContended);
            }
            Err(error) => return Err(error).context("open the channel session"),
        }
    };

    let session = match core
        .session(lash::SessionId::fixture(&thread_id))
        .durable()
        .await
    {
        Ok(session) => session,
        Err(error) if error.is_contended() => {
            return Ok(ThreadSessionOpen::AdmissionContended);
        }
        Err(lash::EmbedError::Store(StoreError::SessionDeleted { .. })) => {
            return Ok(ThreadSessionOpen::Retired);
        }
        Err(error) => return Err(error).context("open thread session"),
    };
    let inherited_context = inherited_thread_context(ledger, &session, record, thread_ts).await?;
    Ok(ThreadSessionOpen::Ready {
        session: Box::new(session),
        inherited_context,
    })
}

/// Resolve only durable evidence tied to the root itself.
async fn root_route(
    core: &LashCore,
    session_spec: &tokio::sync::RwLock<lash::SessionSpec>,
    ledger: &EventLedger,
    record: &EventRecord,
    thread_ts: &str,
) -> Result<RootRoute> {
    let mut root = ledger
        .channel_message(record.channel_id.clone(), thread_ts.to_string())
        .await
        .context("locate thread-root admission")?;
    if let Some(input_id) = root
        .as_ref()
        .filter(|root| root.fork_revision.is_none())
        .and_then(|root| root.input_id.clone())
    {
        // A turn application is durable even if the process died after pinning
        // its input and before projecting the revision into the Slack ledger.
        //
        // The durable repair reads the current store, including a root that
        // committed while this thread reply was waiting.
        let repair_view = match open_channel_session(core, session_spec, &record.channel_id).await {
            Ok(session) => session,
            Err(error) if error.is_contended() => {
                return Ok(RootRoute::Pending);
            }
            Err(error) => {
                return Err(error).context("open a current channel view for thread-root repair");
            }
        };
        try_retain_applied_turn_boundary(ledger, &repair_view, &input_id)
            .await
            .context("re-derive committed thread-root boundary")?;
        root = ledger
            .channel_message(record.channel_id.clone(), thread_ts.to_string())
            .await
            .context("reload repaired thread-root admission")?;
    }
    if let Some(root) = root {
        // A folded root's pinned pre-admission revision is valid fork
        // evidence until a committed turn carries the root; the ledger row is
        // the root's durability, even if the process died before advancing it
        // from Accepted to Folded.
        if let Some(revision) = root.fork_revision.or(root.admission_revision) {
            return Ok(RootRoute::Ready(revision));
        }
        return Ok(RootRoute::Pending);
    }

    let Some(root) = ledger
        .top_level_event(record.channel_id.clone(), thread_ts.to_string())
        .await
        .context("inspect unavailable thread root")?
    else {
        return Ok(RootRoute::NotSeen);
    };
    let has_route_evidence = root.input_id.is_some()
        || root.admission_revision.is_some()
        || root.fork_revision.is_some();
    // `superseded_by_app_mention` does not prove permanent unavailability: the
    // paired app_mention delivery for the same Slack message may still be racing.
    let paired_mention_may_arrive = matches!(
        root.stage,
        Stage::Ignored {
            reason: IgnoreReason::SupersededByAppMention
        }
    );
    if root.stage.is_terminal() && !has_route_evidence && !paired_mention_may_arrive {
        Ok(RootRoute::PermanentlyUnavailable)
    } else {
        Ok(RootRoute::Pending)
    }
}

/// Pin and record the revision published by the turn that consumed `input_id`.
///
/// The pin names the input, so lash resolves it to the root that actually
/// applied it, and it holds whether it is written before, during or after that
/// root's turn. The lookup uses typed application records. No Lash id is
/// parsed: the application names the turn, and every input applied by that
/// turn receives the same pinned revision.
///
/// A revision that cannot be read from a view that post-dates the application
/// is a defect, not a wait, and it fails loudly here rather than silently
/// skipping the ledger write. The polling repair path wants the opposite
/// answer and calls [`try_retain_applied_turn_boundary`].
pub async fn retain_applied_turn_boundary(
    ledger: &EventLedger,
    session: &DurableSession,
    input_id: &str,
) -> Result<()> {
    retain_boundary(ledger, session, input_id, Derivation::Required)
        .await
        .map(|_| ())
}

/// [`retain_applied_turn_boundary`] for a caller that is still waiting.
///
/// `Ok(false)` means the store holds no application for `input_id` *yet* — the
/// input is admitted but no turn has committed it — so nothing was recorded
/// and the caller should poll again. Only the thread-root repair may treat
/// that as a legal state: the root's admission is recorded at send time, ahead
/// of the commit that applies it.
pub async fn try_retain_applied_turn_boundary(
    ledger: &EventLedger,
    session: &DurableSession,
    input_id: &str,
) -> Result<bool> {
    retain_boundary(ledger, session, input_id, Derivation::MayBePending).await
}

/// Whether an unresolved revision is a defect or a "not yet".
#[derive(Clone, Copy, Eq, PartialEq)]
enum Derivation {
    Required,
    MayBePending,
}

async fn retain_boundary(
    ledger: &EventLedger,
    session: &DurableSession,
    input_id: &str,
    derivation: Derivation,
) -> Result<bool> {
    let durable = session;
    let applications = durable
        .turn_input_applications()
        .await
        .context("read turn-input applications for fork boundary")?;
    let Some(turn_id) = applications
        .iter()
        .find(|application| application.input_id == input_id)
        .map(|application| application.turn_id.clone())
    else {
        return Ok(false);
    };
    let target = Target::Input(lash::InputId::parse(input_id)?);
    durable
        .pin(target.clone())
        .await
        .with_context(|| format!("pin the channel turn that applied input {input_id}"))?;
    let Some(revision) = durable
        .revisions()
        .await
        .context("read the channel's retained revisions")?
        .into_iter()
        .find(|revision| revision.pinned_by.contains(&target))
        .map(|revision| revision.head_revision)
    else {
        if derivation == Derivation::MayBePending {
            return Ok(false);
        }
        bail!("the turn that applied input {input_id} published no retained revision");
    };
    let input_ids = applications
        .into_iter()
        .filter(|application| application.turn_id == turn_id)
        .map(|application| application.input_id.to_string())
        .collect();
    ledger
        .record_fork_revision_for_inputs(input_ids, revision)
        .await
        .context("record fork revision for committed Slack inputs")?;
    Ok(true)
}

/// Acquire the channel's durable handle, creating it on the channel's
/// first event. The bot owns its channel session ids and means create-or-use:
/// only `create` creates (FIG-4112), so an existing channel session — the
/// common case — is the arm where `session_spec`, the bot's default, does not
/// apply: the session keeps what it recorded.
pub(crate) async fn open_channel_session(
    core: &LashCore,
    session_spec: &tokio::sync::RwLock<lash::SessionSpec>,
    channel_id: &str,
) -> std::result::Result<DurableSession, lash::EmbedError> {
    let session_spec = session_spec.read().await;
    match core
        .session(session_id(channel_id))
        .create(lash::SessionCreation::root(session_spec.clone()))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => return Err(error),
    }
    core.session(session_id(channel_id)).durable().await
}

/// Pin and record the channel head revision preceding a folded admission. A
/// channel that has never run a turn is forkable too: its creation revision is
/// an ordinary retained revision.
pub async fn retain_admission_boundary(
    ledger: &EventLedger,
    session: &DurableSession,
    event_id: &str,
) -> Result<()> {
    let head = session
        .revisions()
        .await
        .context("read the channel's retained revisions")?
        .into_iter()
        .find(|revision| revision.head)
        .context("channel session records no head revision")?
        .head_revision;
    session
        .pin(Target::Revision(head))
        .await
        .with_context(|| format!("pin channel admission revision {head}"))?;
    ledger
        .record_admission_revision(event_id.to_string(), head)
        .await
        .context("record channel admission boundary")
}

/// The parent context a thread's first batch carries: the thread root, labelled,
/// and the channel context the fork boundary did not already carry.
///
/// Two problems, one pass over the same ledger rows.
///
/// **A thread root is a host concept.** Lash forks at a committed graph boundary;
/// it cannot know which of the messages inside that boundary the thread hangs
/// from, and it must not guess. The inherited prefix normally extends *past* the
/// root — a mention that folded the root committed the traffic after it and the
/// bot's answer too — so a child asked "what did the root say?" has nothing
/// distinguishing the root from the traffic that followed it, and answers about
/// the wrong message. The host owns the distinction, so the host writes it
/// down: one labelled line that names the root message.
///
/// **The root may not be in the prefix at all.** When the fork boundary is the
/// retained pre-admission node of a still-folded root, every channel message up
/// to and including the root is absent from the forked graph. Those are copied
/// here; the root among them arrives as the same labelled line.
///
/// The text is a pure function of the ledger and the two graphs, and the
/// ledger stores the first batch's members, so a redelivery, a second
/// open, or a boot recovery sends the same bytes.
async fn inherited_thread_context(
    ledger: &EventLedger,
    thread: &DurableSession,
    record: &EventRecord,
    thread_ts: &str,
) -> Result<String> {
    let committed_in_thread = thread
        .transcript()
        .await?
        .visible()
        .filter_map(|row| row.provenance.input_id.clone())
        .collect::<HashSet<_>>();
    let inherited = ledger
        .channel_context_through(record.channel_id.clone(), thread_ts.to_string())
        .await
        .context("read channel context through thread root")?;
    let mut context = String::new();
    for row in inherited {
        let Some(text) = row.input_text else {
            continue;
        };
        // The root is labelled whether or not the prefix already carries it:
        // the point of the line is the label, not the text. It starts and ends
        // its own line so the label names the root and nothing else.
        if row.message_ts == thread_ts {
            context.push_str(&format!("\n{THREAD_ROOT_SEED_PREFIX}{text}\n"));
            continue;
        }
        let already_in_graph = row.input_id.as_deref().is_some_and(|input_id| {
            committed_in_thread
                .iter()
                .any(|committed| committed == input_id)
        });
        if already_in_graph {
            continue;
        }
        context.push_str(&text);
        context.push('\n');
    }
    Ok(context)
}
