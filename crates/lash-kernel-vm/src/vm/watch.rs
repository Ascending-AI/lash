//! What observes a core run (`observe/`): site counts, the statement path
//! and the charge tape.
//!
//! An observed run takes the exact path in every block, so each op's
//! points and charges are seen as it runs. The exact and prepaid paths
//! bill alike (contract §3): observation changes no result, charge, park
//! or slice return.

use crate::observe::Weight;
use lash_kernel_doc::{Document, TaskId, Value};
use lash_kernel_value::sites::{Site, TaskIdentity};

use super::Run;
use super::Vm;
use super::charge::Rest;
use crate::bytecode::program::{Event, Point, Program, Unit, UnitId};
use crate::heap::Heap;
use crate::observe::{ChargeTape, PathRecorder};
use crate::{Host, SiteCounts};

/// One machine's observers.
pub(crate) struct Watch {
    /// Each unit's points' count ordinals, parallel to `Unit::points`: a
    /// branch's `then` and `otherwise` ordinals, the first alone for the
    /// others. `None` where the document names no such site.
    ordinals: Vec<Vec<[Option<u32>; 2]>>,
    /// Statement starts, block entries, action issues and tests since the last take, by ordinal;
    /// empty when counts are not observed.
    counts: Vec<u64>,
    path: Option<PathRecorder>,
    pub(crate) tape: Option<ChargeTape>,
}

impl Watch {
    /// Observes what `counts` and `path` ask for, over `program`'s points,
    /// and records charges on `tape`. `None` when nothing is observed.
    pub(crate) fn new(
        document: &Document,
        program: &Program,
        counts: bool,
        path: bool,
        tape: Option<ChargeTape>,
    ) -> Option<Self> {
        if !counts && !path {
            return tape.map(|tape| Self {
                ordinals: Vec::new(),
                counts: Vec::new(),
                path: None,
                tape: Some(tape),
            });
        }
        let ordinals = document.ordinals();
        let size = ordinals.sites.len() + ordinals.actions.len() + ordinals.conditions.len();
        let at = |map: &std::collections::BTreeMap<Site, u32>, site: &Option<Site>| {
            site.as_ref().and_then(|site| map.get(site).copied())
        };
        let units = program
            .units
            .iter()
            .map(|unit| {
                unit.points
                    .iter()
                    .map(|(_, point)| match point {
                        Point::Start(site) => [ordinals.sites.get(site).copied(), None],
                        Point::Branch { then, otherwise } => {
                            [at(&ordinals.sites, then), at(&ordinals.sites, otherwise)]
                        }
                        Point::Test(site) => [ordinals.conditions.get(site).copied(), None],
                        Point::Action(site) => [ordinals.actions.get(site).copied(), None],
                    })
                    .collect()
            })
            .collect();
        Some(Self {
            ordinals: units,
            counts: if counts { vec![0; size] } else { Vec::new() },
            path: path.then(|| PathRecorder::new(ordinals.sites)),
            tape,
        })
    }

    pub(crate) fn counting(&self) -> bool {
        !self.counts.is_empty()
    }

    /// Takes and resets the counts since the previous take.
    pub(crate) fn take_counts(&mut self) -> SiteCounts {
        self.counts
            .iter_mut()
            .enumerate()
            .filter_map(|(ordinal, count)| {
                let count = std::mem::take(count);
                (count != 0).then_some((ordinal as u32, count))
            })
            .collect()
    }

    /// Takes and resets the path since the previous take.
    pub(crate) fn take_path(
        &mut self,
    ) -> Result<crate::ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.path
            .as_mut()
            .map_or_else(|| Ok(crate::ExecutionPath::default()), PathRecorder::take)
    }

    pub(crate) fn charge(&self, weight: Weight) {
        #[cfg(test)]
        super::charge_work();
        if let Some(tape) = &self.tape
            && !matches!(weight, Weight::Units(0) | Weight::Atomic(0))
        {
            tape.push(weight);
        }
    }

    /// What unit `unit`'s points at op `pc` see, `stack` as the op finds
    /// it, in task `task` of identity `identity`.
    pub(crate) fn see(
        &mut self,
        unit: usize,
        points: &[(u32, Point)],
        pc: usize,
        stack: &[Value],
        task: usize,
        identity: &TaskIdentity,
    ) {
        let Self {
            ordinals,
            counts,
            path,
            ..
        } = self;
        let Some(ordinals) = ordinals.get(unit) else {
            return;
        };
        let mut count = |ordinal: Option<u32>| {
            if let Some(count) = ordinal.and_then(|at| counts.get_mut(at as usize)) {
                *count += 1;
            }
        };
        let mut trace = |site: &Site| {
            if let Some(path) = path {
                path.push(TaskId(task as u64), || Ok(identity.clone()), site.clone());
            }
        };
        let from = points.partition_point(|(at, _)| (*at as usize) < pc);
        for (k, (at, point)) in points.iter().enumerate().skip(from) {
            if *at as usize != pc {
                break;
            }
            let [first, second] = ordinals[k];
            match point {
                // A statement start is on the path and counted: statement
                // starts, block entries, action issues and tests share the
                // run-count channel, each in its own ordinal domain.
                Point::Start(site) => {
                    count(first);
                    trace(site);
                }
                Point::Branch { then, otherwise } => {
                    // A condition that is not a bool raises: nothing is entered.
                    let (site, ordinal) = match stack.last() {
                        Some(Value::Bool(true)) => (then, first),
                        Some(Value::Bool(false)) => (otherwise, second),
                        _ => continue,
                    };
                    if let Some(site) = site {
                        count(ordinal);
                        trace(site);
                    }
                }
                Point::Test(_) | Point::Action(_) => count(first),
            }
        }
    }
}

impl<H: Heap> Vm<H> {
    /// [`Self::bill`] under `Charge::Off`, where the event only places the
    /// slice, or onto the charge tape.
    #[cold]
    #[inline(never)]
    pub(super) fn bill_aside(&mut self, event: Event) -> Run<()> {
        if self.off {
            self.meter.place(event.units.saturating_add(event.atomic));
            return Ok(());
        }
        self.bill_watched(event)
    }

    /// [`Self::bill`] onto the charge tape: each unit run whole, or the
    /// units that fit and then the one that passes, and each atomic event
    /// whole before the bound is checked.
    #[cold]
    #[inline(never)]
    fn bill_watched(&mut self, event: Event) -> Run<()> {
        #[cfg(test)]
        super::charge_work();
        let tape = |vm: &Self, weight| {
            if let Some(watch) = &vm.watch {
                watch.charge(weight);
            }
        };
        if event.units != 0 {
            match self.meter.units(event.units) {
                Ok(()) => tape(self, Weight::Units(event.units)),
                Err(refused) => {
                    if let Rest::Units(rest) = refused.rest {
                        tape(self, Weight::Units(event.units - rest + 1));
                    }
                    self.refusal(refused, false)?;
                }
            }
        }
        if event.atomic != 0 {
            tape(self, Weight::Atomic(event.atomic));
            if let Err(refused) = self.meter.atomic(event.atomic) {
                self.refusal(refused, false)?;
            }
        }
        Ok(())
    }

    /// What op `pc` of unit `id`'s points see, before it runs.
    #[cold]
    #[inline(never)]
    pub(super) fn see(&mut self, id: UnitId, unit: &Unit, pc: usize) {
        let running = self.tasks.running;
        if let Some(watch) = self.watch.as_deref_mut() {
            watch.see(
                id as usize,
                &unit.points,
                pc,
                &self.cx.stack,
                running,
                &self.tasks.all[running].identity,
            );
        }
    }

    /// Hands the counts so far to a host that reads them live, before it
    /// is asked something.
    pub(crate) fn report(&mut self, host: &mut dyn Host) {
        if let Some(watch) = self.watch.as_deref_mut()
            && watch.counting()
            && let Some(observer) = host.observer()
        {
            observer.counts(&watch.take_counts());
        }
    }
}
