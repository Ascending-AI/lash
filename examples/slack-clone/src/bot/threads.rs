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
const ROOT_ADMISSION_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const ROOT_ADMISSION_MAX_BACKOFF: Duration = Duration::from_secs(8);

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
    #[cfg(test)] missing_root_observed: &tokio::sync::Notify,
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
            let route = root_route(core, ledger, &channel, record, thread_ts).await?;
            if let RootRoute::Ready(fork_node) = route {
                break fork_node;
            }
            if matches!(route, RootRoute::PermanentlyUnavailable) {
                return Ok(ThreadSessionOpen::RootNotAvailable);
            }
            #[cfg(test)]
            missing_root_observed.notify_one();

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
            tokio::time::sleep(backoff.min(remaining)).await;
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
    inherit_uncommitted_channel_context(ledger, &channel, &session, record, thread_ts).await?;
    Ok(ThreadSessionOpen::Ready(session))
}

/// Resolve only durable evidence tied to the root itself.
async fn root_route(
    core: &LashCore,
    ledger: &EventLedger,
    channel: &LashSession,
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
        retain_applied_turn_boundary(core, ledger, channel, &input_id)
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
pub async fn retain_applied_turn_boundary(
    core: &LashCore,
    ledger: &EventLedger,
    session: &LashSession,
    input_id: &str,
) -> Result<()> {
    let applications = session
        .turn_input_applications()
        .await
        .context("read turn-input applications for fork boundary")?;
    let Some(turn_id) = applications
        .iter()
        .find(|application| application.input_id == input_id)
        .map(|application| application.turn_id.clone())
    else {
        return Ok(());
    };
    let leaf = committed_turn_boundary(session, &applications, &turn_id)?;
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
        .context("record fork boundary for committed Slack inputs")
}

/// Resolve the graph boundary committed by `turn_id`, even when later turns
/// have advanced the session head.
///
/// Application records name the committed user message for every turn. On the
/// active graph path, the parent of the next turn's first application is the
/// exact leaf selected by this turn. When there is no later application, the
/// current leaf is still this turn's boundary (the ordinary under-lock path).
fn committed_turn_boundary(
    session: &LashSession,
    applications: &[lash::TurnInputApplication],
    turn_id: &lash::persistence::TurnId,
) -> Result<String> {
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
    let target_index = active_path
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            node_message_id(node)
                .filter(|message_id| target_message_ids.contains(message_id))
                .map(|_| index)
        })
        .next_back()
        .context("committed turn application message is absent from the active channel graph")?;

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
            .context("a later committed turn has no preceding graph boundary");
    }
    graph
        .leaf_node_id
        .clone()
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

/// Copy only channel admissions that predate the root but are not present in
/// the forked graph. This covers a root that was durably folded at its recorded
/// admission boundary but had not yet committed when the thread began.
async fn inherit_uncommitted_channel_context(
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
        let already_in_graph = context.input_id.as_deref().is_some_and(|input_id| {
            applications.iter().any(|application| {
                application.input_id == input_id
                    && committed_in_thread.contains(&application.committed_message_id)
            })
        });
        if already_in_graph {
            continue;
        }
        let Some(text) = context.input_text else {
            continue;
        };
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
