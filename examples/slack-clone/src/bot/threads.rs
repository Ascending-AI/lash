//! Thread-session lifecycle: a Slack thread is a forked Lash session.
//!
//! A fork happens lazily on the first reply in a thread (mention or ambient),
//! at the channel boundary that precedes the thread root: the boundary a
//! committed turn that carried the root retained, or the channel head recorded
//! when an ambient root was folded. Turn-input application provenance
//! correlates a Slack message to the turn that committed it.
//!
//! The thread starts with its parent's folded context: the channel messages up
//! to the root that the fork boundary does not carry, with the root labelled,
//! lead the thread's first send, ahead of the thread's own folded ambient
//! replies and its first mention.

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use lash::persistence::{ChronologicalPayload, HistoryAnchor, HistoryBudget, StoreError};
use lash::{LashCore, LashSession};

use super::ledger::{EventLedger, EventReason, EventRecord};
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
    /// The thread's session, and the parent context its first send carries.
    Ready {
        session: LashSession,
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
    Ready(String),
    Pending,
    NotSeen,
    PermanentlyUnavailable,
}

/// A missing authoritative root boundary is polled with bounded exponential
/// backoff. It is never replaced with the channel's current leaf: that leaf may
/// already include messages and turns posted after the thread root.
pub async fn open_thread_session(
    core: &LashCore,
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
        .session(thread_id.clone())
        .durable()
        .await
        .context("durable handle for the thread child session")?
        .exists()
        .await
        .context("check whether the thread child session exists")?;
    let channel = if !child_exists {
        let started = tokio::time::Instant::now();
        let mut backoff = ROOT_ADMISSION_INITIAL_BACKOFF;
        let (fork_node, channel) = loop {
            let channel = match open_channel_session(core, &record.channel_id).await {
                Ok(session) => session,
                Err(error) if anyhow_session_admission_contended(&error) => {
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
                Err(error) => return Err(error),
            };
            let route = root_route(core, ledger, record, thread_ts).await?;
            if let RootRoute::Ready(fork_node) = route {
                break (fork_node, channel);
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
        core.pin(&fork_node)
            .await
            .with_context(|| format!("retain channel boundary {fork_node} for thread fork"))?;
        let parent_id = session_id(&record.channel_id);
        let observed_processes = core
            .process_registry()
            .list_observed_by(
                &parent_id.clone().into(),
                &lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .map(|record| record.id)
            .collect();
        match core
            .fork_at(lash::ForkRequest {
                session_id: thread_id.clone().into(),
                node_id: fork_node.clone().into(),
                relation: lash::persistence::SessionRelation::Fork {
                    source_session_id: parent_id.into(),
                    source_node_id: fork_node.clone().into(),
                },
                observed_processes,
            })
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
        channel
    } else {
        match open_channel_session(core, &record.channel_id).await {
            Ok(session) => session,
            Err(error) if anyhow_session_admission_contended(&error) => {
                return Ok(ThreadSessionOpen::AdmissionContended);
            }
            Err(error) => return Err(error),
        }
    };

    let session = match core.session(&thread_id).open().await {
        Ok(session) => session,
        Err(error) if session_admission_contended(&error) => {
            return Ok(ThreadSessionOpen::AdmissionContended);
        }
        Err(lash::EmbedError::Store(StoreError::SessionDeleted { .. })) => {
            return Ok(ThreadSessionOpen::Retired);
        }
        Err(error) => return Err(error).context("open thread session"),
    };
    let inherited_context =
        inherited_thread_context(ledger, &channel, &session, record, thread_ts).await?;
    Ok(ThreadSessionOpen::Ready {
        session,
        inherited_context,
    })
}

pub(crate) fn session_admission_contended(error: &lash::EmbedError) -> bool {
    matches!(
        error,
        lash::EmbedError::Session(lash::SessionError::Store {
            source: StoreError::Contended,
            ..
        }) | lash::EmbedError::Store(StoreError::Contended)
    )
}

pub(crate) fn anyhow_session_admission_contended(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<lash::EmbedError>()
            .is_some_and(session_admission_contended)
    })
}

/// Resolve only durable evidence tied to the root itself.
async fn root_route(
    core: &LashCore,
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
        .filter(|root| root.fork_node_id.is_none())
        .and_then(|root| root.input_id.clone())
    {
        // A turn application is durable even if the process died after pinning
        // its boundary and before projecting that node into the Slack ledger.
        //
        // The repair reads the graph through a session opened now, not through
        // the caller's handle: that handle was opened when this thread reply
        // started waiting, and its graph predates the root turn this repair is
        // about. A snapshot that old can never carry the boundary being derived.
        let repair_view = match open_channel_session(core, &record.channel_id).await {
            Ok(session) => session,
            Err(error) if anyhow_session_admission_contended(&error) => {
                return Ok(RootRoute::Pending);
            }
            Err(error) => {
                return Err(error).context("open a current channel view for thread-root repair");
            }
        };
        try_retain_applied_turn_boundary(core, ledger, &repair_view, &input_id)
            .await
            .context("re-derive committed thread-root boundary")?;
        root = ledger
            .channel_message(record.channel_id.clone(), thread_ts.to_string())
            .await
            .context("reload repaired thread-root admission")?;
    }
    if let Some(root) = root {
        // A folded root's retained pre-admission boundary is valid fork
        // evidence until a committed turn carries the root; the ledger row is
        // the root's durability, even if the process died before advancing it
        // from Accepted to Folded.
        if let Some(node_id) = root.fork_node_id.or(root.admission_node_id) {
            return Ok(RootRoute::Ready(node_id));
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
    let has_route_evidence =
        root.input_id.is_some() || root.admission_node_id.is_some() || root.fork_node_id.is_some();
    // `superseded_by_app_mention` does not prove permanent unavailability: the
    // paired app_mention delivery for the same Slack message may still be racing.
    let paired_mention_may_arrive = root.detail.as_deref().and_then(EventReason::parse)
        == Some(EventReason::SupersededByAppMention);
    if root.stage.is_terminal() && !has_route_evidence && !paired_mention_may_arrive {
        Ok(RootRoute::PermanentlyUnavailable)
    } else {
        Ok(RootRoute::Pending)
    }
}

/// Pin and record the boundary produced by the turn that consumed `input_id`.
///
/// The lookup uses typed application records. No Lash id is parsed: the
/// application names the turn, and every input applied by that turn receives the
/// same retained leaf boundary.
///
/// The derivation reads the store's committed view, so a handle opened before
/// the turn committed is no excuse: a boundary that cannot be derived from a
/// view that post-dates the application is a defect, not a wait, and it fails
/// loudly here rather than silently skipping the retention and the ledger
/// write. The polling repair path wants the opposite answer and calls
/// [`try_retain_applied_turn_boundary`].
pub async fn retain_applied_turn_boundary(
    core: &LashCore,
    ledger: &EventLedger,
    session: &LashSession,
    input_id: &str,
) -> Result<()> {
    retain_boundary(core, ledger, session, input_id, Derivation::Required)
        .await
        .map(|_| ())
}

/// [`retain_applied_turn_boundary`] for a caller that is still waiting.
///
/// `Ok(false)` means the store holds no application for `input_id` *yet* — the
/// input is admitted but no turn has committed it — so nothing was retained
/// and the caller should poll again. Only the thread-root repair may treat
/// that as a legal state: the root's admission is recorded at send time, ahead
/// of the commit that applies it.
pub async fn try_retain_applied_turn_boundary(
    core: &LashCore,
    ledger: &EventLedger,
    session: &LashSession,
    input_id: &str,
) -> Result<bool> {
    retain_boundary(core, ledger, session, input_id, Derivation::MayBePending).await
}

/// Whether an underivable boundary is a defect or a "not yet".
#[derive(Clone, Copy, Eq, PartialEq)]
enum Derivation {
    Required,
    MayBePending,
}

async fn retain_boundary(
    core: &LashCore,
    ledger: &EventLedger,
    session: &LashSession,
    input_id: &str,
    derivation: Derivation,
) -> Result<bool> {
    let durable = session.durable();
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
    let Some(leaf) = committed_turn_boundary(&durable, &applications, &turn_id).await? else {
        if derivation == Derivation::MayBePending {
            return Ok(false);
        }
        bail!("committed turn application message is absent from the active channel graph");
    };
    core.pin(&leaf)
        .await
        .with_context(|| format!("pin committed channel turn boundary {leaf}"))?;
    let input_ids = applications
        .into_iter()
        .filter(|application| application.turn_id == turn_id)
        .map(|application| application.input_id.to_string())
        .collect();
    ledger
        .record_fork_node_for_inputs(input_ids, leaf)
        .await
        .context("record fork boundary for committed Slack inputs")?;
    Ok(true)
}

/// Resolve the graph boundary committed by `turn_id`, even when later turns
/// have advanced the session head. The parent of the next turn's first
/// application is this turn's committed leaf. Without a later application,
/// the current leaf is still this turn's boundary.
async fn committed_turn_boundary(
    durable: &lash::DurableSession,
    applications: &[lash::TurnInputApplication],
    turn_id: &lash::TurnId,
) -> Result<Option<String>> {
    let target_message_ids: HashSet<&str> = applications
        .iter()
        .filter(|application| application.turn_id == turn_id)
        .map(|application| application.committed_message_id.as_str())
        .collect();
    let later_application_ids: HashSet<&str> = applications
        .iter()
        .filter(|application| application.turn_id != turn_id)
        .map(|application| application.committed_message_id.as_str())
        .collect();
    let mut anchor = HistoryAnchor::Head;
    let mut leaf = None;
    let mut next_turn_boundary = None;
    loop {
        let page = durable
            .history(
                anchor,
                HistoryBudget {
                    max_nodes: NonZeroU32::new(128).context("positive history node limit")?,
                    max_bytes: NonZeroU64::new(32 * 1024 * 1024)
                        .context("positive history byte limit")?,
                },
            )
            .await
            .context("read committed channel ancestry for fork boundary")?;
        if leaf.is_none() {
            leaf = page.pinned_leaf.map(|id| id.to_string());
        }
        for node in &page.nodes {
            let Some(message_id) = node_message_id(&node.record) else {
                continue;
            };
            if target_message_ids.contains(message_id) {
                let boundary = match next_turn_boundary {
                    Some(boundary) => boundary,
                    None => leaf.context("committed ancestry node implies a leaf")?,
                };
                return Ok(Some(boundary));
            }
            if later_application_ids.contains(message_id) {
                next_turn_boundary = Some(
                    node.record
                        .parent_node_id
                        .as_ref()
                        .context("a later committed turn has no preceding graph boundary")?
                        .to_string(),
                );
            }
        }
        let Some(next) = page.next else {
            return Ok(None);
        };
        anchor = HistoryAnchor::Cursor(next);
    }
}

fn node_message_id(node: &lash::persistence::SessionNodeRecord) -> Option<&str> {
    node.message_id()
}

async fn open_channel_session(core: &LashCore, channel_id: &str) -> Result<LashSession> {
    let session = core
        .session(session_id(channel_id))
        .open()
        .await
        .with_context(|| format!("open session for channel {channel_id}"))?;
    ensure_forkable_channel_head(core, &session).await?;
    Ok(session)
}

/// Give a newly opened, turn-less channel a real retained boundary without a
/// model call or a user-visible message. A frame-open node alone has no
/// continuation checkpoint, so it cannot honestly be the source of a fork.
pub async fn ensure_forkable_channel_head(core: &LashCore, session: &LashSession) -> Result<()> {
    let session_id = session.session_id();
    if core
        .fork_points()
        .await
        .context("inspect channel fork points")?
        .iter()
        .any(|point| point.source_session_id == session_id)
    {
        return Ok(());
    }
    session
        .admin()
        .state()
        .append_plugin_body(
            "slack_clone_channel_anchor",
            serde_json::json!({ "purpose": "forkable channel baseline" }),
        )
        .await
        .context("commit forkable channel baseline")?;
    Ok(())
}

/// Retain and record the exact channel boundary preceding a folded admission.
pub async fn retain_admission_boundary(
    core: &LashCore,
    ledger: &EventLedger,
    session: &LashSession,
    event_id: &str,
) -> Result<()> {
    let node_id = session
        .read_view()
        .session_graph()
        .leaf_node_id
        .clone()
        .context("channel session has no admission boundary")?;
    core.pin(&node_id)
        .await
        .with_context(|| format!("retain channel admission boundary {node_id}"))?;
    ledger
        .record_admission_node(event_id.to_string(), node_id.to_string())
        .await
        .context("record channel admission boundary")
}

/// The parent context a thread's first send carries: the thread root, labelled,
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
/// ledger stores the first send's composed text, so a redelivery, a second
/// open, or a boot recovery sends the same bytes.
async fn inherited_thread_context(
    ledger: &EventLedger,
    channel: &LashSession,
    thread: &LashSession,
    record: &EventRecord,
    thread_ts: &str,
) -> Result<String> {
    let committed_in_thread: HashSet<String> = thread
        .read_view()
        .chronological_projection()
        .into_entries()
        .into_iter()
        .filter_map(|entry| match entry.payload {
            ChronologicalPayload::Message(message) => Some(message.id),
            ChronologicalPayload::ProtocolEvent(_) => None,
        })
        .collect();
    let applications = channel
        .durable()
        .turn_input_applications()
        .await
        .context("read channel applications for thread inheritance")?;
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
            applications.iter().any(|application| {
                application.input_id == input_id
                    && committed_in_thread.contains(&application.committed_message_id)
            })
        });
        if already_in_graph {
            continue;
        }
        context.push_str(&text);
        context.push('\n');
    }
    Ok(context)
}
