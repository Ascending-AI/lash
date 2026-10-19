//! Where one host process start spends its time, for its producer span.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_protocol_types::sync::MutexExt;
use lash_trace::{ProcessStartPhase, ProcessStartPhaseTime};

/// Contiguous phase clock of one process start: each mark charges the time
/// since the previous one to its phase, so the phases cover the start from
/// its producer's opening to its last mark with no gap. Monotonic and never
/// persisted.
#[derive(Debug)]
pub struct StartPhaseClock {
    clock: Arc<dyn crate::Clock>,
    marks: Mutex<(Instant, Vec<ProcessStartPhaseTime>)>,
}

impl StartPhaseClock {
    /// A clock that starts now.
    pub fn new(clock: Arc<dyn crate::Clock>) -> Self {
        let now = clock.now();
        Self {
            clock,
            marks: Mutex::new((now, Vec::new())),
        }
    }

    /// Charge the time since the previous mark to `phase`, which a start
    /// may reach more than once.
    pub fn mark(&self, phase: ProcessStartPhase) {
        let now = self.clock.now();
        let mut marks = self.marks.lock_recover();
        let elapsed_us =
            u64::try_from(now.saturating_duration_since(marks.0).as_micros()).unwrap_or(u64::MAX);
        marks.0 = now;
        match marks.1.iter_mut().find(|time| time.phase == phase) {
            Some(time) => time.elapsed_us = time.elapsed_us.saturating_add(elapsed_us),
            None => marks.1.push(ProcessStartPhaseTime { phase, elapsed_us }),
        }
    }

    /// The phases marked so far, in phase order.
    pub fn phases(&self) -> Vec<ProcessStartPhaseTime> {
        let mut phases = self.marks.lock_recover().1.clone();
        phases.sort_by_key(|time| time.phase);
        phases
    }
}

/// Marks `phase` on a start that is timed.
pub(crate) fn mark(phases: Option<&StartPhaseClock>, phase: ProcessStartPhase) {
    if let Some(phases) = phases {
        phases.mark(phase);
    }
}
