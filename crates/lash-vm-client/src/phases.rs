//! Bounded, host-clock observations. No guest payload or checkpoint is retained.
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct EffectPhase {
    pub phase: &'static str,
    pub start_ns: u64,
    pub end_ns: u64,
    pub pid: u32,
    pub process_epoch: Option<String>,
    pub lease: u64,
    pub owner_epoch: u64,
    pub frame_epoch: u64,
    pub owner: Option<String>,
    pub document: Option<String>,
    pub effect: Option<u64>,
    pub direction: Option<&'static str>,
    pub kind: Option<&'static str>,
    pub bytes: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EffectPhases {
    pub clock: &'static str,
    pub capacity: usize,
    pub dropped: u64,
    pub events: Vec<EffectPhase>,
}

#[derive(Clone)]
pub(crate) struct Recorder {
    origin: Instant,
    state: Arc<Mutex<EffectPhases>>,
}
impl Recorder {
    pub fn new(capacity: usize) -> Self {
        Self {
            origin: Instant::now(),
            state: Arc::new(Mutex::new(EffectPhases {
                clock: "host Instant nanoseconds since observer creation; worker progress is receipt time",
                capacity,
                dropped: 0,
                events: Vec::new(),
            })),
        }
    }
    pub fn now(&self) -> u64 {
        self.origin.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
    }
    pub fn snapshot(&self) -> EffectPhases {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn push(&self, event: EffectPhase) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.events.len() < state.capacity {
            state.events.push(event);
        } else {
            state.dropped += 1;
        }
    }
}

#[derive(Clone)]
pub(crate) struct Scope {
    pub recorder: Recorder,
    pub pid: u32,
    pub process_epoch: Option<String>,
    pub lease: u64,
    pub owner_epoch: u64,
    pub frame_epoch: u64,
    pub owner: Option<String>,
    pub document: Option<String>,
    pub effect: Option<u64>,
}
impl Scope {
    pub fn record(&self, phase: &'static str, start: u64) {
        self.frame(phase, start, None, None, None);
    }
    pub fn frame(
        &self,
        phase: &'static str,
        start: u64,
        direction: Option<&'static str>,
        kind: Option<&'static str>,
        bytes: Option<usize>,
    ) {
        self.recorder.push(EffectPhase {
            phase,
            start_ns: start,
            end_ns: self.recorder.now(),
            pid: self.pid,
            process_epoch: self.process_epoch.clone(),
            lease: self.lease,
            owner_epoch: self.owner_epoch,
            frame_epoch: self.frame_epoch,
            owner: self.owner.clone(),
            document: self.document.clone(),
            effect: self.effect,
            direction,
            kind,
            bytes,
        });
    }
    pub fn point(&self, phase: &'static str) {
        self.record(phase, self.recorder.now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EFFECT-PHASE-LOSS: bounded observations never evict old endpoints or
    /// conceal a lost suffix as a complete population.
    #[test]
    fn overflow_retains_the_prefix_and_reports_the_lost_suffix() {
        let recorder = Recorder::new(1);
        let scope = Scope {
            recorder: recorder.clone(),
            pid: 7,
            process_epoch: Some("123".to_owned()),
            lease: 1,
            owner_epoch: 2,
            frame_epoch: 3,
            owner: None,
            document: None,
            effect: Some(0),
        };
        scope.point("answer");
        scope.point("resume_observed");
        let report = recorder.snapshot();
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.events[0].phase, "answer");
        assert_eq!(report.events[0].effect, Some(0));
        assert_eq!(report.dropped, 1);
    }
}
