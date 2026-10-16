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
use lash_kernel_doc::{Datum, Name, Site, SourceMap, TaskIdentity};
use lash_kernel_vm::{Bound, BoundExceeded, RunError};
use lash_vm_client::service::DialectRefusal;

use crate::dialect::DialectPrompts;

/// Offer typed dialect facts to the session's pure feedback contributors.
/// The original diagnosis remains intact; no contributor means no new text.
pub(crate) fn contribute_repairs(
    refusal: &mut DialectRefusal,
    source: &str,
    grant_calls: &BTreeMap<String, String>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Vec<lash_core::CellFailureRepair> {
    // An unavailable provider contributes no names. Diagnostic discovery does
    // not change whether this cell resolves, parks or receives a grant.
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
    let unknown_name = matches!(
        refusal.code.as_str(),
        "TS_UNKNOWN_BINDING" | "PY_UNKNOWN_NAME"
    )
    .then(|| refusal.span.and_then(|(start, end)| source.get(start..end)))
    .flatten();
    let diagnostic = lash_core::plugin::CellFailureDiagnostic {
        code: &refusal.code,
        span: refusal.span,
        unknown_name,
        grants: grant_calls,
        deferred: &deferred,
    };
    let feedback = ctx.cell_failure_repairs(&diagnostic);
    refusal.repairs.extend(
        feedback
            .iter()
            .flat_map(|reply| reply.repairs.iter().cloned()),
    );
    feedback
}

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

/// The failure of a cell whose run met a limit of its own at the worker
/// boundary. The CPU budget and the compute deadline are limits lash set on
/// the cell's worker: the same cell on the same inputs meets them again, so
/// the cell is refused with the bound it passed and its configured value,
/// as an execution bound is, and the model repairs from it (FIG-6523).
pub(crate) fn worker_limit_failure(limit: lash_vm_client::WorkerLimit) -> CellFailure {
    let (what, bound_ms, repair) = match limit {
        lash_vm_client::WorkerLimit::Cpu { bound_ms } => (
            "worker CPU time limit exceeded",
            bound_ms,
            "Reduce the computation or split it across smaller cells to fit the CPU time limit.",
        ),
        lash_vm_client::WorkerLimit::Deadline { bound_ms } => (
            "worker compute deadline exceeded",
            bound_ms,
            "Reduce the computation or split it across smaller cells to finish within the deadline.",
        ),
        other => {
            return CellFailure::new(CellFailureKind::Program, other.to_string())
                .with_worker_limit(other);
        }
    };
    CellFailure::new(
        CellFailureKind::Policy,
        format!("{CELL_BOUND_EXCEEDED}: {what} (limit {bound_ms} ms)\n{repair}"),
    )
    .with_worker_limit(limit)
}

/// What the kernel or the dialect made of a cell that did not complete, as
/// typed facts: the cell's failure is rendered from one of these and from
/// nothing else.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellObservation {
    /// The dialect refused the source.
    Refused(DialectRefusal),
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
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    /// No task is ready and none waits on anything the host can answer.
    Deadlock { waiting: Vec<TaskIdentity> },
    /// The cell passed an execution bound; a bound error is not catchable.
    Bound(BoundExceeded),
}

impl CellObservation {
    /// A dialect refusal remains the dialect's typed diagnosis. Source or
    /// diagnostic text cannot establish that a session binding was dropped.
    pub(crate) fn of_refusal(refusal: DialectRefusal) -> Self {
        Self::Refused(refusal)
    }

    /// How a run's error ended the cell. An uncaught unbound-variable error
    /// that names a binding the session lost to `K-SES-003` is that rule's
    /// refusal.
    pub(crate) fn of_run_error(error: RunError, not_carried: &BTreeMap<Name, String>) -> Self {
        match error {
            RunError::Uncaught(value) => {
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
    /// `map` is the document's source map, checked against `source`, which
    /// places a task's spawn there.
    pub(crate) fn failure(
        &self,
        source: &str,
        map: Option<&SourceMap>,
        channel: crate::plugin::RlmChannel,
        prompts: &dyn DialectPrompts,
    ) -> CellFailure {
        let vocabulary = prompts.prompt_vocabulary();
        let place = |tasks: &[TaskIdentity]| -> String {
            tasks
                .iter()
                .map(|task| task_place(task, source, map))
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
                        "\n\nA line starting with `{}` after whitespace terminates the outer {} even inside multiline source text; construct that content without a line-leading closing delimiter. Everything after that closing tag is discarded; wait for real results before continuing.",
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
                    Bound::Guard { .. } => (
                        "work limit exceeded".to_owned(),
                        None,
                    ),
                };
                let function = exceeded.function.as_ref()
                    .map(|name| format!(" in library function `{name}`"))
                    .unwrap_or_default();
                let repair = match exceeded.bound {
                    Bound::Charge => "Reduce the computation or split it across smaller cells to fit the instruction budget.",
                    Bound::Memory => "Keep fewer or smaller values live to fit the memory limit.",
                    Bound::Guard { .. } => "Reduce the function's input or split its work into smaller calls to fit its work limit.",
                    Bound::CallDepth => "Reduce nested calls or replace recursion with an iterative computation.",
                    Bound::LiveTasks => "Start fewer tasks at once and await them before starting more.",
                    Bound::RequestsPerPark => "Start fewer tool calls and sleeps at once; await some before starting more.",
                    Bound::JoinMembers => "Await fewer tasks together, in smaller groups.",
                };
                let mut failure = CellFailure::new(
                    CellFailureKind::Policy,
                    format!("{CELL_BOUND_EXCEEDED}: {what}{function} (limit {})\n{repair}", exceeded.limit),
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
/// when the document's source map places its spawn, else its typed label.
fn task_place(task: &TaskIdentity, source: &str, map: Option<&SourceMap>) -> String {
    let TaskIdentity::Spawned(spawn) = task else {
        return "the program itself".to_owned();
    };
    match map.and_then(|map| site_start(map, &spawn.site)) {
        Some(start) => {
            let (line, _) = line_column(source, start);
            let text = source.lines().nth(line - 1).unwrap_or_default().trim();
            format!("the async code at line {line} (`{text}`)")
        }
        None => format!("the task {}", task_label(task)),
    }
}

/// Where the source the map locates at `site`, or at the nearest located
/// node that encloses it, starts.
fn site_start(map: &SourceMap, site: &Site) -> Option<usize> {
    map.sites
        .iter()
        .filter(|located| {
            located.site.unit == site.unit && site.path.starts_with(&located.site.path)
        })
        .filter_map(|located| {
            let start = located
                .ranges
                .first()
                .map(|range| range.start)
                .or(located.implicit)?;
            Some((located.site.path.len(), start))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, start)| start)
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
        CellFailureKind::HelperReleaseNotHeld { release } => format!(
            "Next: ask the host to serve this {cell_noun} on a build that holds helper release {release}."
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash_kernel_doc::heap::{HeapFault, LogicalBytes, PageGrant, PageId, PageRef};
    use lash_kernel_doc::{
        ErrorDatum, FunctionRegistry, Handle, NativeCall, NativeError, NativeFunction, Timestamp,
        Value, parse_definition, parse_document,
    };
    use lash_kernel_state::paged::census_impl::Census;
    use lash_kernel_state::paged::{
        AccountingCertificate, AccountingMetric, AllocationCensus as _, PageAnswer, PageBody,
        PageSource,
    };
    use lash_kernel_vm::{Bounds, CellStart, End, Host, KernelMachine, Machine, Program, Target};

    use super::*;

    struct Guarded {
        memory: bool,
    }

    impl NativeFunction for Guarded {
        fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
            if self.memory {
                return Err(NativeError::Memory);
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
        fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
            pages
                .iter()
                .map(|page| {
                    self.0
                        .get(&page.id)
                        .cloned()
                        .ok_or(HeapFault::Missing { page: page.id })
                })
                .collect()
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
        fn clock(&mut self) -> Timestamp {
            panic!("no clock read")
        }
        fn random(&mut self) -> u64 {
            panic!("no random read")
        }
        fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
            panic!("no projection read")
        }
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
                .register(definition, Some(Arc::new(Guarded { memory })))
                .expect("registered");
            let document = parse_document(&format!(
            "kernel 1\nnumbers by_spelling\nuse work.spin = @{function}\nmain {{ return work.spin() }}",
        )).expect("document");
            let bounds = Bounds {
                charge: 1000,
                memory: 1 << 20,
                call_depth: 100,
                live_tasks: 10,
                requests_per_park: 10,
                join_members: 10,
            };
            let census = Census::restore(
                AccountingCertificate {
                    metric: AccountingMetric::LocalCostV1,
                    upper: LogicalBytes(0),
                    entities: 0,
                    refinements: 0,
                },
                LogicalBytes(bounds.memory),
            )
            .expect("census");
            let mut machine = KernelMachine::start(
                Program {
                    release: PageId([8; 32]),
                    document: Arc::new(document),
                    library: lash_kernel_vm::PreparedLibrary::new(Arc::new(registry)),
                },
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
                Box::new(census),
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
            let failure =
                observation.failure("", None, crate::plugin::RlmChannel::Cell, prompts.as_ref());
            let text = render(&failure, "cell");
            assert!(text.contains("work.spin"), "{text}");
            assert!(!text.contains(&function.to_string()), "{text}");
            assert!(!text.contains("form named above"), "{text}");
            assert!(text.contains(repair), "{text}");
        }
    }
}
