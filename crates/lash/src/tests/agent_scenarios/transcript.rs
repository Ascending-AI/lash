//! Agent Scenario projection into the shared behavior-transcript vocabulary.
//!
//! Everything here is extracted from facts the run really produced: the streamed
//! `TurnActivity` sequence in emission order, the runtime-checkpoint commits the
//! session store actually accepted (through
//! [`lash_core::testing::checkpoint_observer`]), and the terminal process fold
//! read back from the process registry.
//!
//! Two deliberate omissions, both documented so a reviewer knows what a diff
//! here cannot tell them:
//!
//! - Prose / reasoning deltas and live token-usage events are dropped. They are
//!   provider-wire volume, not behavior, and they would push every transcript
//!   past its review budget. Settled usage remains pinned on each observed
//!   checkpoint commit.
//! - Durable commits are appended in commit order rather than interleaved with
//!   the activity stream. The harness observes the store seam and the activity
//!   sink separately and has no shared ordering between them, so pretending to
//!   interleave would be a fact the harness constructed.
//!
//! Commit groups are ordered **without reading a raw session id**. The observer
//! hands back commits sorted by session id, and a child session's id is a bare
//! UUID (subagents mint one per run), so ordering by it makes the transcript a
//! coin flip: the child's block sorts before or after the root's depending on the
//! UUID's first hex digit. Groups are therefore ordered root-first and then by the
//! *shape* of what each session committed, which contains no identifier at all.

use std::collections::BTreeMap;

use super::harness::AgentScenarioRun;
use lash_core::testing::behavior_transcript::{Actor, Attr, Component, Entry, IdKind, Kind, Usage};
use lash_core::testing::checkpoint_observer::{CheckpointComponentWriteKind, CheckpointWriteEvent};

/// Render one Agent Scenario run as a behavior transcript.
///
/// `root` is the semantic name pinned to the scenario's root session, so the
/// text reads `root` instead of an alias that shifts when an unrelated session
/// appears first.
pub(super) fn agent_scenario_transcript(run: &AgentScenarioRun, root: &str) -> String {
    let mut transcript = lash_core::testing::behavior_transcript::Transcript::new();
    transcript.pin(run.session_id.clone(), root.to_string());
    let root_actor = || Actor::session(run.session_id.clone());

    transcript.record(Entry::new(Kind::Ingress, root_actor(), "turn.start"));

    for activity in &run.streamed_events {
        if let Some(entry) = activity_entry(&activity.event, &run.session_id) {
            transcript.record(entry);
        }
    }

    for write in ordered_checkpoint_writes(&run.checkpoint_writes, &run.session_id) {
        transcript.record(commit_entry(write));
    }

    let mut processes = run.final_process_list.clone();
    processes.sort_by(|left, right| {
        (left.label.as_deref(), left.process_id.as_str())
            .cmp(&(right.label.as_deref(), right.process_id.as_str()))
    });
    for process in &processes {
        transcript.record(
            Entry::new(
                Kind::Outcome,
                Actor::process(process.process_id.clone()),
                format!("process.{}", process.status.label()),
            )
            .attr(Attr::text("label", process.label.as_deref().unwrap_or("-")))
            .attr(Attr::text("kind", &process.kind))
            .attr(Attr::flag("terminal", process.status.is_terminal())),
        );
    }

    transcript.render()
}

fn activity_entry(event: &lash_core::TurnEvent, session_id: &str) -> Option<Entry> {
    let actor = || Actor::session(session_id.to_string());
    Some(match event {
        lash_core::TurnEvent::ModelRequestStarted { protocol_iteration } => {
            Entry::new(Kind::Provider, actor(), "model.request")
                .attr(Attr::int("iteration", *protocol_iteration as u64))
        }
        lash_core::TurnEvent::ModelAttemptReset {
            assistant_prose_correlation_ids,
            reasoning_correlation_ids,
        } => Entry::new(Kind::Cancel, actor(), "model.attempt_reset")
            .attr(Attr::int(
                "prose",
                assistant_prose_correlation_ids.len() as u64,
            ))
            .attr(Attr::int(
                "reasoning",
                reasoning_correlation_ids.len() as u64,
            )),
        lash_core::TurnEvent::RetryStatus {
            attempt,
            max_attempts,
            reason,
            ..
        } => Entry::new(Kind::Fault, actor(), "provider.retry")
            .attr(Attr::int("attempt", *attempt as u64))
            .attr(Attr::int("of", *max_attempts as u64))
            .attr(Attr::text("reason", reason)),
        lash_core::TurnEvent::CodeBlockStarted { language, .. } => {
            Entry::new(Kind::Exec, actor(), "cell.start").attr(Attr::text("lang", language))
        }
        lash_core::TurnEvent::CodeBlockCompleted {
            success,
            error,
            tool_call_ids,
            ..
        } => {
            let mut entry = Entry::new(
                Kind::Exec,
                actor(),
                if *success { "cell.ok" } else { "cell.failed" },
            )
            .attr(Attr::int("calls", tool_call_ids.len() as u64));
            if let Some(error) = error {
                entry = entry.attr(Attr::text("error", error));
            }
            entry
        }
        lash_core::TurnEvent::ToolCallStarted { call_id, name, .. } => {
            let mut entry =
                Entry::new(Kind::Tool, actor(), "tool.start").attr(Attr::text("name", name));
            if let Some(call_id) = call_id {
                entry = entry.attr(Attr::id("call", IdKind::Call, call_id));
            }
            entry
        }
        lash_core::TurnEvent::ToolCallCompleted {
            call_id,
            name,
            output,
            ..
        } => {
            let mut entry = Entry::new(Kind::Tool, actor(), "tool.result")
                .attr(Attr::text("name", name))
                .attr(Attr::debug_token("outcome", &output.status()));
            if let Some(call_id) = call_id {
                entry = entry.attr(Attr::id("call", IdKind::Call, call_id));
            }
            entry
        }
        lash_core::TurnEvent::QueuedInputAccepted { applications } => {
            Entry::new(Kind::Ingress, actor(), "queued_input.accepted")
                .attr(Attr::int("inputs", applications.len() as u64))
        }
        lash_core::TurnEvent::QueuedMessagesCommitted {
            messages,
            checkpoint,
        } => {
            // Queued-message ingress persists host-authored messages before a
            // provider turn runs, so there is no recorded usage at this seam.
            Entry::new(Kind::Commit, actor(), "queued_messages.committed")
                .attr(Attr::int("messages", messages.len() as u64))
                .attr(Attr::debug_token("checkpoint", checkpoint))
                .usage(Usage::none())
        }
        lash_core::TurnEvent::FinalValue { value } => {
            Entry::new(Kind::Outcome, actor(), "turn.final_value").attr(Attr::json("value", value))
        }
        lash_core::TurnEvent::ToolValue { tool_name, value } => {
            Entry::new(Kind::Outcome, actor(), "turn.tool_value")
                .attr(Attr::text("name", tool_name))
                .attr(Attr::json("value", value))
        }
        lash_core::TurnEvent::Error { message } => {
            Entry::new(Kind::Outcome, actor(), "turn.error").attr(Attr::text("error", message))
        }
        // Provider-wire volume and plugin chatter: see the module doc.
        lash_core::TurnEvent::QueuedWorkStarted { .. }
        | lash_core::TurnEvent::AssistantProseDelta { .. }
        | lash_core::TurnEvent::ReasoningDelta { .. }
        | lash_core::TurnEvent::Usage { .. }
        | lash_core::TurnEvent::ChildUsage { .. }
        | lash_core::TurnEvent::PluginRuntime { .. } => return None,
    })
}

/// Group observed commits per session and order the groups deterministically:
/// the root session first, then every other session by the shape of what it
/// committed. No raw identifier takes part in the ordering, so a per-run UUID
/// cannot move a block.
fn ordered_checkpoint_writes<'writes>(
    writes: &'writes [CheckpointWriteEvent],
    root_session_id: &str,
) -> Vec<&'writes CheckpointWriteEvent> {
    let mut grouped = BTreeMap::<&str, Vec<&CheckpointWriteEvent>>::new();
    for write in writes {
        grouped
            .entry(write.attributed_session())
            .or_default()
            .push(write);
    }
    let mut groups = grouped
        .into_iter()
        .map(|(session_id, mut group)| {
            group.sort_by_key(|write| {
                (
                    write.revision_before,
                    write.revision_after,
                    write.commit_index,
                )
            });
            let is_root = session_id == root_session_id;
            (!is_root, commit_shape(&group), group)
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    groups.into_iter().flat_map(|(_, _, group)| group).collect()
}

/// An identifier-free description of one session's commit sequence, used only to
/// order groups stably.
fn commit_shape(group: &[&CheckpointWriteEvent]) -> String {
    let mut shape = String::new();
    for write in group {
        shape.push_str(&format!(
            "{}>{}",
            write.revision_before, write.revision_after
        ));
        for component in &write.components {
            shape.push(':');
            shape.push_str(component.component.as_str());
            match &component.kind {
                CheckpointComponentWriteKind::Stored { logical_bytes } => {
                    shape.push_str(&format!("=stored{}", logical_bytes.unwrap_or(0)));
                }
                CheckpointComponentWriteKind::UnchangedRef => shape.push_str("=ref"),
            }
        }
        shape.push('|');
    }
    shape
}

fn commit_entry(write: &CheckpointWriteEvent) -> Entry {
    let mut entry = Entry::commit(
        Actor::session(write.attributed_session().to_string()),
        write.revision_before,
        write.revision_after,
        Usage::new(
            write.usage.entries,
            write.usage.input_tokens,
            write.usage.output_tokens,
            write.usage.cache_read_input_tokens,
            write.usage.cache_write_input_tokens,
            write.usage.reasoning_output_tokens,
        ),
    );
    for component in &write.components {
        entry = entry.component(match &component.kind {
            CheckpointComponentWriteKind::Stored { logical_bytes } => {
                Component::stored(component.component.as_str(), *logical_bytes)
            }
            CheckpointComponentWriteKind::UnchangedRef => {
                Component::unchanged_ref(component.component.as_str())
            }
        });
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::testing::checkpoint_observer::{
        CHECKPOINT_WRITE_EVENT_SCHEMA, CheckpointComponent, CheckpointComponentWrite,
    };

    fn write(
        session_id: &str,
        revision_before: u64,
        component: CheckpointComponent,
    ) -> CheckpointWriteEvent {
        CheckpointWriteEvent {
            schema: CHECKPOINT_WRITE_EVENT_SCHEMA.to_string(),
            session_id: session_id.to_string(),
            attributed_session_id: None,
            cause_boundary_id: None,
            commit_index: revision_before as usize + 1,
            turn_index: revision_before as usize + 1,
            revision_before,
            revision_after: revision_before + 1,
            usage: Default::default(),
            components: vec![CheckpointComponentWrite {
                component,
                kind: CheckpointComponentWriteKind::Stored {
                    logical_bytes: Some(64),
                },
            }],
        }
    }

    /// Subagents mint a fresh UUID session id per run. Ordering commit groups by
    /// that id made the rendered block order depend on the UUID's first hex digit,
    /// which is how a passing snapshot turned into a coin flip in CI.
    #[test]
    fn commit_group_order_does_not_depend_on_a_per_run_child_session_id() {
        let root = "agent-scenario-root";
        let renders = ["0eeccccb-sorts-before-root", "feeccccb-sorts-after-root"]
            .into_iter()
            .map(|child| {
                let mut writes = vec![
                    write(child, 0, CheckpointComponent::ToolState),
                    write(root, 0, CheckpointComponent::TurnState),
                ];
                // The observer hands commits back sorted by raw session id; mimic
                // both possible orders it can produce.
                writes.sort_by(|left, right| left.session_id.cmp(&right.session_id));
                ordered_checkpoint_writes(&writes, root)
                    .into_iter()
                    .map(|write| {
                        format!(
                            "{}:{}",
                            if write.session_id == root {
                                "root"
                            } else {
                                "child"
                            },
                            write.components[0].component.as_str()
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            renders[0], renders[1],
            "commit group order changed with the child session's per-run id"
        );
        assert_eq!(
            renders[0],
            vec![
                "root:turn_state".to_string(),
                "child:tool_state".to_string()
            ],
            "the root session's commits must render first"
        );
    }
}
