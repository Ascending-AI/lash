//! The determinism harness (FIG-3672): the checks every slice that makes the
//! drive deterministic proves its change with.
//!
//! A drive is deterministic when, over the same recorded history, every run
//! issues the same commands in the same order with the same bytes and commits
//! the same bytes — whichever worker runs it and however its operations'
//! completions are scheduled. The harness checks exactly that:
//!
//! - [`LocalTestCx`] is an engine-free test context. It records each
//!   operation a drive issues, journals the outcome on a fresh run, serves it
//!   on a replay after comparing the reissued command bytes, and records commit
//!   bytes. It is also a [`RuntimeEffectController`](crate::RuntimeEffectController),
//!   so drive code that issues effects through a scoped controller runs over it
//!   unchanged ([`LocalTestCx::controller`]).
//! - [`LocalTestCx::run`] is a single-thread, non-Tokio executor that fails a
//!   run on any wake of the drive no operation caused, and on a drive that is
//!   pending with no operation in flight.
//! - [`Schedule`] perturbs, under a seed, when settled operations are delivered
//!   and in which order.
//! - [`DriveTranscript`] is the command stream plus commit bytes, and
//!   [`DriveTranscript::compare`] reports the first entry two runs disagree on.
//! - [`DeterminismEngine`] is the seam an engine leg plugs into, and
//!   [`DeterminismCheck`] runs a fresh run then a cold replay, a replay on a
//!   separate worker, and perturbed replays against it. [`LocalEngine`] is the
//!   engine-free leg.
//!
//! A test builds a [`LocalEngine`] over its drive and asserts
//! `DeterminismCheck::new(seed).run(&engine)` passes; a red test asserts the
//! [`DeterminismFailure`] it expects.

mod check;
mod controller;
mod cx;
mod schedule;
mod transcript;

pub use check::{
    DeterminismCheck, DeterminismEngine, DeterminismFailure, DeterminismReport, EngineRun,
    FailureCause, LocalDrive, LocalEngine, ReplayMode, RunMode, WorkerState,
};
pub use cx::{
    CxMode, DEFAULT_BODY_TIMEOUT, DriveJournal, JournalEntry, LocalTestCx, Op, ReplayDivergence,
    RunFailure, RunRecord,
};
pub use schedule::{Schedule, SeededRng};
pub use transcript::{DriveTranscript, TranscriptDivergence, TranscriptEntry};

#[cfg(test)]
mod tests;
