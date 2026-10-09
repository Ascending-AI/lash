//! How a cell's failure is reported back to the model.
//!
//! Three things can go wrong with a cell, and they call for different responses.
//! The runtime can *refuse* it — a construct outside the dialect, an execution
//! bound, a budget — in which case the program is not wrong so much as not
//! allowed, and resending it unchanged is guaranteed to fail again. Or the
//! program can *run and fail* — a throw, a rejected tool call, a bad value —
//! in which case the shape was fine and the logic was not.
//! Host infrastructure can also fail while preparing or executing valid code;
//! that failure must not send the model off to rewrite its program.
//!
//! Until now both arrived as `Error:` followed by prose, and a model had to
//! infer which it was from the wording of the diagnostic. Guessing wrong is
//! expensive in both directions: reading a refusal as a runtime bug produces a
//! turn spent adding error handling around a construct that will never be
//! accepted, and reading a runtime bug as a refusal produces a turn spent
//! rewriting a program that was structurally fine.
//!
//! The executor decides the kind where the failure source is known and carries
//! it structurally in [`lash_core::CellFailure`]. This module renders that typed
//! value for the model; it does not encode or recover type information in prose.

use std::collections::BTreeSet;

use lash_core::{CellDefect, CellFailure, CellFailureKind};
use lash_kernel_doc::{Annotations, Datum, Name, Site, TaskIdentity};
use lash_kernel_vm::{Bound, BoundExceeded, RunError};
use lash_vm_client::service::DialectRefusal;

use crate::dialect::DialectPrompts;

/// The stable code of the refusal a model reads when a cell uses a binding
/// an earlier cell left that was not carried (`K-SES-003`).
pub const SESSION_BINDING_NOT_CARRIED: &str = "SESSION_BINDING_NOT_CARRIED";
/// The stable code of the error a cell ends with when it leaves tasks
/// unfinished, or failed with an error nothing awaited (`K-TASK-018`).
pub const CELL_TASKS_OUTSTANDING: &str = "CELL_TASKS_OUTSTANDING";
/// The stable code of a cell whose tasks wait on each other.
pub const CELL_DEADLOCK: &str = "CELL_DEADLOCK";
/// The stable code of a cell that passed an execution bound.
pub const CELL_BOUND_EXCEEDED: &str = "CELL_BOUND_EXCEEDED";

/// What the kernel or the dialect made of a cell that did not complete, as
/// typed facts: the cell's failure is rendered from one of these and from
/// nothing else.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellObservation {
    /// The dialect refused the source.
    Refused(DialectRefusal),
    /// The cell used a binding an earlier cell left that reached a function
    /// or a task, which no cell carries to the next (`K-SES-003`).
    BindingNotCarried { binding: String },
    /// A value no `catch` took ended the cell.
    Uncaught(Datum),
    /// The cell ended with tasks unfinished, or failed unobserved
    /// (`K-TASK-018`).
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    /// No task is ready and none waits on anything the host can answer.
    Deadlock { waiting: Vec<TaskIdentity> },
    /// The cell passed an execution bound; a bound error is not catchable.
    Bound(BoundExceeded),
}

/// Whether `text` names `name` as a whole identifier.
fn names(text: &str, name: &str) -> bool {
    let part = |character: char| character == '_' || character.is_alphanumeric();
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(part)
            && !text[at + name.len()..].chars().next().is_some_and(part)
    })
}

impl CellObservation {
    /// A dialect's refusal of `source`. A refusal at a name the session
    /// lost to `K-SES-003` is that rule's refusal: the name is unknown
    /// because it was not carried, and the model is told so.
    pub(crate) fn of_refusal(
        refusal: DialectRefusal,
        source: &str,
        not_carried: &BTreeSet<Name>,
    ) -> Self {
        let at = refusal
            .span
            .and_then(|(start, end)| source.get(start..end))
            .unwrap_or_default();
        match not_carried
            .iter()
            .find(|name| names(at, name.as_str()) || names(&refusal.message, name.as_str()))
        {
            Some(name) => Self::BindingNotCarried {
                binding: name.to_string(),
            },
            None => Self::Refused(refusal),
        }
    }

    /// How a run's error ended the cell. An uncaught unbound-variable error
    /// that names a binding the session lost to `K-SES-003` is that rule's
    /// refusal.
    pub(crate) fn of_run_error(error: RunError, not_carried: &BTreeSet<Name>) -> Self {
        match error {
            RunError::Uncaught(value) => {
                let text = datum_text(&value);
                match not_carried.iter().find(|name| names(&text, name.as_str())) {
                    Some(name) => Self::BindingNotCarried {
                        binding: name.to_string(),
                    },
                    None => Self::Uncaught(value),
                }
            }
            RunError::TasksOutstanding {
                unfinished,
                unobserved,
            } => Self::TasksOutstanding {
                unfinished,
                unobserved,
            },
            RunError::Deadlock { waiting } => Self::Deadlock { waiting },
            RunError::Bound(exceeded) => Self::Bound(exceeded),
        }
    }

    /// The observation's stable code: what a consumer matches on.
    pub(crate) fn code(&self) -> String {
        match self {
            Self::Refused(refusal) => refusal.code.clone(),
            Self::BindingNotCarried { .. } => SESSION_BINDING_NOT_CARRIED.to_owned(),
            Self::Uncaught(Datum::Error(error)) => error.kind.clone(),
            Self::Uncaught(_) => "uncaught".to_owned(),
            Self::TasksOutstanding { .. } => CELL_TASKS_OUTSTANDING.to_owned(),
            Self::Deadlock { .. } => CELL_DEADLOCK.to_owned(),
            Self::Bound(_) => CELL_BOUND_EXCEEDED.to_owned(),
        }
    }

    /// The cell's failure: its kind, its typed defect where a rule of the
    /// session's cells names one, and the message a model repairs from.
    /// `annotations` are the document's, which place a task's spawn in the
    /// source.
    pub(crate) fn failure(
        &self,
        source: &str,
        annotations: Option<&Annotations>,
        channel: crate::plugin::RlmChannel,
        prompts: &dyn DialectPrompts,
    ) -> CellFailure {
        let vocabulary = prompts.prompt_vocabulary();
        let place = |tasks: &[TaskIdentity]| -> String {
            tasks
                .iter()
                .map(|task| task_place(task, source, annotations))
                .collect::<Vec<_>>()
                .join("; ")
        };
        match self {
            Self::Refused(refusal) => {
                let mut message = format!("{}: {}", refusal.code, refusal.message);
                if let Some((start, _)) = refusal.span {
                    let (line, column) = line_column(source, start);
                    message.push_str(&format!(" (line {line}, column {column})"));
                    if let Some(text) = source.lines().nth(line - 1) {
                        message.push_str(&format!("\n  {}", text.trim_end()));
                    }
                }
                for repair in &refusal.repairs {
                    message.push_str(&format!("\nInstead: {repair}"));
                }
                // A closing delimiter line inside multiline source closes
                // the cell early, so the dialect receives a program that
                // stops mid-literal. A native tool call carries the program
                // as an argument, where no delimiter can truncate it.
                if !refusal.unsupported && channel == crate::plugin::RlmChannel::Cell {
                    message.push_str(&format!(
                        "\n\nA standalone `{}` line terminates the outer {} even inside multiline source text; construct that content without a standalone delimiter line.",
                        vocabulary.cell_tags.close, vocabulary.cell_noun,
                    ));
                }
                CellFailure::new(
                    if refusal.unsupported {
                        CellFailureKind::Policy
                    } else {
                        CellFailureKind::Program
                    },
                    message,
                )
            }
            Self::BindingNotCarried { binding } => CellFailure::new(
                CellFailureKind::Program,
                format!(
                    "{SESSION_BINDING_NOT_CARRIED}: `{binding}` was bound by an earlier {noun} to a value that holds a function or a task, and neither outlives the {noun} that created it, so `{binding}` is not bound here. {repair}",
                    noun = vocabulary.cell_noun,
                    repair = vocabulary.not_carried_repair,
                ),
            )
            .with_defect(CellDefect::BindingNotCarried {
                binding: binding.clone(),
            }),
            Self::Uncaught(value) => CellFailure::new(
                CellFailureKind::Program,
                format!("uncaught {}", datum_text(value)),
            ),
            Self::TasksOutstanding {
                unfinished,
                unobserved,
            } => {
                let mut message = format!(
                    "{CELL_TASKS_OUTSTANDING}: this {noun} ended while work it started was not awaited.",
                    noun = vocabulary.cell_noun
                );
                if !unfinished.is_empty() {
                    message.push_str(&format!(
                        "\nStill running ({}): {}",
                        unfinished.len(),
                        place(unfinished)
                    ));
                }
                if !unobserved.is_empty() {
                    message.push_str(&format!(
                        "\nFailed with an error nothing awaited ({}): {}",
                        unobserved.len(),
                        place(unobserved)
                    ));
                }
                message.push_str(&format!("\n{}", vocabulary.unjoined_task_repair));
                CellFailure::new(CellFailureKind::Program, message).with_defect(
                    CellDefect::TasksOutstanding {
                        unfinished: unfinished.iter().map(task_label).collect(),
                        unobserved: unobserved.iter().map(task_label).collect(),
                    },
                )
            }
            Self::Deadlock { waiting } => CellFailure::new(
                CellFailureKind::Program,
                format!(
                    "{CELL_DEADLOCK}: {} task(s) wait on each other and nothing can wake them: {}",
                    waiting.len(),
                    place(waiting)
                ),
            ),
            Self::Bound(exceeded) => {
                let (what, limit) = match &exceeded.bound {
                    Bound::Charge => (
                        "instruction budget exceeded".to_owned(),
                        Some(lash_vm_client::WorkerLimit::Fuel),
                    ),
                    Bound::Memory => (
                        "logical memory limit exceeded".to_owned(),
                        Some(lash_vm_client::WorkerLimit::Heap),
                    ),
                    Bound::CallDepth => (
                        "call depth limit exceeded".to_owned(),
                        Some(lash_vm_client::WorkerLimit::Depth),
                    ),
                    Bound::LiveTasks => ("too many tasks are live at once".to_owned(), None),
                    Bound::RequestsPerPark => (
                        "too many tool calls and sleeps were started at once".to_owned(),
                        None,
                    ),
                    Bound::JoinMembers => ("too many tasks were awaited together".to_owned(), None),
                    Bound::Guard { function } => (
                        format!("the library function {function} ran past its work limit"),
                        None,
                    ),
                };
                let mut failure = CellFailure::new(
                    CellFailureKind::Policy,
                    format!("{CELL_BOUND_EXCEEDED}: {what} (limit {})", exceeded.limit),
                );
                failure.worker_limit = limit;
                failure
            }
        }
    }
}

/// The 1-based line and column of a byte offset.
fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset.min(source.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |text| text.chars().count())
        + 1;
    (line, column)
}

/// A task as a typed label: the site that spawned it and which run of
/// that site it is.
pub(crate) fn task_label(task: &TaskIdentity) -> String {
    match task {
        TaskIdentity::Main => "main".to_owned(),
        TaskIdentity::Spawned(spawn) => format!(
            "{}#{}",
            crate::executor::site_label(&spawn.site),
            spawn.occurrence
        ),
    }
}

/// Which task, for the model: the source line of the async code it runs
/// when the document's annotations place its spawn, else its typed label.
fn task_place(task: &TaskIdentity, source: &str, annotations: Option<&Annotations>) -> String {
    let TaskIdentity::Spawned(spawn) = task else {
        return "the program itself".to_owned();
    };
    match annotations.and_then(|annotations| site_span(annotations, &spawn.site)) {
        Some((start, _)) => {
            let (line, _) = line_column(source, start);
            let text = source.lines().nth(line - 1).unwrap_or_default().trim();
            format!("the async code at line {line} (`{text}`)")
        }
        None => format!("the task {}", task_label(task)),
    }
}

/// The source span a front end recorded for the statement at `site`, or
/// for the nearest statement that encloses it.
fn site_span(annotations: &Annotations, site: &Site) -> Option<(usize, usize)> {
    annotations
        .nodes
        .iter()
        .filter(|node| node.site.unit == site.unit && site.path.starts_with(&node.site.path))
        .max_by_key(|node| node.site.path.len())
        .and_then(|node| node.data.get("span"))
        .and_then(|span| {
            let start = usize::try_from(span.get(0)?.as_u64()?).ok()?;
            let end = usize::try_from(span.get(1)?.as_u64()?).ok()?;
            Some((start, end))
        })
}

/// A value copied out of a run, as the text a failure message carries: an
/// error as its kind and message, anything else as its JSON reading.
pub(crate) fn datum_text(value: &Datum) -> String {
    match value {
        Datum::Error(error) if error.message.is_empty() => error.kind.clone(),
        Datum::Error(error) => format!("{}: {}", error.kind, error.message),
        Datum::Text(text) => text.clone(),
        other => crate::cell_value::datum_json(other).to_string(),
    }
}

/// Renders typed failure evidence followed by kind-specific recovery guidance.
pub(crate) fn render(failure: &CellFailure, cell_noun: &str) -> String {
    format!(
        "{}\n\n{}",
        failure.message,
        imperative(failure.kind, cell_noun)
    )
}

fn imperative(kind: CellFailureKind, cell_noun: &str) -> String {
    match kind {
        CellFailureKind::Policy => format!(
            "Next: the runtime refused this {cell_noun}; sending it again unchanged will be refused again. Rewrite it in the form named above."
        ),
        CellFailureKind::Program => format!(
            "Next: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected {cell_noun}."
        ),
        CellFailureKind::Host => format!(
            "Next: the host failed while handling this {cell_noun}. Retry it; if the failure persists, report the host problem."
        ),
    }
}
