//! `await_shift` fixtures for the follower's tail behaviours.
//!
//! A `SendHandle` follower waits on three wakes: live replay, the engine's
//! shift attach (`SessionWorkEngine::await_shift`) and the store poll that
//! backs off 25 ms → 1 s. The `poll` and `grace` cases isolate the last two
//! against a live server by substituting only the host's `await_shift`:
//!
//! * [`AwaitShiftMode::Answered`] answers the attach immediately, so the
//!   follower treats the shift as already stopped and settlement detection
//!   rides the poll cadence alone — the measured `settle→complete` tail is
//!   the realized 25 ms..1 s backoff.
//! * [`AwaitShiftMode::Pending`] never answers the attach, exactly as a
//!   shift that outlives the measured run does. The follower's 5 s
//!   live-report grace binds only while a run in the host may still deposit
//!   a report, so a run the worker ran answers with the durable thin report
//!   once the store shows it settled.
//!
//! Both still deliver the shift: `schedule_shift`/`request_shift` forward
//! untouched, only the host-side wait is stubbed, and the real shift runs to
//! completion on the worker either way.

/// How a host's `await_shift` answers in a latency case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AwaitShiftMode {
    /// The engine's own attach — production behaviour.
    Real,
    /// Answer immediately as if the shift already stopped; polls alone
    /// carry settlement detection.
    Answered,
    /// Never answer, as a shift that outlives the run does.
    Pending,
}
