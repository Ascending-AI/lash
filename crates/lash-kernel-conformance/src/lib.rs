//! One corpus of kernel text and observations, independent of any dialect.
//!
//! [`DocumentRunner`] is the reader seam. [`MachineRunner`] adapts any kernel
//! machine, scripting deliveries and synchronous host answers. [`check_native`]
//! compares cold and warm runs for every registered native implementation.

mod case;
mod coverage;
mod machine;
mod native;
mod native_calls;

pub use case::{
    Case, CaseBounds, Delivery, Environment, Expected, ExpectedEnd, HostAnswer, Observations,
    ReadAnswer, ScriptOutcome, Shard, Trace,
};
pub use coverage::{CoverageError, PendingRule, check_coverage, load_corpus, rule_ids};
pub use machine::MachineRunner;
pub use native::check_native;
pub use native_calls::{
    NativeCase, NativeObservation, NativeOutcome, NativeShard, check_native_calls,
};

/// A reader, interpreter or front end capable of running a kernel document.
/// It returns observations, never expectations copied from the case.
pub trait DocumentRunner {
    fn observe(&mut self, case: &Case) -> Result<Observations, HarnessError>;
}

/// A failed script, refusal, or difference from the written rule.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HarnessError(pub String);

/// Runs one case and checks every observation it pins.
pub fn check_case(
    runner: &mut impl DocumentRunner,
    case: &Case,
) -> Result<Observations, HarnessError> {
    let actual = runner.observe(case)?;
    case.expected
        .check(&actual)
        .map_err(|error| HarnessError(format!("{}: {error}", case.name)))?;
    Ok(actual)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod native_laws;
