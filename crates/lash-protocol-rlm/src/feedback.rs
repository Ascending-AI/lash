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

use std::collections::BTreeMap;

use lash_core::{CellDefect, CellFailure, CellFailureKind};
use lash_kernel_dialect::Diagnostic;
use lash_kernel_doc::Datum;
use lash_kernel_doc::SourceMap;
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::TaskLabel;
use lash_kernel_vm::{Bound, BoundExceeded, RunError};

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
/// The stable code of a cell whose worker died while it ran: ended by a
/// limit lash sets on the worker, or lost from outside it on every run the
/// cell had (FIG-6786, FIG-6806).
pub const CELL_WORKER_DIED: &str = "CELL_WORKER_DIED";

/// The failure of a cell whose run met a limit of its own at the worker
/// boundary. The CPU budget, the compute deadline and the held wall time
/// are limits lash set on the cell's workers, and the cgroup memory ceiling
/// one its host set: the same cell on the same inputs meets them again, so
/// the cell is refused with the bound it passed and its configured value,
/// as an execution bound is, and the model repairs from it (FIG-6523,
/// FIG-6786, FIG-6787).
pub(crate) fn worker_limit_failure(limit: lash_vm_client::WorkerLimit) -> CellFailure {
    let (what, bound, repair) = match limit {
        lash_vm_client::WorkerLimit::Cpu { bound_ms } => (
            "worker CPU time limit exceeded",
            format!("{bound_ms} ms"),
            "Reduce the computation or split it across smaller cells to fit the CPU time limit.",
        ),
        lash_vm_client::WorkerLimit::Deadline { bound_ms } => (
            "worker compute deadline exceeded",
            format!("{bound_ms} ms"),
            "Reduce the computation or split it across smaller cells to finish within the deadline.",
        ),
        lash_vm_client::WorkerLimit::Memory { bound_bytes } => (
            "worker memory limit exceeded",
            format!("{bound_bytes} bytes"),
            "Hold less data at once or split the work across smaller cells to fit the memory limit.",
        ),
        lash_vm_client::WorkerLimit::Wall { bound_ms } => (
            "worker wall time limit exceeded",
            format!("{bound_ms} ms"),
            "Split the work across smaller cells, each of which holds its worker for less time.",
        ),
        lash_vm_client::WorkerLimit::HostRead { bound_ms } => (
            "host read deadline exceeded",
            format!("{bound_ms} ms"),
            "A read the host answers while the cell runs took too long; call the tool as an awaited effect instead of a read, or ask for less.",
        ),
        other => {
            return CellFailure::new(CellFailureKind::Program, other.to_string())
                .with_worker_limit(other);
        }
    };
    CellFailure::new(
        CellFailureKind::Policy,
        format!("{CELL_BOUND_EXCEEDED}: {what} (limit {bound})\n{repair}"),
    )
    .with_worker_limit(limit)
}

/// How a worker that died running a cell ended, as its supervisor saw it
/// (FIG-6806).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerDeath {
    /// A limit lash sets on the worker ended it: an allocation abort at its
    /// address-space ceiling (`SIGABRT`), or its confinement's kill for a
    /// forbidden system call. The same cell meets it again.
    OwnLimit,
    /// No limit lash saw: a `SIGKILL` with no out-of-memory kill of the
    /// worker's cgroup behind it, another signal, an exit or a closed
    /// connection. An operator, a node drain or a neighbour's
    /// out-of-memory kill ends a worker so, whatever its cell runs.
    Lost,
}

/// The signal an allocation abort raises.
const SIGABRT: i32 = 6;

impl WorkerDeath {
    pub(crate) fn of(evidence: lash_protocol_types::engine_fault::SupervisorEvidence) -> Self {
        match evidence {
            lash_protocol_types::engine_fault::SupervisorEvidence::Signalled {
                signal: SIGABRT,
            }
            | lash_protocol_types::engine_fault::SupervisorEvidence::ForbiddenSyscall => {
                Self::OwnLimit
            }
            lash_protocol_types::engine_fault::SupervisorEvidence::Signalled { .. }
            | lash_protocol_types::engine_fault::SupervisorEvidence::Exited { .. }
            | lash_protocol_types::engine_fault::SupervisorEvidence::EndOfStream => Self::Lost,
        }
    }
}

fn death(evidence: lash_protocol_types::engine_fault::SupervisorEvidence) -> String {
    match evidence {
        lash_protocol_types::engine_fault::SupervisorEvidence::Signalled { signal } => {
            format!("killed by signal {signal}")
        }
        lash_protocol_types::engine_fault::SupervisorEvidence::Exited { code } => {
            format!("exited with code {code}")
        }
        lash_protocol_types::engine_fault::SupervisorEvidence::ForbiddenSyscall => {
            "killed for a forbidden system call".to_owned()
        }
        lash_protocol_types::engine_fault::SupervisorEvidence::EndOfStream => {
            "closed its connection".to_owned()
        }
    }
}

/// The failure of a cell whose worker a limit of lash's ended after it
/// began computing the cell ([`WorkerDeath::OwnLimit`]): the same cell
/// meets it again, so the death is the cell's end, recorded once, and the
/// model repairs the cell from it (FIG-6786, FIG-6806).
pub(crate) fn worker_died_failure(
    evidence: lash_protocol_types::engine_fault::SupervisorEvidence,
) -> CellFailure {
    let repair = if evidence
        == lash_protocol_types::engine_fault::SupervisorEvidence::ForbiddenSyscall
    {
        "The cell's run made a system call its worker's confinement forbids, and running it again meets it again. Do the step another way."
    } else {
        "The cell's run passed a limit of its worker, such as its memory ceiling, and running it again meets it again. Hold less data at once, or split the work across smaller cells."
    };
    CellFailure::new(
        CellFailureKind::Policy,
        format!(
            "{CELL_WORKER_DIED}: the worker running the cell died ({})\n{repair}",
            death(evidence)
        ),
    )
}

/// The failure of a cell whose worker was lost from outside it
/// ([`WorkerDeath::Lost`]) on every one of its `runs`. Nothing names the
/// cell as the cause: the model reads that the worker was lost, and
/// whether running the cell again is safe, given the tool calls of it that
/// completed and took effect (FIG-6806).
pub(crate) fn worker_lost_failure(
    evidence: lash_protocol_types::engine_fault::SupervisorEvidence,
    runs: u32,
    completed: &[lash_core::ExecutedCall],
) -> CellFailure {
    let ran = if runs == 1 {
        "lash ran the cell once and lost its worker".to_owned()
    } else {
        format!("lash ran the cell {runs} times, on a fresh worker each time, and lost each one")
    };
    let mut operations: Vec<&str> = Vec::new();
    for call in completed {
        if !operations.contains(&call.operation.as_str()) {
            operations.push(&call.operation);
        }
    }
    let rerun = if operations.is_empty() {
        "No tool call of the cell completed, so running the cell again is safe.".to_owned()
    } else {
        format!(
            "These tool calls of the cell completed and took effect, which nothing undoes: {}. Running the cell again runs them again; run again only what did not complete.",
            operations.join(", ")
        )
    };
    CellFailure::new(
        CellFailureKind::Host,
        format!(
            "{CELL_WORKER_DIED}: the worker running the cell was lost from outside the cell ({}); {ran}\nNo limit lash sets on a cell stopped it, so the cell's code is not the cause. {rerun}",
            death(evidence)
        ),
    )
}

/// What the kernel or the dialect made of a cell that did not complete, as
/// typed facts: the cell's failure is rendered from one of these and from
/// nothing else.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellObservation {
    /// The dialect refused the source.
    Refused(Diagnostic),
    /// The cell used a binding an earlier cell left that reached a function
    /// or a task, which no cell carries to the next (`K-SES-003`).
    BindingNotCarried {
        binding: String,
        /// Why it was not kept, as a clause.
        why: String,
    },
    /// A value no `catch` took ended the cell.
    Uncaught(Datum),
    /// The cell ended with tasks unfinished, or failed unobserved
    /// (`K-TASK-018`).
    TasksOutstanding {
        unfinished: Vec<TaskLabel>,
        unobserved: Vec<TaskLabel>,
        sources: Vec<(TaskLabel, lash_kernel_value::sites::PositionLabel)>,
    },
    /// No task is ready and none waits on anything the host can answer.
    Deadlock { waiting: Vec<TaskLabel> },
    /// The cell passed an execution bound; a bound error is not catchable.
    Bound(BoundExceeded),
}

impl CellObservation {
    /// Collect parent-side repair decisions before rendering the failure. The
    /// executor's recorded response retains every callback's identity and answer,
    /// including empty answers, so continuation reads them without calling hooks.
    pub(crate) fn failure_with_repairs(
        &self,
        source: &str,
        map: Option<&SourceMap>,

        prompts: &dyn DialectPrompts,
        packages: Option<&lash_vocabulary::package::NamespaceSnapshot>,
        ctx: &lash_core::RuntimeExecutionContext<'_>,
    ) -> CellFailure {
        use {
            lash_kernel_value::package::MemberKind,
            lash_kernel_value::package::PackageAdvertisement,
            lash_kernel_value::package::Visibility,
        };
        let mut grants = BTreeMap::new();
        for accepted in packages
            .into_iter()
            .flat_map(|snapshot| &snapshot.advertisements)
        {
            let PackageAdvertisement::Enumerated(package) = &accepted.advertisement else {
                continue;
            };
            for (name, member) in &package.members {
                if member.visibility == Visibility::Public
                    && matches!(&member.kind, MemberKind::Operation(_))
                {
                    let operation = lash_kernel_value::names::QualifiedOperation {
                        package: package.path.clone(),
                        name: name.clone(),
                    };
                    grants.insert(
                        operation.effect_name().to_string(),
                        lash_kernel_dialect::spell_operation(prompts.spelling(), &operation),
                    );
                }
            }
        }
        // Discovery contributes names only: it never resolves or admits a tool.
        let deferred = ctx
            .deferred_provider()
            .ok()
            .flatten()
            .map(|(provider, resolver)| {
                resolver.searchable_names(&lash_core::DeferredResolveContext::new(
                    provider,
                    ctx.owner(),
                    ctx.logical_run(),
                    ctx.run_capabilities(),
                    ctx.owner_config(),
                ))
            })
            .unwrap_or_default();
        let code = self.code();
        let span = match self {
            Self::Refused(refusal) => refusal.span().map(|span| (span.start, span.end)),
            _ => None,
        };
        let unknown_name = matches!(code.as_str(), "TS_UNKNOWN_BINDING" | "PY_UNKNOWN_NAME")
            .then(|| span.and_then(|(start, end)| source.get(start..end)))
            .flatten();
        let feedback = ctx.cell_failure_repairs(&lash_core::plugin::CellFailureDiagnostic {
            code: &code,
            span,
            unknown_name,
            grants: &grants,
            deferred: &deferred,
        });
        let mut failure = if let Self::Refused(refusal) = self {
            let mut refusal = refusal.clone();
            refusal.repairs.extend(
                feedback
                    .iter()
                    .flat_map(|reply| reply.repairs.iter().cloned()),
            );
            Self::Refused(refusal).failure(source, map, prompts)
        } else {
            let mut failure = self.failure(source, map, prompts);
            for repair in feedback.iter().flat_map(|reply| &reply.repairs) {
                failure.message.push_str(&format!("\nInstead: {repair}"));
            }
            failure
        };
        failure.feedback = feedback;
        failure
    }

    /// A dialect refusal remains the dialect's typed diagnosis. Source or
    /// diagnostic text cannot establish that a session binding was dropped.
    pub(crate) fn of_refusal(refusal: Diagnostic) -> Self {
        Self::Refused(refusal)
    }

    /// How a run's error ended the cell. An uncaught unbound-variable error
    /// that names a binding the session lost to `K-SES-003` is that rule's
    /// refusal.
    pub(crate) fn of_run_error(error: RunError, not_carried: &BTreeMap<Name, String>) -> Self {
        match error {
            RunError::Uncaught { error: value, .. } => {
                if let Datum::Error(error) = &value
                    && error.kind == "unbound_variable"
                    && let Datum::Text(binding) = &error.data
                    && let Some(why) = not_carried.get(&Name::new(binding.as_str()))
                {
                    Self::BindingNotCarried {
                        binding: binding.clone(),
                        why: why.clone(),
                    }
                } else {
                    Self::Uncaught(value)
                }
            }
            RunError::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            } => Self::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            },
            RunError::Deadlock { waiting } => Self::Deadlock { waiting },
            RunError::Bound(exceeded) => Self::Bound(exceeded),
        }
    }

    /// The observation's stable code: what a consumer matches on.
    pub(crate) fn code(&self) -> String {
        match self {
            Self::Refused(refusal) => refusal.code().to_owned(),
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
    /// `map` is the document's source map, checked against `source`, which
    /// places a task's spawn there.
    pub(crate) fn failure(
        &self,
        source: &str,
        map: Option<&SourceMap>,

        prompts: &dyn DialectPrompts,
    ) -> CellFailure {
        let vocabulary = prompts.prompt_vocabulary();
        let place = |tasks: &[TaskLabel],
                     sources: &[(TaskLabel, lash_kernel_value::sites::PositionLabel)]|
         -> String {
            tasks
                .iter()
                .map(|task| {
                    let body = sources.iter().find(|(identity, _)| identity == task);
                    task_place(task, source, map, body.map(|(_, site)| site))
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        match self {
            Self::Refused(refusal) => {
                let mut message = format!("{}: {}", refusal.code(), refusal.message);
                if let Some(span) = refusal.span() {
                    let (line, column) = line_column(source, span.start);
                    message.push_str(&format!(" (line {line}, column {column})"));
                    if let Some(text) = source.lines().nth(line - 1) {
                        message.push_str(&format!("\n  {}", text.trim_end()));
                    }
                }
                for repair in &refusal.repairs {
                    message.push_str(&format!("\nInstead: {repair}"));
                }
                let mut failure = CellFailure::new(
                    if refusal.kind() == lash_kernel_dialect::DiagnosticKind::Refusal {
                        CellFailureKind::Policy
                    } else {
                        CellFailureKind::Program
                    },
                    message,
                );
                if let lash_kernel_dialect::DiagnosticReason::NamespaceShadowing { shadowing, span } = &refusal.reason {
                    failure = failure.with_defect(CellDefect::NamespaceShadowedByLocal {
                        shadowing: shadowing.clone(),
                        span: span.map(|span| (span.start, span.end)),
                    });
                }
                failure
            }
            Self::BindingNotCarried { binding, why } => CellFailure::new(
                CellFailureKind::Program,
                format!(
                    "{SESSION_BINDING_NOT_CARRIED}: `{binding}` was bound by an earlier {noun} and was not kept: {why}. So `{binding}` is not bound here. {repair}",
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
                sources,
            } => {
                let mut message = format!(
                    "{CELL_TASKS_OUTSTANDING}: this {noun} ended while work it started was not awaited.",
                    noun = vocabulary.cell_noun
                );
                if !unfinished.is_empty() {
                    message.push_str(&format!(
                        "\nStill running ({}): {}",
                        unfinished.len(),
                        place(unfinished, sources)
                    ));
                }
                if !unobserved.is_empty() {
                    message.push_str(&format!(
                        "\nFailed with an error nothing awaited ({}): {}",
                        unobserved.len(),
                        place(unobserved, sources)
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
                    place(waiting, &[])
                ),
            ),
            Self::Bound(exceeded) => {
                let (what, limit) = match &exceeded.bound {
                    Bound::Charge => (
                        "instruction budget exceeded".to_owned(),
                        Some(lash_vm_client::WorkerLimit::Fuel),
                    ),
                    Bound::Memory => (
                        "memory limit exceeded (arena capacity in bytes)".to_owned(),
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
                    Bound::Guard { .. } => (
                        "work limit exceeded".to_owned(),
                        None,
                    ),
                    Bound::Effects => (
                        "the cell started too many tool calls and sleeps over its life".to_owned(),
                        None,
                    ),
                    Bound::CheckpointBytes => (
                        "the cell saved too much state over its life".to_owned(),
                        None,
                    ),
                };
                let function = exceeded.function.as_ref()
                    .map(|name| format!(" in library function `{name}`"))
                    .unwrap_or_default();
                let location = exceeded.site.as_deref().and_then(|site| {
                    let map = map.filter(|map| map.validate_source(source).is_ok())?;
                    let entry = map.sites.iter().find(|entry| entry.site == *site)?;
                    let offset = entry.location.start();
                    let (line, column) = line_column(source, offset);
                    let text = source.lines().nth(line - 1).unwrap_or_default().trim();
                    let definition = map.sites.iter().find(|entry| {
                        entry.site.unit == site.unit && entry.site.path.is_empty()
                    }).and_then(|entry| entry.location.ranges().first()).and_then(|span| {
                        source.get(span.start..span.end)?.lines().next()
                    }).unwrap_or(text).trim();
                    Some(format!(" in guest function `{definition}` at line {line}, column {column}\n  {text}"))
                }).unwrap_or_default();
                let repair = match exceeded.bound {
                    Bound::Charge => "Reduce the computation or split it across smaller cells to fit the instruction budget.",
                    Bound::Memory => "Keep fewer or smaller values live to fit the memory limit.",
                    Bound::Guard { .. } => "Reduce the function's input or split its work into smaller calls to fit its work limit.",
                    Bound::CallDepth => "Reduce nested calls or replace recursion with an iterative computation.",
                    Bound::LiveTasks => "Start fewer tasks at once and await them before starting more.",
                    Bound::RequestsPerPark => "Start fewer tool calls and sleeps at once; await some before starting more.",
                    Bound::JoinMembers => "Await fewer tasks together, in smaller groups.",
                    Bound::Effects => "Split the work across several cells, each starting fewer tool calls and sleeps.",
                    Bound::CheckpointBytes => "Split the work across several cells, and keep fewer or smaller values live across tool calls and sleeps.",
                };
                let mut failure = CellFailure::new(
                    CellFailureKind::Policy,
                    format!("{CELL_BOUND_EXCEEDED}: {what}{function} (limit {}){location}\n{repair}", exceeded.limit),
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
pub(crate) fn task_label(task: &TaskLabel) -> String {
    lash_kernel_value::sites::ObservationReader::task_label(task)
}

/// Which task, for the model: the source line of the async code it runs
/// when the document's source map places its spawn or its source body
/// through a library adapter, else its typed label.
fn task_place(
    task: &TaskLabel,
    source: &str,
    map: Option<&SourceMap>,
    body: Option<&lash_kernel_value::sites::PositionLabel>,
) -> String {
    if lash_kernel_value::sites::ObservationReader::task_is_root(task) {
        return "the program itself".to_owned();
    }
    match map
        .and_then(|map| lash_kernel_value::sites::ObservationReader::task_source(task, map, body))
    {
        Some(start) => {
            let (line, _) = line_column(source, start);
            let text = source.lines().nth(line - 1).unwrap_or_default().trim();
            format!("the async code at line {line} (`{text}`)")
        }
        None => format!("the task {}", task_label(task)),
    }
}

/// A value copied out of a run, as the text a failure message carries: an
/// error as its kind and message, anything else as its JSON reading.
pub(crate) fn datum_text(value: &Datum) -> String {
    match value {
        Datum::Error(error)
            if error.kind == crate::executor::TOOL_FAILED
                && let Datum::Record(fields) = &error.data
                && let Some(Datum::Text(code)) = fields
                    .iter()
                    .find_map(|(name, value)| (name == "code").then_some(value)) =>
        {
            let kind = format!("{} ({code})", error.kind);
            if error.message.is_empty() {
                kind
            } else {
                format!("{kind}: {}", error.message)
            }
        }
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
            "Next: the runtime refused this {cell_noun}; sending it again unchanged will be refused again. Correct the reported constraint, then send the revised {cell_noun}."
        ),
        CellFailureKind::Program => format!(
            "Next: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected {cell_noun}."
        ),
        CellFailureKind::Host => format!(
            "Next: the host failed while handling this {cell_noun}. Retry it; if the failure persists, report the host problem."
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
    use lash_kernel_doc::{
        FunctionRegistry, NativeCall, NativeError, NativeFunction, Value, parse_definition,
        parse_document,
    };

    use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
    use lash_kernel_vm::{Bounds, CellStart, End, Host, KernelMachine, Machine, Program, Target};

    use super::*;

    /// FIG-6894: the refusal names the capacity meter and its byte unit,
    /// whether the run crossed its bound in a library call or in guest code.
    #[test]
    fn a_memory_refusal_names_arena_capacity_in_bytes() {
        let prompts = crate::CellDialect::typescript().prompts();
        for function in [None, Some("ts.string.repeat"), Some("ts.array.construct")] {
            let observation = CellObservation::Bound(BoundExceeded {
                bound: Bound::Memory,
                limit: 67_108_864,
                site: None,
                function: function.map(|name| {
                    lash_kernel_value::names::FunctionName::new(name).expect("function name")
                }),
            });
            let failure = observation.failure("", None, prompts.as_ref());
            let context = function
                .map(|name| format!(" in library function `{name}`"))
                .unwrap_or_default();
            assert_eq!(
                failure.message.lines().next(),
                Some(format!(
                    "CELL_BOUND_EXCEEDED: memory limit exceeded (arena capacity in bytes){context} (limit 67108864)"
                ).as_str()),
            );
            assert_eq!(
                failure.worker_limit,
                Some(lash_vm_client::WorkerLimit::Heap)
            );
        }
    }

    /// FIG-7430: depth feedback names the authored function and its checked
    /// source coordinate, including when a helper used the final depth unit.
    #[test]
    fn a_depth_refusal_names_the_guest_function_at_its_source_coordinate() {
        use lash_kernel_doc::{SourceLanguage, SourceSite, Span};
        use lash_kernel_value::sites::{Site, Unit};
        let definition = parse_definition("function descend(self: Any) -> Any\nkernel 1\nmetering statements\nbody { do apply self(self) }\n").expect("definition");
        let function = definition.identity();
        let definition_text =
            serde_json::to_string(&lash_kernel_doc::print_definition(&definition))
                .expect("definition text");
        let document = parse_document(&format!("kernel 1\nnumbers by_spelling\ndefinition {definition_text}\nmain {{ do call descend(&descend) }}")).expect("document");
        let site = Site::new(Unit::Library(function), vec![0]);
        for (dialect, language, source) in [
            (
                crate::CellDialect::typescript(),
                SourceLanguage::TypeScript,
                "function descend(n) {\n    return descend(n - 1);\n}",
            ),
            (
                crate::CellDialect::python(),
                SourceLanguage::Python,
                "def descend(n):\n    return descend(n - 1)\n",
            ),
        ] {
            let offset = source.find("return").expect("call line");
            let map = SourceMap::new(
                &document,
                source,
                language,
                vec![
                    SourceSite {
                        site: Site::new(site.unit.clone(), vec![]),
                        location: lash_kernel_doc::SourceLocation::Spelled {
                            ranges: lash_kernel_doc::NonEmptySpans::new(
                                Span::new(0, source.len()).expect("function range"),
                            ),
                        },
                    },
                    SourceSite {
                        site: site.clone(),
                        location: lash_kernel_doc::SourceLocation::Spelled {
                            ranges: lash_kernel_doc::NonEmptySpans::new(
                                Span::new(offset, source.len()).expect("call range"),
                            ),
                        },
                    },
                ],
            )
            .expect("checked source map");
            let observation = CellObservation::Bound(BoundExceeded {
                bound: Bound::CallDepth,
                limit: 32,
                function: None,
                site: Some(Box::new(site.clone())),
            });
            let prompts = dialect.prompts();
            let failure = observation.failure(source, Some(&map), prompts.as_ref());
            assert!(
                failure.message.contains("descend(n)"),
                "{}",
                failure.message
            );
            assert!(
                failure.message.contains("line 2, column 5"),
                "{}",
                failure.message
            );
            assert!(
                !failure.message.contains("library function"),
                "{}",
                failure.message
            );
            assert_eq!(
                failure.worker_limit,
                Some(lash_vm_client::WorkerLimit::Depth)
            );
            let stale = observation.failure(
                &format!("{source}\n# changed"),
                Some(&map),
                prompts.as_ref(),
            );
            assert!(
                !stale.message.contains("line 2"),
                "stale maps name no source location"
            );
        }
    }

    struct Guarded {
        memory: bool,
    }

    impl NativeFunction for Guarded {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            if self.memory {
                // Room past every bound: the run ends at its memory bound.
                return Err(lash_kernel_doc::Retry::Room {
                    wanted: lash_kernel_doc::heap::LogicalBytes(u64::MAX),
                }
                .into());
            }
            call.counter.spend(1)?;
            Ok(Value::Null)
        }
    }

    struct World;

    /// The pages a run seals, kept in memory.
    #[derive(Default)]
    struct Pages(BTreeMap<PageId, PageAnswer>);

    impl PageSource for Pages {
        fn read(
            &mut self,
            pages: &[PageRef],
        ) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
            Ok(pages
                .iter()
                .map(|page| {
                    self.0
                        .get(&page.id)
                        .cloned()
                        .ok_or(HeapFault::Missing { page: page.id })
                })
                .collect())
        }

        fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
            let mut tokens = body.kind.0.to_be_bytes().to_vec();
            tokens.extend_from_slice(&body.tokens);
            let page = PageRef {
                id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
                grant: PageGrant::NONE,
            };
            self.0.insert(
                page.id,
                PageAnswer {
                    page,
                    kind: body.kind,
                    tokens: body.tokens.into(),
                    children: body.children,
                },
            );
            Ok(page)
        }
    }

    impl Host for World {
        fn print(&mut self, _: &Datum) {
            panic!("no print")
        }
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }

    /// FIG-5763 / K-LIB-008: a guard observation names the definition,
    /// survives recording the end, and offers repair without an unnamed form.
    #[test]
    fn a_guard_observation_names_the_function_and_its_repair() {
        for (guard, memory, charge, repair) in [
            (true, false, 1, "smaller calls"),
            (false, true, 1, "fewer or smaller values"),
            (false, false, 10_000, "instruction budget"),
        ] {
            let mut registry = FunctionRegistry::new();
            let definition = parse_definition(&format!(
                "function work.spin() -> Any\nkernel 1\ncharge {charge}\n{}native\n",
                if guard { "guard \"step\" 0\n" } else { "" },
            ))
            .expect("definition");
            let function = registry
                .register(definition.clone(), Some(Arc::new(Guarded { memory })))
                .expect("registered");
            let definition_text =
                serde_json::to_string(&lash_kernel_doc::print_definition(&definition)).unwrap();
            let document = parse_document(&format!("kernel 1\nnumbers by_spelling\ndefinition {definition_text}\nnative @{function}\nmain {{ return work.spin() }}")).unwrap();
            let link = lash_kernel_value::Link {
                helper_fills: Default::default(),
                document: document.identity(),
                fills: Default::default(),
            };
            let link_id = link.identity();
            let program = Program::prepare(
                link_id,
                [(document.identity(), Arc::new(document))].into(),
                &[(link_id, link)].into(),
                PageId([8; 32]),
                lash_kernel_vm::PreparedLibrary::new(Arc::new(registry)),
                lash_kernel_doc::Selection::Fresh,
            )
            .unwrap();
            let bounds = Bounds {
                memory_meter: lash_kernel_vm::MemoryMeter::On,
                charge: lash_kernel_vm::Charge::Exact,
                charge_limit: 1000,
                memory: 1 << 20,
                call_depth: 100,
                live_tasks: 10,
                requests_per_park: 10,
                join_members: 10,
            };
            let mut machine = KernelMachine::start(
                program,
                bounds,
                CellStart {
                    owner: PageId([9; 32]),
                    target: Target::Main,
                    args: Vec::new(),
                    session: None,
                    modes: Default::default(),
                    host: Vec::new(),
                },
                Box::new(Pages::default()),
            )
            .expect("started");
            let lash_kernel_vm::Step::Ended(end) = machine.run(&mut World, 1000).expect("run")
            else {
                panic!("the guard must end the run");
            };
            let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
                lash_core_store::checkpoint_graph::EncodeBudget::new(
                    lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
                ),
            );
            let record = lash_vm_broker::kernel::EndRecord::stage(&end, &mut staging)
                .expect("staged")
                .expect("recorded");
            let nodes = staging.into_nodes();
            let end = record
                .end(&mut lash_core_store::checkpoint_graph::GraphDecoder::new(
                    &nodes,
                    lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 24),
                ))
                .expect("read back");
            let End::Error(error) = end else {
                panic!("bound error");
            };
            let observation = CellObservation::of_run_error(error, &BTreeMap::new());
            let prompts = crate::CellDialect::typescript().prompts();
            let failure = observation.failure("", None, prompts.as_ref());
            let text = render(&failure, "cell");
            assert!(text.contains("work.spin"), "{text}");
            assert!(!text.contains(&function.to_string()), "{text}");
            assert!(!text.contains("form named above"), "{text}");
            assert!(text.contains(repair), "{text}");
        }
    }
}
