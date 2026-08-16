//! Per-event interpretation of language-execution records.
//!
//! Split from the viewer entry point because it is the one family of events
//! whose rendering is a typed match over its own event enum rather than a
//! generic record shape, and because it grows with the language surface rather
//! than with the viewer.

use lash_trace::{
    TraceLanguageChildExecution, TraceLanguageExecutionEvent, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionStatus, TraceRuntimeSubject,
};

pub(crate) fn language_execution_title(
    language: &str,
    event: &TraceLanguageExecutionEvent,
) -> String {
    match event {
        TraceLanguageExecutionEvent::ExecutionStarted { identity, .. } => {
            format!("{} started", entry_name(language, identity))
        }
        TraceLanguageExecutionEvent::ExecutionFinished {
            identity, status, ..
        } => format!(
            "{} {}",
            entry_name(language, identity),
            language_execution_status_str(*status)
        ),
        TraceLanguageExecutionEvent::NodeStarted { label, .. } => format!("{label} started"),
        TraceLanguageExecutionEvent::NodeCompleted { label, .. } => format!("{label} completed"),
        TraceLanguageExecutionEvent::NodeFailed { label, .. } => format!("{label} failed"),
        TraceLanguageExecutionEvent::BranchSelected { selected, .. } => {
            format!("branch selected: {}", branch_selection_str(*selected))
        }
        TraceLanguageExecutionEvent::ChildStarted {
            identity, child, ..
        } => format!(
            "{} started child {}",
            entry_name(language, identity),
            child_label(child)
        ),
    }
}

pub(crate) fn language_execution_summary(event: &TraceLanguageExecutionEvent) -> String {
    let mut parts = Vec::new();
    let identity = language_execution_identity(event);
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
    match event {
        TraceLanguageExecutionEvent::NodeStarted {
            node_id,
            occurrence,
            ..
        }
        | TraceLanguageExecutionEvent::NodeCompleted {
            node_id,
            occurrence,
            ..
        } => {
            parts.push(format!("node {node_id}"));
            parts.push(format!("occurrence {occurrence}"));
        }
        TraceLanguageExecutionEvent::NodeFailed {
            node_id,
            occurrence,
            error,
            ..
        } => {
            parts.push(format!("node {node_id}"));
            parts.push(format!("occurrence {occurrence}"));
            parts.push(format!("error {error}"));
        }
        TraceLanguageExecutionEvent::BranchSelected {
            node_id,
            occurrence,
            edge_id,
            ..
        } => {
            parts.push(format!("node {node_id}"));
            parts.push(format!("occurrence {occurrence}"));
            parts.push(format!("edge {edge_id}"));
        }
        TraceLanguageExecutionEvent::ChildStarted {
            occurrence, child, ..
        } => {
            parts.push(format!("occurrence {occurrence}"));
            parts.push(format!("child {}", subject_summary(&child.subject)));
        }
        TraceLanguageExecutionEvent::ExecutionFinished { error, .. } => {
            if let Some(error) = error {
                parts.push(format!("error {error}"));
            }
        }
        TraceLanguageExecutionEvent::ExecutionStarted { execution_map, .. } => {
            parts.push(format!("{} nodes", execution_map.nodes.len()));
            parts.push(format!("{} edges", execution_map.edges.len()));
        }
    }
    parts.join("\n")
}

pub(crate) fn language_execution_failed(event: &TraceLanguageExecutionEvent) -> bool {
    matches!(
        event,
        TraceLanguageExecutionEvent::NodeFailed { .. }
            | TraceLanguageExecutionEvent::ExecutionFinished {
                status: TraceLanguageExecutionStatus::Failed,
                ..
            }
    )
}

pub(crate) fn language_execution_identity(
    event: &TraceLanguageExecutionEvent,
) -> &TraceLanguageExecutionIdentity {
    match event {
        TraceLanguageExecutionEvent::ExecutionStarted { identity, .. }
        | TraceLanguageExecutionEvent::ExecutionFinished { identity, .. }
        | TraceLanguageExecutionEvent::NodeStarted { identity, .. }
        | TraceLanguageExecutionEvent::NodeCompleted { identity, .. }
        | TraceLanguageExecutionEvent::NodeFailed { identity, .. }
        | TraceLanguageExecutionEvent::BranchSelected { identity, .. }
        | TraceLanguageExecutionEvent::ChildStarted { identity, .. } => identity,
    }
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
