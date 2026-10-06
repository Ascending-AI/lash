//! The defects the soak found, and what the smoke leaves out until each is
//! fixed.
//!
//! Each [`Finding`] names the step kinds that expose it and a replay that
//! shows it. [`smoke_without`] is the union of the [`OPEN`] findings' kinds:
//! the smoke mode, which must stay green on `main`, draws none of them, and
//! the release soak draws every kind. Each finding has a regression test in
//! `tests/chaos_soak.rs` that runs its replay: ignored while the defect
//! stands, live once it is [`FIXED`]. The change that fixes one moves its
//! entry from [`OPEN`] to [`FIXED`] and deletes the `ignore` of its test; the
//! registry test refuses a mismatch, and the smoke draws the kinds again.

use super::{EpochReport, run_epoch};

/// A defect the soak found, with the replay that shows it.
#[derive(Clone, Copy, Debug)]
pub struct Finding {
    /// The id the ignore reason and the report name: `<ticket> S<n>`, under
    /// the ticket whose work found it.
    pub id: &'static str,
    /// What goes wrong, in one line.
    pub summary: &'static str,
    /// The step kinds that expose it: the smoke leaves them out.
    pub exposed_by: &'static [&'static str],
    /// The replay: the epoch seed, its plan length, and the kinds it leaves
    /// out to keep the trace short.
    pub seed: u64,
    pub steps: usize,
    pub without: &'static [&'static str],
}

impl Finding {
    /// Run the finding's replay: one epoch of its plan.
    pub async fn replay(&self) -> EpochReport {
        let without: Vec<String> = self.without.iter().map(|kind| (*kind).to_owned()).collect();
        Box::pin(run_epoch(0, self.seed, self.steps, &without)).await
    }
}

/// Every defect the soak found that `main` still has.
pub const OPEN: &[Finding] = &[];

/// The defects the soak found that `main` has fixed: each replay must pass.
/// Each `FIG-3873` finding was found before the plan drew park steps, and
/// leaves them out, so its replay is the plan it was found on.
pub const FIXED: &[Finding] = &[
    // FIG-3892: a session command enqueued between an input run's
    // admission and its execution held the admission back, so the run retried an
    // admission race forever and the command never drained behind it.
    Finding {
        id: "FIG-3873 S1",
        summary: "a session with a queued command wedges: its admitted head run \
                  fails every admission as `session_execution_lane_busy: missed its head \
                  on an admission race`, parks EngineRetryExhausted after 8 attempts, \
                  and every later input queues behind it uncommitted",
        exposed_by: &["command"],
        seed: 0x4299_608a_2be8_dd17,
        steps: 40,
        without: &[
            "cancel_held",
            "delete_held",
            "park_redrive",
            "delete",
            "start_process",
            "kill",
            "engine_cut",
            "host_crash",
            "lease_loss",
            "roll",
        ],
    },
    // FIG-3893: a queued-work run whose settling execution recorded its
    // scope close replayed without it, because the close was gated on
    // whether this execution wrote the run's evidence rather than on the
    // durable evidence its replayed run carries. Later fixes moved this
    // seed's schedule off the cut that exposed it; the shift-admission law
    // `a_command_runs_redrive_replays_its_recorded_outcome` pins the
    // redelivery itself.
    Finding {
        id: "FIG-3873 S2",
        summary: "a queued-work (command) run whose first attempt dies replays \
                  divergently: Restate journal mismatch 570 (recorded `set state` at \
                  index 4, the replay attempts `run`), so the run retries until it \
                  pauses and pins its build past the rolling deploy's drain",
        exposed_by: &["command"],
        seed: 0x70b3_4810_d30c_b07a,
        steps: 200,
        without: &["park_redrive"],
    },
    // FIG-3894: a turn whose final commit the close cut short left its
    // closure pinned, which no activation of a closing session drains, so
    // the physical delete refused it for good, and a deletion retried after
    // its close was refused by the same pin.
    Finding {
        id: "FIG-3873 S3",
        summary: "a session deleted while a run is in flight across a deployment \
                  death never finishes deleting: the close names the run, the turn \
                  stays in flight with no engine invocation and no park, and the \
                  SessionDelete obligation stays Due",
        exposed_by: &["delete"],
        seed: 0x6005,
        steps: 96,
        without: &["command", "park_redrive"],
    },
    // FIG-3895: the generation drain read nothing of a closing session,
    // whose runs' waits stay registered with the engine until the physical
    // delete; and the soak's rolling deploy held the deployment it rolled
    // onto across its whole drain, so one a crash replaced mid-drain kept
    // the recovery lease and nothing retried the delete.
    Finding {
        id: "FIG-3873 S4",
        summary: "deleting a session whose run is running, when the host dies \
                  inside the first delete and a retry completes it, leaves the \
                  run's `turn_cancel_gate` durable wait suspended for good: the \
                  LashDurableWaitWorkflow invocation pins its build, so a rolling \
                  deploy never retires that generation although lash reads it \
                  drained",
        exposed_by: &["delete_held"],
        seed: 0xa005,
        steps: 68,
        without: &["command", "delete", "park_redrive"],
    },
    // FIG-3896: a recovery tick cancelled after the store granted its
    // deployment the lease left the lease to a holder nobody ran, so no
    // deployment claimed a due obligation again.
    Finding {
        id: "FIG-3873 S5",
        summary: "after a deployment kill, a cancelled run whose held \
                  child it ended never finishes its scope close: the run's \
                  scope-close obligation stays Claimed and its parent-end \
                  obligation stays Due, neither delivered nor stalled",
        exposed_by: &["cancel_held"],
        seed: 0xe646_becf_47ba_ea87,
        steps: 200,
        without: &[
            "command",
            "delete",
            "delete_held",
            "lease_loss",
            "park_redrive",
        ],
    },
    // FIG-4780 (found by FIG-4718): a park ended only when a turn commit
    // named its run, and a command run commits no turn, so the park of one
    // that was redriven and ran outlived it. A run's terminal write now ends
    // its park, whatever kind of run it is. Found by the redrive checker's
    // first replay of S1's seed with park steps in its plan, so its replay
    // draws them.
    Finding {
        id: "FIG-4718 S6",
        summary: "a queued-command run whose execution stops retrying is parked, and its \
                  redrive is acknowledged and resumes it, but nothing ends the park: the \
                  command applies and the session shifts its later inputs while the turn \
                  park feed never records `unparked` for the command run",
        exposed_by: &["park_redrive"],
        seed: 0x4299_608a_2be8_dd17,
        steps: 40,
        without: &[
            "cancel_held",
            "delete_held",
            "delete",
            "start_process",
            "kill",
            "engine_cut",
            "host_crash",
            "lease_loss",
            "roll",
        ],
    },
];

/// The step kinds the smoke leaves out: every open finding's.
#[must_use]
pub fn smoke_without() -> Vec<String> {
    let mut kinds: Vec<String> = OPEN
        .iter()
        .flat_map(|finding| finding.exposed_by.iter())
        .map(|kind| (*kind).to_owned())
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_finding_names_optional_kinds() {
        for finding in OPEN.iter().chain(FIXED) {
            for kind in finding.exposed_by.iter().chain(finding.without) {
                assert!(
                    super::super::plan::OPTIONAL_KINDS.contains(kind),
                    "{} names `{kind}`, which a plan cannot leave out",
                    finding.id
                );
            }
            for kind in finding.exposed_by {
                assert!(
                    !finding.without.contains(kind),
                    "{}'s replay leaves out `{kind}`, which exposes it",
                    finding.id
                );
            }
        }
    }
}
