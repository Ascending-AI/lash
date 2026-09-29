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
pub static QUARANTINE: &[Quarantine] = &[
    Quarantine {
        name: "withdrawn-input-keeps-its-ingress-obligation",
        invariant: "obligations-settled-or-stalled",
        scenario_prefix: "",
        detail_contains: "state cancelled) left ",
        reason: "A host cancel of an open turn input (lash-store-sql `pending_inputs.cancel`) \
                 sets the row cancelled and leaves its ADR 0109 ingress obligation as it was; \
                 only an admission delivers it. Nothing admits a cancelled row, so the obligation \
                 stays claimed, and once the claim lapses the relay asks the engine for a drive \
                 with nothing to admit, attempt after attempt, until the ceiling stalls it as \
                 attempts_exhausted. Seen on generated seeds whose queued input is cancelled \
                 (fast-random seed 0x5, ...). Fixed when withdrawing an open row settles its \
                 obligation in the same write.",
    },
    Quarantine {
        name: "frame-environment-cleanup-stays-claimed-after-its-claimant-dies",
        invariant: "obligations-settled-or-stalled",
        scenario_prefix: "",
        detail_contains: "on artifact_cleanup_obligations frame_environment/",
        reason: "An ADR 0113 artifact-cleanup obligation for a frame_environment referrer is \
                 left claimed, with no stall reason and no last error, after a session delete \
                 with an orphaned root across deployment deaths (chaos soak S3, seed 0x6005, \
                 main full run 36571087705). FIG-4129 fixes it so the lapsed claim is \
                 reclaimed and settled, or stalls typed, and deletes this entry.",
    },
];

/// The entry that covers `violation` in `scenario`, when one does.
#[must_use]
pub fn covering(scenario: &str, violation: &Violation) -> Option<&'static Quarantine> {
    QUARANTINE.iter().find(|entry| {
        entry.invariant == violation.invariant
            && scenario.starts_with(entry.scenario_prefix)
            && violation.detail.contains(entry.detail_contains)
    })
}
