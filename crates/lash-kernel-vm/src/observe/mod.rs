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

pub use context::{GuestContext, GuestFrame, GuestPhase, NO_STATEMENT};
pub use symbols::{GuestSymbols, WorkReport};
// What only the diagnostic seam names (`crate::perf_witness`).
#[cfg(any(test, feature = "perf-witness"))]
pub use context::{GuestSample, ReadRefused};
#[cfg(any(test, feature = "perf-witness"))]
pub use profile::{Completeness, GuestProfile, READS_PER_TICK, SymbolTime};
#[cfg(any(test, feature = "perf-witness"))]
pub use symbols::{CodeKind, CodeSymbol, CodeWork, LibKind, LibSymbol, NativeWork};
#[cfg(any(test, feature = "perf-witness"))]
pub use work::{CodeCounts, NativeCounts};

pub(crate) use context::Publisher;
pub(crate) use work::Counting;

use lash_kernel_doc::TaskId;
use lash_kernel_exec::Weight;

use crate::compile::{CodeId, LibId, StmtId};

pub use lash_kernel_doc::RunObservation as ObservationMode;

pub use lash_kernel_doc::SiteCounts;

/// Document statement starts and empty-block entries in the order they
/// happened: each entry is one task's consecutive starts, by the block
/// sites they started at. Library code is never on it.
pub(crate) type StatementPath = Vec<(
    lash_kernel_doc::kernel_internals::TaskIdentity,
    Vec<lash_kernel_doc::Site>,
)>;

/// What [`crate::KernelMachine::trace_path`] records until it is taken.
#[derive(Default)]
pub(crate) struct PathRecorder {
    last: Option<TaskId>,
    ordinals: std::collections::BTreeMap<lash_kernel_doc::Site, u32>,
    path: StatementPath,
    /// The heap fault that kept a task's identity off the path: the path
    /// is then not answered at all, never answered short.
    fault: Option<lash_kernel_doc::heap::HeapFault>,
}

impl PathRecorder {
    pub(crate) fn new(ordinals: std::collections::BTreeMap<lash_kernel_doc::Site, u32>) -> Self {
        Self {
            ordinals,
            ..Default::default()
        }
    }
    pub(crate) fn push(
        &mut self,
        task: TaskId,
        identity: impl FnOnce() -> Result<
            lash_kernel_doc::kernel_internals::TaskIdentity,
            lash_kernel_doc::heap::HeapFault,
        >,
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

    pub(crate) fn take(
        &mut self,
    ) -> Result<lash_kernel_doc::ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.last = None;
        let path = std::mem::take(&mut self.path);
        match self.fault.take() {
            Some(fault) => Err(fault),
            None => {
                let rows = path
                    .into_iter()
                    .map(|(task, sites)| {
                        let starts = sites
                            .into_iter()
                            .map(|site| {
                                self.ordinals.get(&site).copied().ok_or_else(|| {
                                    lash_kernel_doc::heap::HeapFault::Malformed(format!(
                                        "the path names no document site {site}"
                                    ))
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok((task, starts))
                    })
                    .collect::<Result<Vec<_>, lash_kernel_doc::heap::HeapFault>>()?;
                Ok(lash_kernel_doc::kernel_internals::execution_path(rows))
            }
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

/// Every charge a run makes, in order, as the per-unit reference charger
/// reads them (`K-CHG-002`, [`lash_kernel_exec::ReferenceCharger`]): each
/// unit run as [`Weight::Units`], each other charge as [`Weight::Atomic`].
/// A run that passes its bound inside a unit run records the units within
/// it, then the unit that passes. Clones share one record, so a law keeps
/// it across a seal and a reopen and reads it while the run is parked.
#[derive(Clone, Debug, Default)]
pub struct ChargeTape(Arc<std::sync::Mutex<Recorded>>);

/// What a [`ChargeTape`] records: the charges, and the work the regions
/// tier committed while it recorded (`docs/design/kernel-regions.md` §1).
#[derive(Debug, Default)]
struct Recorded {
    steps: Vec<Weight>,
    work: RegionWork,
}

/// The work the regions tier committed in a recorded run: what tells a law
/// comparing a run with regions on against the reference that regions ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionWork {
    /// Loop iterations a region ran to their end.
    pub iterations: u64,
    /// Calls an island entered: their frames, which the reference would
    /// push, it ran virtually.
    pub calls: u64,
    /// Loop passes a helper plan's region ran to their end
    /// (`machine::plan_regions`).
    pub passes: u64,
}

impl ChargeTape {
    pub fn new() -> Self {
        Self::default()
    }

    /// The charges recorded so far.
    pub fn steps(&self) -> Vec<Weight> {
        self.lock().steps.clone()
    }

    /// How many charges are recorded.
    pub fn len(&self) -> usize {
        self.lock().steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().steps.is_empty()
    }

    pub(crate) fn push(&self, weight: Weight) {
        self.lock().steps.push(weight);
    }

    /// The work regions committed so far.
    pub(crate) fn work(&self) -> RegionWork {
        self.lock().work
    }

    /// Counts `iterations` a loop region ran to their end.
    pub(crate) fn iterated(&self, iterations: u64) {
        self.lock().work.iterations += iterations;
    }

    /// Counts a pass a helper plan's loop region ran to its end.
    pub(crate) fn passed(&self) {
        self.lock().work.passes += 1;
    }

    /// Counts a call an island entered.
    pub(crate) fn entered(&self) {
        self.lock().work.calls += 1;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What one machine observes.
pub(crate) struct Observer {
    #[cfg(any(test, feature = "perf-witness"))]
    pub(crate) work_witness: bool,
    pub(crate) path: Option<PathRecorder>,
    pub(crate) work: Option<Counting>,
    pub(crate) context: Option<Publisher>,
    pub(crate) charges: Option<ChargeTape>,
}

impl Observer {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(any(test, feature = "perf-witness"))]
            work_witness: false,
            path: None,
            work: None,
            context: None,
            charges: None,
        }
    }

    /// Whether a charge tape is all that observes: the one observer a
    /// region records for (`docs/design/kernel-regions.md` §1).
    pub(crate) fn only_charges(&self) -> bool {
        self.path.is_none() && self.work.is_none() && self.context.is_none()
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
