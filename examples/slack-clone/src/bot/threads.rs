//! Thread-session lifecycle: a Slack thread is a forked Lash session.
//!
//! A fork happens lazily on the first reply in a thread (mention or ambient).
//! That makes pre-mention replies durable directly in the child session and
//! avoids a second buffer state. The source boundary is recorded when channel
//! inputs commit: turn-input application provenance correlates the Slack
//! admission to the turn, and the turn's retained leaf is the forkable boundary.

use std::collections::HashSet;

use anyhow::{Context as _, Result, bail};
use lash::persistence::{ChronologicalPayload, LeaseOwnerIdentity, StoreError};
use lash::{LashCore, LashSession, TurnInput};

use super::ledger::{EventLedger, EventRecord};
use super::runtime::{session_id, thread_session_id};

/// Open an existing thread fork or create it at the honest channel boundary.
///
/// `Ok(None)` is the terminal conflict case: deterministic session ids are
/// single-use, so a deleted thread session must not be silently recreated.
pub async fn open_thread_session(
    core: &LashCore,
    ledger: &EventLedger,
    owner: &LeaseOwnerIdentity,
    record: &EventRecord,
) -> Result<Option<LashSession>> {
    let thread_ts = record
        .thread_ts
        .as_deref()
        .context("thread route has no thread_ts")?;
    let thread_id = thread_session_id(&record.channel_id, thread_ts);
    let channel = open_channel_session(core, owner, &record.channel_id).await?;

    let child_exists = core
        .session_exists(&thread_id)
        .await
        .context("check whether the thread child session exists")?;
    if !child_exists {
        let mut root = ledger
            .channel_message(record.channel_id.clone(), thread_ts.to_string())
            .await
            .context("locate thread-root admission")?;
        if let Some(input_id) = root
            .as_ref()
            .filter(|root| root.fork_node_id.is_none())
            .and_then(|root| root.input_id.clone())
        {
            // A turn application is durable even if the process died after
            // pinning its boundary and before projecting that node into the
            // Slack ledger. Repair that projection before considering the
            // honest current-head fallbacks below.
            retain_applied_turn_boundary(core, ledger, &channel, &input_id)
                .await
                .context("re-derive committed thread-root boundary")?;
            root = ledger
                .channel_message(record.channel_id.clone(), thread_ts.to_string())
                .await
                .context("reload repaired thread-root admission")?;
        }
        let fork_node = root
            .as_ref()
            .and_then(|root| root.fork_node_id.clone())
            .or_else(|| channel_snapshot_leaf(&channel));
        let Some(fork_node) = fork_node else {
            bail!("channel session has no committed head to fork");
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
            Err(lash::EmbedError::Store(StoreError::SessionDeleted { .. })) => return Ok(None),
            Err(error) => return Err(error).context("fork thread session"),
        }
    }

    let session = match core
        .session(&thread_id)
        .session_execution_owner(owner.clone())
        .open()
        .await
    {
        Ok(session) => session,
        Err(lash::EmbedError::Store(StoreError::SessionDeleted { .. })) => return Ok(None),
        Err(error) => return Err(error).context("open thread session"),
    };
    inherit_uncommitted_channel_context(ledger, &channel, &session, record, thread_ts).await?;
    Ok(Some(session))
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

async fn open_channel_session(
    core: &LashCore,
    owner: &LeaseOwnerIdentity,
    channel_id: &str,
) -> Result<LashSession> {
    let session = core
        .session(session_id(channel_id))
        .session_execution_owner(owner.clone())
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

fn channel_snapshot_leaf(session: &LashSession) -> Option<String> {
    session.read_view().session_graph().leaf_node_id.clone()
}

/// Copy only channel admissions that predate the root but are not present in
/// the forked graph. This covers the honest fallback where the root was queued
/// but had not yet committed when the thread began.
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
