//! Thread-session lifecycle: a Slack thread is a forked Lash session.
//!
//! A fork happens lazily on the first reply in a thread (mention or ambient).
//! That makes pre-mention replies durable directly in the child session and
//! avoids a second buffer state. The source boundary is recorded when channel
//! inputs commit: turn-input application provenance correlates the Slack
//! admission to the turn, and the turn's retained leaf is the forkable boundary.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use lash::persistence::{ChronologicalPayload, StoreError};
use lash::{LashCore, LashSession, TurnInput};

use super::ledger::{EventLedger, EventRecord};
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

/// Test-only record of what the root-admission wait actually did.
///
/// Tests that pin a fail-fast path need the fact "the root wait budget was not
/// spent". Wall-clock time is a poor proxy for it: on a loaded runner a
/// scheduling stall is indistinguishable from a real wait, so a tight
/// `tokio::time::timeout` around the call reddens for the one reason the test
/// does not care about. These counters make the fact directly observable —
/// `probes` counts loop turns that found no authoritative root, and `budget`
/// accumulates the wait each turn asked for (the requested nap, never the
/// observed elapsed time, so runner load cannot inflate it).
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

    /// Resolve the next time the wait loop sees a missing root.
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
    Ready(LashSession),
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

/// Open an existing thread fork or create it at the honest channel boundary.
///
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
    let channel = open_channel_session(core, &record.channel_id).await?;

    let child_exists = core
        .session_exists(&thread_id)
        .await
        .context("check whether the thread child session exists")?;
    if !child_exists {
        let started = tokio::time::Instant::now();
        let mut backoff = ROOT_ADMISSION_INITIAL_BACKOFF;
        let fork_node = loop {
            let route = root_route(core, ledger, record, thread_ts).await?;
            if let RootRoute::Ready(fork_node) = route {
                break fork_node;
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
        match core.fork_at(&fork_node, &thread_id).await {
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
    }

    let session = match core.session(&thread_id).open().await {
        Ok(session) => session,
        Err(lash::EmbedError::Store(StoreError::SessionDeleted { .. })) => {
            return Ok(ThreadSessionOpen::Retired);
        }
        Err(error) => return Err(error).context("open thread session"),
    };
    seed_thread_root_and_uncommitted_context(ledger, &channel, &session, record, thread_ts).await?;
    Ok(ThreadSessionOpen::Ready(session))
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
        let repair_view = open_channel_session(core, &record.channel_id)
            .await
            .context("open a current channel view for thread-root repair")?;
        try_retain_applied_turn_boundary(core, ledger, &repair_view, &input_id)
            .await
            .context("re-derive committed thread-root boundary")?;
        root = ledger
            .channel_message(record.channel_id.clone(), thread_ts.to_string())
            .await
            .context("reload repaired thread-root admission")?;
    }
    if let Some(root) = root {
        // A durable enqueue plus its retained pre-admission boundary is already
        // valid fork evidence even if the process died before advancing the
        // ledger row from Accepted to Folded.
        if let Some(node_id) = root.fork_node_id.or_else(|| {
            root.input_id
                .is_some()
                .then_some(root.admission_node_id)
                .flatten()
        }) {
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
    let paired_mention_may_arrive = root.detail.as_deref() == Some("superseded_by_app_mention");
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
/// For a caller holding the handle the turn just committed on, a boundary that
/// cannot be derived is a defect, not a wait: it fails loudly here rather than
/// silently skipping the retention and the ledger write. The polling repair path
/// wants the opposite answer and calls [`try_retain_applied_turn_boundary`].
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
/// `Ok(false)` means the turn's application is not on this handle's active path
/// *yet*, so nothing was retained and the caller should poll again. Only the
/// thread-root repair may treat that as a legal state: it reads applications
/// from the store while the graph comes from a handle opened earlier, so it can
/// legitimately see the application before the commit that carries it.
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
    let applications = session
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
    let Some(leaf) = committed_turn_boundary(session, &applications, &turn_id)? else {
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
        .map(|application| application.input_id)
        .collect();
    ledger
        .record_fork_node_for_inputs(input_ids, leaf)
        .await
        .context("record fork boundary for committed Slack inputs")?;
    Ok(true)
}

/// Resolve the graph boundary committed by `turn_id`, even when later turns
/// have advanced the session head.
///
/// Application records name the committed user message for every turn. On the
/// active graph path, the parent of the next turn's first application is the
/// exact leaf selected by this turn. When there is no later application, the
/// current leaf is still this turn's boundary (the ordinary under-lock path).
///
/// `None` means the turn's application is not on this handle's active graph
/// path *yet*. Application records are read from the store while the graph comes
/// from the handle's own state, so a caller polling with a handle opened before
/// the turn committed can legitimately see the application first. That is a
/// "come back later", not a broken graph, and the caller's wait loop is what
/// resolves it.
fn committed_turn_boundary(
    session: &LashSession,
    applications: &[lash::TurnInputApplication],
    turn_id: &lash::persistence::TurnId,
) -> Result<Option<String>> {
    let graph = session.read_view().session_graph().clone();
    let nodes_by_id: std::collections::HashMap<&str, _> = graph
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let mut active_path = Vec::new();
    let mut cursor = graph.leaf_node_id.as_deref();
    let mut visited = HashSet::new();
    while let Some(node_id) = cursor {
        if !visited.insert(node_id) {
            bail!("channel session graph contains a cycle at {node_id}");
        }
        let node = nodes_by_id
            .get(node_id)
            .with_context(|| format!("channel graph leaf path is missing node {node_id}"))?;
        active_path.push(*node);
        cursor = node.parent_node_id.as_deref();
    }
    active_path.reverse();

    let target_message_ids: HashSet<&str> = applications
        .iter()
        .filter(|application| &application.turn_id == turn_id)
        .map(|application| application.committed_message_id.as_str())
        .collect();
    let Some(target_index) = active_path
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            node_message_id(node)
                .filter(|message_id| target_message_ids.contains(message_id))
                .map(|_| index)
        })
        .next_back()
    else {
        return Ok(None);
    };

    let later_application_ids: HashSet<&str> = applications
        .iter()
        .filter(|application| &application.turn_id != turn_id)
        .map(|application| application.committed_message_id.as_str())
        .collect();
    if let Some(next_turn) = active_path.iter().skip(target_index + 1).find(|node| {
        node_message_id(node).is_some_and(|message_id| later_application_ids.contains(message_id))
    }) {
        return next_turn
            .parent_node_id
            .clone()
            .map(Some)
            .context("a later committed turn has no preceding graph boundary");
    }
    graph
        .leaf_node_id
        .clone()
        .map(Some)
        .context("committed turn has no graph leaf")
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
        .record_admission_node(event_id.to_string(), node_id)
        .await
        .context("record channel admission boundary")
}

/// Seed the thread root into the child, and copy the channel context the fork
/// boundary did not already carry.
///
/// Two problems, one pass over the same ledger rows.
///
/// **A thread root is a host concept.** Lash forks at a committed graph boundary;
/// it cannot know which of the messages inside that boundary the thread hangs
/// from, and it must not guess. The inherited prefix normally extends *past* the
/// root — an ambient root only commits when a later mention drains the channel
/// queue, and that same turn commits the mention and the bot's answer too — so a
/// child asked "what did the root say?" has nothing distinguishing the root from
/// the traffic that followed it, and answers about the wrong message. The host
/// owns the distinction, so the host writes it down: one labelled admission that
/// names the root message, seeded before the child's first turn runs.
///
/// **The root may not be in the prefix at all.** When the fork boundary is the
/// retained pre-admission node of a still-queued root, every channel message up
/// to and including the root is absent from the forked graph. Those are copied
/// here; the root among them arrives as the same labelled seed.
///
/// Both writes are ordinary queued inputs under deterministic source keys, so a
/// redelivery, a second open, or a boot recovery resolves to the admission Lash
/// already holds instead of duplicating a context line. That guard is the stored
/// `(session_id, source_key)` row, which a store vacuum tombstones — a host that
/// vacuums live sessions would re-seed on the next redelivery. This bot never
/// vacuums, so the guard holds for its lifetime.
async fn seed_thread_root_and_uncommitted_context(
    ledger: &EventLedger,
    channel: &LashSession,
    thread: &LashSession,
    record: &EventRecord,
    thread_ts: &str,
) -> Result<()> {
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
        .turn_input_applications()
        .await
        .context("read channel applications for thread inheritance")?;
    let inherited = ledger
        .channel_context_through(record.channel_id.clone(), thread_ts.to_string())
        .await
        .context("read channel context through thread root")?;
    for context in inherited {
        let Some(text) = context.input_text else {
            continue;
        };
        // The root is seeded whether or not the prefix already carries it: the
        // point of the seed is the label, not the text. Both newlines are the
        // host's own doing — queued text inputs concatenate into one user message
        // with no separator, so without them the label runs out of the copied
        // line ahead of it and into the reply behind it, and a label that starts
        // mid-line labels nothing.
        if context.message_ts == thread_ts {
            thread
                .enqueue(TurnInput::text(format!(
                    "\n{THREAD_ROOT_SEED_PREFIX}{text}\n"
                )))
                .id(format!(
                    "thread-root:{}:{}",
                    context.channel_id, context.message_ts
                ))
                .send()
                .await
                .context("seed the thread root into the fork")?;
            continue;
        }
        let already_in_graph = context.input_id.as_deref().is_some_and(|input_id| {
            applications.iter().any(|application| {
                application.input_id == input_id
                    && committed_in_thread.contains(&application.committed_message_id)
            })
        });
        if already_in_graph {
            continue;
        }
        thread
            .enqueue(TurnInput::text(text))
            .id(format!(
                "thread-inherited:{}:{}",
                context.channel_id, context.message_ts
            ))
            .send()
            .await
            .context("enqueue not-yet-committed channel context in thread fork")?;
    }
    Ok(())
}
