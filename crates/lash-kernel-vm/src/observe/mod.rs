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

pub use lash_kernel_doc::RunObservation as Observe;

/// Nonzero statement starts and empty-block entries, keyed by
/// [`lash_kernel_doc::Document::site_ordinals`]. Deltas add across frames
/// and committed run segments. Counts use u64 throughout.
pub type RanDelta = Vec<(u32, u64)>;

/// Document statement starts and empty-block entries in the order they
/// happened: each entry is one task's consecutive starts, by the block
/// sites they started at. Library code is never on it.
pub type StatementPath = Vec<(lash_kernel_doc::TaskIdentity, Vec<lash_kernel_doc::Site>)>;

/// What [`crate::KernelMachine::trace_path`] records until it is taken.
#[derive(Default)]
pub(crate) struct PathRecorder {
    last: Option<TaskId>,
    path: StatementPath,
    /// The heap fault that kept a task's identity off the path: the path
    /// is then not answered at all, never answered short.
    fault: Option<lash_kernel_doc::heap::HeapFault>,
}

impl PathRecorder {
    pub(crate) fn push(
        &mut self,
        task: TaskId,
        identity: impl FnOnce()
            -> Result<lash_kernel_doc::TaskIdentity, lash_kernel_doc::heap::HeapFault>,
        site: lash_kernel_doc::Site,
    ) {
        if self.fault.is_some() {
            return;
        }
        match self.path.last_mut() {
            Some((_, sites)) if self.last == Some(task) => sites.push(site),
            _ => match identity() {
                Ok(identity) => {
                    self.last = Some(task);
                    self.path.push((identity, vec![site]));
                }
                Err(fault) => self.fault = Some(fault),
            },
        }
    }

    pub(crate) fn take(&mut self) -> Result<StatementPath, lash_kernel_doc::heap::HeapFault> {
        self.last = None;
        let path = std::mem::take(&mut self.path);
        match self.fault.take() {
            Some(fault) => Err(fault),
            None => Ok(path),
        }
    }
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
    /// Its guard counted this much work.
    GuardWork(u64),
    /// It returned or raised, and was charged.
    NativeExit,
    /// A library function's formula is evaluated.
    FormulaEnter(LibId),
    FormulaExit,
}

/// What one machine observes.
pub(crate) struct Observer {
    pub(crate) path: Option<PathRecorder>,
    pub(crate) work: Option<Counting>,
    pub(crate) context: Option<Publisher>,
}

impl Observer {
    pub(crate) fn new() -> Self {
        Self {
            path: None,
            work: None,
            context: None,
        }
    }

    #[inline(never)]
    pub(crate) fn on(&mut self, event: Event, charged: u64, allocated: u64) {
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
                Event::GuardWork(units) => work.guard_work(units),
                Event::NativeExit => {
                    work.flush(charged, allocated);
                    work.native_exit();
                }
                Event::FormulaEnter(lib) => work.formula(lib.0),
                Event::FormulaExit => {}
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
                Event::NativeExit | Event::FormulaExit => context.leave(),
                Event::TaskEnded(_) | Event::NativeFast | Event::GuardWork(_) => {}
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
