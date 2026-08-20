//! Per-event interpretation of language-execution records.
//!
//! Split from the viewer entry point because it is the one family of events
//! whose rendering is a typed match over its own event enum rather than a
//! generic record shape, and because it grows with the language surface rather
//! than with the viewer.

use lash_trace::{
    TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceRuntimeSubject,
};

pub(crate) fn language_execution_title(language: &str, event: &TraceLanguageExecution) -> String {
    match &event.payload {
        TraceLanguageExecutionPayload::ExecutionStarted { .. } => {
            format!("{} started", entry_name(language, &event.identity))
        }
        TraceLanguageExecutionPayload::ExecutionFinished { status, .. } => format!(
            "{} {}",
            entry_name(language, &event.identity),
            language_execution_status_str(*status)
        ),
        TraceLanguageExecutionPayload::NodeStarted { label, .. } => format!("{label} started"),
        TraceLanguageExecutionPayload::NodeCompleted { label, .. } => format!("{label} completed"),
        TraceLanguageExecutionPayload::NodeFailed { label, .. } => format!("{label} failed"),
        TraceLanguageExecutionPayload::BranchSelected { selected, .. } => {
            format!("branch selected: {}", branch_selection_str(*selected))
        }
        TraceLanguageExecutionPayload::ChildStarted { child, .. } => format!(
            "{} started child {}",
            entry_name(language, &event.identity),
            child_label(child)
        ),
    }
}

pub(crate) fn language_execution_summary(event: &TraceLanguageExecution) -> String {
    let mut parts = Vec::new();
    let identity = &event.identity;
    if !identity.entry_name.is_empty() {
        parts.push(format!("entry {}", identity.entry_name));
    }
    if !identity.entry_kind.is_empty() {
        parts.push(format!("kind {}", identity.entry_kind));
    }
    parts.push(format!("subject {}", subject_summary(&identity.subject)));
    if !identity.scope.session_id.is_empty() {
        parts.push(format!("session {}", identity.scope.session_id));
    }
    if let Some(turn_id) = &identity.scope.turn_id {
        parts.push(format!("turn {turn_id}"));
    }
    if !identity.module_ref.is_empty() {
        parts.push(format!("module {}", identity.module_ref));
    }
    match &event.payload {
        TraceLanguageExecutionPayload::NodeStarted {
            node_id,
            occurrence,
            ..
        }
        | TraceLanguageExecutionPayload::NodeCompleted {
            node_id,
            occurrence,
            ..
        } => {
            parts.push(format!("node {node_id}"));
            parts.push(format!("occurrence {occurrence}"));
        }
        TraceLanguageExecutionPayload::NodeFailed {
            node_id,
            occurrence,
            error,
            ..
        } => {
            parts.push(format!("node {node_id}"));
            parts.push(format!("occurrence {occurrence}"));
            parts.push(format!("error {error}"));
        }
        TraceLanguageExecutionPayload::BranchSelected {
            node_id,
            occurrence,
            edge_id,
            ..
        } => {
            parts.push(format!("node {node_id}"));
            parts.push(format!("occurrence {occurrence}"));
            parts.push(format!("edge {edge_id}"));
        }
        TraceLanguageExecutionPayload::ChildStarted {
            occurrence, child, ..
        } => {
            parts.push(format!("occurrence {occurrence}"));
            parts.push(format!("child {}", subject_summary(&child.subject)));
        }
        TraceLanguageExecutionPayload::ExecutionFinished { error, .. } => {
            if let Some(error) = error {
                parts.push(format!("error {error}"));
            }
        }
        TraceLanguageExecutionPayload::ExecutionStarted { execution_map, .. } => {
            parts.push(format!("{} nodes", execution_map.nodes.len()));
            parts.push(format!("{} edges", execution_map.edges.len()));
        }
    }
    parts.join("\n")
}

pub(crate) fn language_execution_failed(event: &TraceLanguageExecution) -> bool {
    matches!(
        &event.payload,
        TraceLanguageExecutionPayload::NodeFailed { .. }
            | TraceLanguageExecutionPayload::ExecutionFinished {
                status: TraceLanguageExecutionStatus::Failed,
                ..
            }
    )
}

pub(crate) fn entry_name<'a>(
    language: &'a str,
    identity: &'a TraceLanguageExecutionIdentity,
) -> &'a str {
    if identity.entry_name.is_empty() {
        language
    } else {
        &identity.entry_name
    }
}

pub(crate) fn child_label(child: &TraceLanguageChildExecution) -> String {
    child
        .entry_name
        .clone()
        .unwrap_or_else(|| subject_summary(&child.subject))
}

pub(crate) fn subject_summary(subject: &TraceRuntimeSubject) -> String {
    match subject {
        TraceRuntimeSubject::Process { process_id } => format!("process {process_id}"),
        TraceRuntimeSubject::Effect { effect_id, kind } => format!("effect {kind}:{effect_id}"),
    }
}

pub(crate) fn language_execution_status_str(status: TraceLanguageExecutionStatus) -> &'static str {
    match status {
        TraceLanguageExecutionStatus::Running => "running",
        TraceLanguageExecutionStatus::Completed => "completed",
        TraceLanguageExecutionStatus::Failed => "failed",
        TraceLanguageExecutionStatus::Cancelled => "cancelled",
    }
}

pub(crate) fn branch_selection_str(selection: lash_trace::TraceBranchSelection) -> &'static str {
    match selection {
        lash_trace::TraceBranchSelection::Then => "then",
        lash_trace::TraceBranchSelection::Else => "else",
    }
}
