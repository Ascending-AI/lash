//! Observing a run: exact counters of the work each guest function does,
//! and a published guest context a sampler outside the machine reads.
//!
//! Both are opt-in, per machine, and derived: nothing they hold is part of
//! a run's state, and neither changes a result, a charge, a park or an
//! export. A machine that observes nothing pays one branch at each
//! observation point.

mod context;
mod profile;
mod symbols;
mod work;

use std::sync::Arc;

pub use context::{GuestContext, GuestFrame, GuestPhase, GuestSample, NO_STATEMENT, ReadRefused};
pub use profile::{Completeness, GuestProfile, READS_PER_TICK, SymbolTime};
pub use symbols::{
    CodeKind, CodeSymbol, CodeWork, GuestSymbols, LibKind, LibSymbol, NativeWork, WorkReport,
};
pub use work::{CodeCounts, NativeCounts};

pub(crate) use context::Publisher;
pub(crate) use work::Counting;

use lash_kernel_doc::TaskId;

use crate::compile::{CodeId, LibId, StmtId};

/// Spike (FIG-6364): a document statement by its coordinate: the site of
/// its block and its index there. Layout-independent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StatementAt {
    pub block: lash_kernel_doc::Site,
    pub index: u32,
}

/// Something a run did that an observer sees.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    /// A task's innermost frame begins a statement.
    Statement {
        task: TaskId,
        code: CodeId,
        stmt: StmtId,
        charging: bool,
    },
    /// A frame was pushed.
    Push {
        task: TaskId,
        code: CodeId,
    },
    /// A frame was left, once its call was charged.
    Pop {
        task: TaskId,
    },
    TaskEnded(TaskId),
    /// A library function with no frame is called, by `task`.
    NativeEnter {
        task: TaskId,
        lib: LibId,
    },
    /// Its fast path answered.
    NativeFast,
    /// It is called again after a collection.
    NativeRetry,
    /// Its guard counted this much work.
    GuardWork(u64),
    /// It returned or raised, and was charged.
    NativeExit,
    /// A library function's formula is evaluated.
    FormulaEnter(LibId),
    FormulaExit,
    CollectEnter,
    CollectExit,
}

/// What one machine observes.
pub(crate) struct Observer {
    pub(crate) work: Option<Counting>,
    pub(crate) context: Option<Publisher>,
    /// Spike (FIG-6364): every statement start, counted by statement id.
    pub(crate) counts: Option<Vec<u64>>,
    /// Spike (FIG-6364): every statement start, in order, with its task.
    pub(crate) trace: Option<Vec<(u32, u32)>>,
}

impl Observer {
    pub(crate) fn new() -> Self {
        Self {
            work: None,
            context: None,
            counts: None,
            trace: None,
        }
    }

    #[inline(never)]
    pub(crate) fn on(&mut self, event: Event, charged: u64, allocated: u64) {
        if let Event::Statement { task, stmt, .. } = event {
            if let Some(counts) = &mut self.counts {
                let at = stmt.0 as usize;
                if counts.len() <= at {
                    counts.resize(at + 1, 0);
                }
                counts[at] += 1;
            }
            if let Some(trace) = &mut self.trace {
                trace.push((task.0 as u32, stmt.0));
            }
        }
        if let Some(work) = &mut self.work {
            match event {
                Event::Statement { code, charging, .. } => work.statement(code.0, charging),
                Event::Push { task, code } => {
                    work.flush(charged, allocated);
                    work.push(task.0 as usize, code.0);
                }
                Event::Pop { task } => {
                    work.flush(charged, allocated);
                    work.pop(task.0 as usize);
                }
                Event::TaskEnded(task) => {
                    work.flush(charged, allocated);
                    work.task_ended(task.0 as usize);
                }
                Event::NativeEnter { task, lib } => {
                    work.flush(charged, allocated);
                    work.native_enter(task.0 as usize, lib.0);
                }
                Event::NativeFast => work.native_fast(),
                Event::NativeRetry => work.native_retry(),
                Event::GuardWork(units) => work.guard_work(units),
                Event::NativeExit => {
                    work.flush(charged, allocated);
                    work.native_exit();
                }
                Event::FormulaEnter(lib) => work.formula(lib.0),
                Event::FormulaExit | Event::CollectEnter | Event::CollectExit => {}
            }
        }
        if let Some(context) = &mut self.context {
            match event {
                Event::Statement {
                    task, code, stmt, ..
                } => context.statement(task.0, code.0, stmt.0),
                Event::Push { task, code } => context.push(task.0, code.0),
                Event::Pop { task } => context.pop(task.0),
                Event::NativeEnter { lib, .. } => context.enter(GuestPhase::Native(lib.0)),
                Event::FormulaEnter(lib) => context.enter(GuestPhase::Formula(lib.0)),
                Event::CollectEnter => context.enter(GuestPhase::Collect),
                Event::NativeExit | Event::FormulaExit | Event::CollectExit => context.leave(),
                Event::TaskEnded(_)
                | Event::NativeFast
                | Event::NativeRetry
                | Event::GuardWork(_) => {}
            }
        }
    }

    /// An expression or an action operand is evaluated: counted, and
    /// nothing a context shows.
    #[inline(never)]
    pub(crate) fn expression(&mut self, charging: bool) {
        if let Some(work) = &mut self.work {
            work.expression(charging);
        }
    }

    pub(crate) fn publish(&mut self, context: Arc<GuestContext>) {
        self.context = Some(Publisher::new(context));
    }
}
