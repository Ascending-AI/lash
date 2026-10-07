//! A soak epoch's fault plan: a seeded sequence of steps, each at a virtual
//! time, that the driver applies to the deployment while its workloads run.
//!
//! A plan is a function of its seed, and a shorter plan of one seed is a
//! prefix of a longer one, so a failed epoch replays from its seed and a
//! plan can be shortened to find the step that broke it.

use std::time::Duration;

/// A node of the soak's deployment.
pub type Node = &'static str;

/// What one step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// The node dies where it stands: every task and byte it held is gone.
    Kill(Node),
    /// A new boot of the node starts (a dead node's supervisor restarting
    /// it, or a rolling restart of a live one).
    Restart(Node),
    /// The node pauses whole (a long GC, a `SIGSTOP`) for this long: past a
    /// lease it resumes as a zombie whose commits the fence must refuse.
    Pause(Node, Duration),
    /// The node's heartbeats fail for this long while its bodies run on:
    /// past `self_stop_after` it stops itself.
    Partition(Node, Duration),
    /// Another session holds the writer fence's row for this long, so the
    /// engine's transactions hit their `lock_timeout` (PostgreSQL only).
    LockTimeout(Duration),
    /// The database restarts: every connection is terminated (PostgreSQL
    /// only).
    DatabaseRestart,
}

impl StepKind {
    /// The kinds a plan draws from, by name.
    pub const NAMES: [&'static str; 6] = [
        "kill",
        "restart",
        "pause",
        "partition",
        "lock_timeout",
        "database_restart",
    ];

    /// The step kind's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kill(_) => "kill",
            Self::Restart(_) => "restart",
            Self::Pause(..) => "pause",
            Self::Partition(..) => "partition",
            Self::LockTimeout(_) => "lock_timeout",
            Self::DatabaseRestart => "database_restart",
        }
    }
}

/// One step, at a virtual time from the epoch's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub at: Duration,
    pub kind: StepKind,
}

/// The nodes a plan picks from.
pub const NODES: [Node; 2] = ["a", "b"];

/// A seeded fault plan of `len` steps, `without` the kinds named.
#[must_use]
pub fn draw(seed: u64, len: usize, without: &[&str]) -> Vec<Step> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut at = Duration::ZERO;
    let mut steps = Vec::with_capacity(len);
    while steps.len() < len {
        // Every draw takes the same random numbers whatever it keeps, so a
        // shorter plan is a prefix of a longer one.
        at += Duration::from_millis(rng.u64(200..4_000));
        let node = NODES[rng.usize(..NODES.len())];
        let pick = rng.usize(..StepKind::NAMES.len());
        let long = Duration::from_millis(rng.u64(18_000..30_000));
        let short = Duration::from_millis(rng.u64(500..12_000));
        let kind = match pick {
            0 => StepKind::Kill(node),
            1 => StepKind::Restart(node),
            2 => StepKind::Pause(node, long),
            3 => StepKind::Partition(node, if rng.bool() { long } else { short }),
            4 => StepKind::LockTimeout(short),
            _ => StepKind::DatabaseRestart,
        };
        if !without.contains(&kind.name()) {
            steps.push(Step { at, kind });
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan is a function of its seed, and a shorter plan of one seed is
    /// a prefix of a longer one, so a failed epoch replays and shrinks.
    #[test]
    fn a_plan_is_a_function_of_its_seed_and_a_shorter_plan_is_a_prefix() {
        let long = draw(7, 40, &[]);
        assert_eq!(long, draw(7, 40, &[]));
        assert_eq!(draw(7, 12, &[]), long[..12]);
        assert_ne!(long, draw(8, 40, &[]));
        assert!(long.windows(2).all(|pair| pair[0].at < pair[1].at));
    }

    /// Every step kind is drawn, and a kind left out never is.
    #[test]
    fn every_step_kind_is_drawn() {
        let plan = draw(11, 200, &[]);
        for name in StepKind::NAMES {
            assert!(
                plan.iter().any(|step| step.kind.name() == name),
                "{name} was never drawn"
            );
        }
        let without = draw(11, 200, &["pause", "kill"]);
        assert!(
            without
                .iter()
                .all(|step| !matches!(step.kind, StepKind::Pause(..) | StepKind::Kill(_)))
        );
    }
}
