//! Replay-leg evidence for the H2 runner. The always-suspending server must
//! park the case's own Run invocations and restart them on their journal.
//! Every V7 proxy retains each frame it relays: a resumed attempt's Start
//! names how many entries Restate replays, and the frames that follow must be
//! exactly that invocation's journal prefix as `sys_journal` records it.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use lash_restate_test::protocol::generated::{
    RunCommandMessage, RunCompletionNotificationMessage, StartMessage,
    run_completion_notification_message,
};
use lash_restate_test::protocol::{Frame, MessageType};
use lash_upgrade_harness::e2e::evidence::{Evidence, JournalFact};
use lash_upgrade_harness::restate_view::RestateView;
use serde_json::json;

use super::proposal::wire_frame;

/// One relayed frame, keyed as the proxy named its retained artifact.
struct Wire {
    connection: u64,
    stream: u32,
    to_host: bool,
    observation: u64,
    path: std::path::PathBuf,
}

fn wires(directory: &Path) -> Result<Vec<Wire>> {
    let mut wires = Vec::new();
    for file in std::fs::read_dir(directory)? {
        let path = file?.path();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("wire-"))
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let parts: Vec<&str> = name.split('-').collect();
        let [connection, stream, direction, observation] = parts[..] else {
            anyhow::bail!("unrecognised wire artifact {}", path.display());
        };
        wires.push(Wire {
            connection: connection.parse()?,
            stream: stream.parse()?,
            to_host: direction == "in",
            observation: observation.parse()?,
            path,
        });
    }
    wires.sort_by_key(|wire| (wire.connection, wire.stream, wire.observation));
    Ok(wires)
}

/// `sys_journal`'s entry type for a replayed frame: `RunCommand` is
/// "Command: Run", `RunCompletionNotification` is "Notification: Run".
fn entry_type(ty: MessageType) -> Option<String> {
    let name = format!("{ty:?}");
    if let Some(command) = name.strip_suffix("Command") {
        return Some(format!("Command: {command}"));
    }
    name.strip_suffix("CompletionNotification")
        .or_else(|| name.strip_suffix("Notification"))
        .map(|notification| format!("Notification: {notification}"))
}

/// Check one replayed frame against the journal entry at its index.
fn same_entry(frame: &Frame, fact: &JournalFact) -> Result<()> {
    let expected = entry_type(frame.ty).context("replayed frame is not a journal entry")?;
    ensure!(
        fact.entry_type == expected,
        "replayed {:?} differs from journal entry {} ({})",
        frame.ty,
        fact.index,
        fact.entry_type
    );
    if frame.ty == MessageType::RunCommand {
        let command: RunCommandMessage = frame.decode()?;
        ensure!(
            fact.name.as_deref() == Some(command.name.as_str()),
            "replayed Run {:?} differs from journal entry {} ({:?})",
            command.name,
            fact.index,
            fact.name
        );
    }
    if frame.ty == MessageType::RunCompletionNotification {
        let notification: RunCompletionNotificationMessage = frame.decode()?;
        if let Some(run_completion_notification_message::Result::Value(value)) = notification.result
        {
            let recorded: Vec<u8> = serde_json::from_value(
                fact.value
                    .pointer("/Notification/Completion/Run/result/Success")
                    .cloned()
                    .context("journal Run completion has no recorded value")?,
            )?;
            ensure!(
                recorded == value.content,
                "replayed Run completion differs from journal entry {}",
                fact.index
            );
        }
    }
    Ok(())
}

/// One attempt of an invocation: its Start, the frames Restate sent after
/// it, and whether the SDK ended it with a Suspension.
struct Attempt {
    stream: String,
    start: StartMessage,
    incoming: Vec<Frame>,
    suspended: bool,
}

/// Every SDK attempt the proxies relayed so far, oldest connection first.
fn attempts(directory: &Path) -> Result<Vec<Attempt>> {
    let mut streams: BTreeMap<(u64, u32), (Vec<Wire>, Vec<Wire>)> = BTreeMap::new();
    for wire in wires(directory)? {
        let entry = streams.entry((wire.connection, wire.stream)).or_default();
        if wire.to_host {
            entry.0.push(wire);
        } else {
            entry.1.push(wire);
        }
    }
    let mut attempts = Vec::new();
    for ((connection, stream), (incoming, outgoing)) in streams {
        let incoming = incoming
            .iter()
            .map(|wire| wire_frame(&wire.path))
            .collect::<Result<Vec<_>>>()?;
        let Some(first) = incoming.first() else {
            continue;
        };
        if first.ty != MessageType::Start {
            continue;
        }
        let start: StartMessage = first.decode()?;
        let suspended = outgoing
            .iter()
            .map(|wire| wire_frame(&wire.path))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|frame| frame.ty == MessageType::Suspension);
        attempts.push(Attempt {
            stream: format!("{connection}-{stream}"),
            start,
            incoming: incoming.into_iter().skip(1).collect(),
            suspended,
        });
    }
    Ok(attempts)
}

/// Prove the case's journaled Run invocations suspended and replayed: some
/// attempt ended in a V7 Suspension, and every resumed attempt received its
/// invocation's recorded journal prefix, entry for entry, before running.
///
/// The wire is read before the journal, so the journal covers every entry a
/// relayed attempt replayed even while an invocation is still running.
pub(super) async fn assert_replayed(
    directory: &Path,
    view: &RestateView,
    evidence: &Evidence,
) -> Result<serde_json::Value> {
    let attempts = attempts(directory)?;
    let mut owners: BTreeMap<&str, &JournalFact> = BTreeMap::new();
    for fact in &evidence.journals {
        owners.entry(fact.invocation.as_str()).or_insert(fact);
    }
    ensure!(!owners.is_empty(), "replay leg has no journal evidence");
    let mut journals: BTreeMap<&str, BTreeMap<u64, JournalFact>> = BTreeMap::new();
    for (invocation, fact) in &owners {
        let entries = view.journal(&fact.work, invocation, fact.protocol).await?;
        journals.insert(
            invocation,
            entries
                .into_iter()
                .map(|entry| (entry.index, entry))
                .collect(),
        );
    }
    let mut invocations: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for attempt in &attempts {
        let invocation = attempt.start.debug_id.as_str();
        let Some(journal) = journals.get(invocation) else {
            continue;
        };
        let known = usize::try_from(attempt.start.known_entries)?;
        let mut replayed_commands = 0_u64;
        for (index, frame) in attempt.incoming.iter().take(known).enumerate() {
            let fact = journal.get(&u64::try_from(index)?).with_context(|| {
                format!("{invocation} replayed entry {index} that its journal lacks")
            })?;
            same_entry(frame, fact)
                .with_context(|| format!("{invocation} stream {}", attempt.stream))?;
            if frame.ty.is_command() && frame.ty != MessageType::InputCommand {
                replayed_commands += 1;
            }
        }
        let summary = invocations.entry(invocation.to_owned()).or_insert_with(
            || json!({"attempts":0,"suspensions":0,"replays":0,"replayed_commands":0}),
        );
        for (key, add) in [
            ("attempts", 1),
            ("suspensions", u64::from(attempt.suspended)),
            ("replays", u64::from(known > 1)),
            ("replayed_commands", replayed_commands),
        ] {
            summary[key] = json!(summary[key].as_u64().unwrap_or_default() + add);
        }
    }
    ensure!(
        invocations.values().any(|summary| summary["suspensions"]
            .as_u64()
            .is_some_and(|count| count > 0)
            && summary["replayed_commands"]
                .as_u64()
                .is_some_and(|count| count > 0)),
        "no journaled Run invocation both suspended and replayed a recorded command: {invocations:?}"
    );
    Ok(json!({"kind":"h2_replay_wire","invocations":invocations}))
}
