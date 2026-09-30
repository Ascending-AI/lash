//! The seeded plan of one soak epoch: a pure function of the epoch's seed.
//!
//! [`plan`] draws every step up front from the seed alone, so an epoch's
//! plan is the same on every run and a shorter run's plan is a prefix of a
//! longer one's: a failing epoch replays from its seed, and its step count
//! can be cut down to the shortest prefix that still fails.

use crate::crash_matrix::deployment::HostSite;

/// A session the plan opened, by its index in the epoch's session list.
pub type SessionRef = usize;

/// What a session is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Plain inputs, batches and commands. Inputs the engine admits together
    /// batch into one root, so every input is answered but not every input
    /// owns a root.
    Plain,
    /// One held root at a time, cancelled or deleted by the step that sent
    /// it: nothing else is ever queued beside it, so it is always the head of
    /// its own root.
    Held,
}

/// One step of an epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Open a session, as a child of `parent` when set.
    Open {
        session: SessionRef,
        lane: Lane,
        parent: Option<SessionRef>,
    },
    /// Accept one input.
    Send { session: SessionRef, root: String },
    /// Accept several inputs as one request.
    SendBatch {
        session: SessionRef,
        roots: Vec<String>,
    },
    /// Submit a session command on the command lane.
    Command { session: SessionRef, key: String },
    /// Send a root whose model call never answers, wait until it runs, and
    /// cancel it. With `child`, a child process lives until the root ends.
    CancelHeld {
        session: SessionRef,
        root: String,
        child: bool,
    },
    /// Send a held root, wait until it runs, and delete its session.
    DeleteHeld {
        session: SessionRef,
        root: String,
        child: bool,
    },
    /// Delete a session (its close intent, then its physical delete).
    Delete { session: SessionRef },
    /// Start a Lashlang process that sleeps and finishes, and arm an engine
    /// waiter on its terminal.
    StartProcess { sleep_ms: u64 },
    /// Run the recovery interval `count` times on virtual time.
    Tick { count: u32 },
    /// Wait until the engine and the host have nothing left to do.
    Quiesce,
    /// Kill the deployment where it stands and bring up a fresh one.
    Kill,
    /// Crash the next first attempt of `service` at journal command `index`.
    ArmEngineCut { service: &'static str, index: usize },
    /// Crash the host the next time a call reaches `site`.
    ArmHostCrash { site: HostSite },
    /// Another holder takes the recovery leader lease for `ticks` ticks and
    /// then gives it up.
    LeaseLoss { ticks: u32 },
    /// A rolling deploy: register build N+1, move the deployment onto it,
    /// drain generation N and retire its build.
    Roll,
}

impl Step {
    /// The step's kind, as `LASH_CHAOS_SOAK_WITHOUT` names it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Send { .. } => "send",
            Self::SendBatch { .. } => "send_batch",
            Self::Command { .. } => "command",
            Self::CancelHeld { .. } => "cancel_held",
            Self::DeleteHeld { .. } => "delete_held",
            Self::Delete { .. } => "delete",
            Self::StartProcess { .. } => "start_process",
            Self::Tick { .. } => "tick",
            Self::Quiesce => "quiesce",
            Self::Kill => "kill",
            Self::ArmEngineCut { .. } => "engine_cut",
            Self::ArmHostCrash { .. } => "host_crash",
            Self::LeaseLoss { .. } => "lease_loss",
            Self::Roll => "roll",
        }
    }
}

/// The step kinds a triage run may leave out: every kind but the ones that
/// open sessions, send, or move time, which later steps build on.
pub const OPTIONAL_KINDS: [&str; 11] = [
    "send_batch",
    "command",
    "cancel_held",
    "delete_held",
    "delete",
    "start_process",
    "kill",
    "engine_cut",
    "host_crash",
    "lease_loss",
    "roll",
];

/// The services an engine cut draws from, with the journal commands each
/// handler of the workload writes on its first attempt.
const ENGINE_CUTS: [(&str, u64); 3] = [
    (lash_restate_test::SESSION_DRIVER_SERVICE, 8),
    (lash_restate_test::TURN_DRIVER_SERVICE, 20),
    ("LashProcessWorkflow", 14),
];

/// The host sites a crash draws from: every seam boundary the crash matrix
/// cuts, with the crash effect only (a refusal or a retryable failure
/// forever is a stall surface the matrix owns, not a crash).
const HOST_SITES: [HostSite; 7] = [
    HostSite::DriveAsk,
    HostSite::ReleaseRootBefore,
    HostSite::ReleaseRootAfter,
    HostSite::AcknowledgeIntentBefore,
    HostSite::DeleteStorageBefore,
    HostSite::DeliverCancelBefore,
    HostSite::DeliverCancelAfter,
];

/// The sleeps a process draws from: some finish inside a tick, some outlive
/// a rolling deploy's first drain ticks.
const SLEEPS_MS: [u64; 4] = [500, 5_000, 20_000, 45_000];

/// How many sessions an epoch keeps open at most.
const MAX_SESSIONS: usize = 8;

/// The generator's view of one session.
#[derive(Clone, Copy, Debug)]
struct Planned {
    lane: Lane,
    open: bool,
}

/// The sessions an epoch opens before its first drawn step: two plain
/// sessions and one held lane.
#[must_use]
pub fn opening() -> Vec<Step> {
    vec![
        Step::Open {
            session: 0,
            lane: Lane::Plain,
            parent: None,
        },
        Step::Open {
            session: 1,
            lane: Lane::Plain,
            parent: None,
        },
        Step::Open {
            session: 2,
            lane: Lane::Held,
            parent: None,
        },
    ]
}

/// The first `steps` steps of the epoch under `seed`, after [`opening`]. A
/// step whose kind `without` names is a [`Step::Quiesce`] instead; the draws
/// are the same, so every other step stays where it was.
#[must_use]
pub fn plan(seed: u64, steps: usize, without: &[String]) -> Vec<Step> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut sessions: Vec<Planned> = opening()
        .iter()
        .map(|step| match step {
            Step::Open { lane, .. } => Planned {
                lane: *lane,
                open: true,
            },
            _ => unreachable!("the opening only opens sessions"),
        })
        .collect();
    let mut plan = opening();
    let mut next_input = 0_u64;
    let mut root = |prefix: &str| {
        next_input += 1;
        format!("{prefix}-{next_input}")
    };
    while plan.len() < steps + opening().len() {
        let open = |lane: Lane, sessions: &[Planned]| -> Vec<SessionRef> {
            sessions
                .iter()
                .enumerate()
                .filter(|(_, planned)| planned.open && planned.lane == lane)
                .map(|(index, _)| index)
                .collect()
        };
        let plain = open(Lane::Plain, &sessions);
        let held = open(Lane::Held, &sessions);
        let live = sessions.iter().filter(|planned| planned.open).count();
        let pick = |rng: &mut fastrand::Rng, from: &[SessionRef]| from[rng.usize(0..from.len())];
        // Weights, out of 100.
        let draw = rng.u32(0..100);
        let step = match draw {
            0..=23 if !plain.is_empty() => Step::Send {
                session: pick(&mut rng, &plain),
                root: root("in"),
            },
            24..=31 if !plain.is_empty() => {
                let session = pick(&mut rng, &plain);
                let count = rng.usize(2..5);
                Step::SendBatch {
                    session,
                    roots: (0..count).map(|_| root("batch")).collect(),
                }
            }
            32..=36 if !plain.is_empty() => Step::Command {
                session: pick(&mut rng, &plain),
                key: root("command"),
            },
            37..=42 if !held.is_empty() => Step::CancelHeld {
                session: pick(&mut rng, &held),
                root: root("held"),
                child: rng.bool(),
            },
            43..=44 if held.len() > 1 => {
                let session = pick(&mut rng, &held);
                sessions[session].open = false;
                Step::DeleteHeld {
                    session,
                    root: root("held"),
                    child: rng.bool(),
                }
            }
            45..=48 if live < MAX_SESSIONS => {
                let lane = if rng.u32(0..4) == 0 {
                    Lane::Held
                } else {
                    Lane::Plain
                };
                let parent = (!plain.is_empty() && rng.bool()).then(|| pick(&mut rng, &plain));
                sessions.push(Planned { lane, open: true });
                Step::Open {
                    session: sessions.len() - 1,
                    lane,
                    parent,
                }
            }
            49..=51 if plain.len() > 1 => {
                let session = pick(&mut rng, &plain);
                sessions[session].open = false;
                Step::Delete { session }
            }
            52..=59 => Step::StartProcess {
                sleep_ms: SLEEPS_MS[rng.usize(0..SLEEPS_MS.len())],
            },
            60..=71 => Step::Tick {
                count: rng.u32(1..4),
            },
            72..=79 => Step::Quiesce,
            80..=84 => Step::Kill,
            85..=89 => {
                let (service, commands) = ENGINE_CUTS[rng.usize(0..ENGINE_CUTS.len())];
                Step::ArmEngineCut {
                    service,
                    index: 1 + rng.u64(0..commands) as usize,
                }
            }
            90..=93 => Step::ArmHostCrash {
                site: HOST_SITES[rng.usize(0..HOST_SITES.len())],
            },
            94..=96 => Step::LeaseLoss {
                ticks: rng.u32(1..6),
            },
            97..=99 => Step::Roll,
            // A draw whose precondition fails (no session of its lane) falls
            // back to a plain tick.
            _ => Step::Tick { count: 1 },
        };
        if without.iter().any(|kind| kind == step.kind()) {
            plan.push(Step::Quiesce);
        } else {
            plan.push(step);
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_is_a_function_of_its_seed_and_a_shorter_plan_is_a_prefix() {
        let long = plan(0x3873, 200, &[]);
        assert_eq!(long, plan(0x3873, 200, &[]));
        assert_eq!(plan(0x3873, 50, &[])[..], long[..50 + opening().len()]);
        assert_ne!(long, plan(0x3874, 200, &[]));
        let without = plan(0x3873, 200, &["roll".to_owned()]);
        for (step, kept) in long.iter().zip(&without) {
            if step.kind() == "roll" {
                assert_eq!(kept, &Step::Quiesce);
            } else {
                assert_eq!(kept, step);
            }
        }
    }

    #[test]
    fn every_step_kind_is_drawn() {
        let steps = plan(7, 2_000, &[]);
        let kinds: std::collections::BTreeSet<&str> = steps.iter().map(Step::kind).collect();
        assert_eq!(kinds.len(), 15, "{kinds:?}");
    }
}
