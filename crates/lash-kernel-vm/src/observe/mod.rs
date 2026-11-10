//! Observing a run: exact counters of the work each guest function does,
//! and a published guest context a sampler outside the machine reads.
//!
//! Both are opt-in, per machine, and derived: nothing they hold is part of
//! a run's state, and neither changes a result, a charge, a park or an
//! export. A machine that observes nothing pays one branch at each
//! observation point.

mod charge;
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

pub use charge::{Ending, ReferenceCharger, Weight};
use lash_kernel_doc::TaskId;

pub use lash_kernel_value::observation::RunObservation as ObservationMode;

/// Document statement starts and empty-block entries in the order they
/// happened: each entry is one task's consecutive starts, by the block
/// sites they started at. Library code is never on it.
pub(crate) type StatementPath = Vec<(
    lash_kernel_value::sites::TaskIdentity,
    Vec<lash_kernel_value::sites::Site>,
)>;

/// What [`crate::KernelMachine::trace_path`] records until it is taken.
#[derive(Default)]
pub(crate) struct PathRecorder {
    last: Option<TaskId>,
    ordinals: std::collections::BTreeMap<lash_kernel_value::sites::Site, u32>,
    path: StatementPath,
    /// The heap fault that kept a task's identity off the path: the path
    /// is then not answered at all, never answered short.
    fault: Option<lash_kernel_doc::heap::HeapFault>,
}

impl PathRecorder {
    pub(crate) fn new(
        ordinals: std::collections::BTreeMap<lash_kernel_value::sites::Site, u32>,
    ) -> Self {
        Self {
            ordinals,
            ..Default::default()
        }
    }
    pub(crate) fn push(
        &mut self,
        task: TaskId,
        identity: impl FnOnce() -> Result<
            lash_kernel_value::sites::TaskIdentity,
            lash_kernel_doc::heap::HeapFault,
        >,
        site: lash_kernel_value::sites::Site,
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
    ) -> Result<lash_kernel_value::observation::ExecutionPath, lash_kernel_doc::heap::HeapFault>
    {
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
                Ok(lash_kernel_value::kernel_internals::execution_path(rows))
            }
        }
    }
}

/// Every charge a run makes, in order, as the per-unit reference charger
/// reads them (`K-CHG-002`, [`ReferenceCharger`]): each
/// unit run as [`Weight::Units`], each other charge as [`Weight::Atomic`].
/// A run that passes its bound inside a unit run records the units within
/// it, then the unit that passes. Clones share one record, so a law keeps
/// it across a seal and a reopen and reads it while the run is parked.
#[derive(Clone, Debug, Default)]
pub struct ChargeTape(Arc<std::sync::Mutex<Recorded>>);

/// The ordered charges a [`ChargeTape`] records.
#[derive(Debug, Default)]
struct Recorded {
    steps: Vec<Weight>,
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
        // A one-unit atomic charge and one unit have the same event boundary
        // (Weight::expand). Author that event once where facts enter the tape,
        // so source forms and metered helper forms retain identical records.
        let weight = match weight {
            Weight::Atomic(1) => Weight::Units(1),
            weight => weight,
        };
        self.lock().steps.push(weight);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
