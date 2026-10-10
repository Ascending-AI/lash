//! One corpus of kernel text and observations, independent of any dialect.
//!
//! [`DocumentRunner`] is the reader seam. [`MachineRunner`] adapts any kernel
//! machine, scripting deliveries and synchronous host answers. [`check_native`]
//! compares cold and warm runs for every registered native implementation.
//! [`check_migration`] is the conformance case a kernel version's migration
//! ships with: a run parked under the old version at each of its parks,
//! carried across and resumed under the new one, against a run that never
//! left the old.

mod case;
mod coverage;
mod machine;
mod migration;
mod native;
mod native_calls;
pub mod smith;

pub use case::{
    Case, CaseBounds, Delivery, Environment, Expected, ExpectedEnd, HostAnswer, Observations,
    ReadAnswer, ScriptOutcome, Shard, Trace,
};
pub use coverage::{CoverageError, PendingRule, check_coverage, load_corpus, rule_ids};
pub use machine::MachineRunner;
pub use migration::{MigrationCheck, check_migration};
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

#[cfg(all(test, feature = "synthetic-next"))]
mod migration_laws;
