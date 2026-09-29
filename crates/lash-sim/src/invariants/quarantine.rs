//! Known runtime defects the global invariants find, each by name.
//!
//! A violation an entry covers still prints with its seed, invariant and
//! excerpt; it does not fail the run. An entry names the ticket that fixes
//! the defect, and the fix deletes it.

use super::Violation;

/// One quarantined defect.
#[derive(Debug)]
pub struct Quarantine {
    /// The entry's name, printed with every violation it covers.
    pub name: &'static str,
    /// The invariant it covers.
    pub invariant: &'static str,
    /// Scenarios it covers, by prefix; empty covers every scenario.
    pub scenario_prefix: &'static str,
    /// Text the violation's detail must contain.
    pub detail_contains: &'static str,
    /// What the defect is, where it was seen, and what fixes it.
    pub reason: &'static str,
}

/// Every open quarantine entry.
pub static QUARANTINE: &[Quarantine] = &[];

/// The entry that covers `violation` in `scenario`, when one does.
#[must_use]
pub fn covering(scenario: &str, violation: &Violation) -> Option<&'static Quarantine> {
    QUARANTINE.iter().find(|entry| {
        entry.invariant == violation.invariant
            && scenario.starts_with(entry.scenario_prefix)
            && violation.detail.contains(entry.detail_contains)
    })
}
