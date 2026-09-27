//! The monotonic stopwatch the VM's optional profiler reads.
//!
//! Profiling is observational only: a mark measures how long an instruction
//! ran for the host's `observe_profile` report and feeds no runtime decision,
//! so the wall read lives here — outside the scanned drive paths — behind a
//! name that says what it is for (FIG-3672).

/// A monotonic instant taken for profiling; [`ProfileMark::elapsed_nanos`]
/// reports the span since it was taken.
#[derive(Clone, Copy, Debug)]
pub struct ProfileMark(std::time::Instant);

impl ProfileMark {
    /// Take a mark on the monotonic clock.
    pub fn now() -> Self {
        Self(std::time::Instant::now())
    }

    /// Nanoseconds since the mark.
    pub fn elapsed_nanos(&self) -> u128 {
        self.0.elapsed().as_nanos()
    }
}
