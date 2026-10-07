//! One soak epoch: every crash-matrix workload at once in one deployment of
//! the production durable runtime, both nodes serving, under the epoch's
//! seeded fault plan; then every fault healed, the run driven to its end and
//! the matrix's invariants and the workloads' laws checked.

use std::sync::Arc;
use std::time::Duration;

use lash_durable::CommitLabel;
use lash_durable_test::{Script, SimClock, SimNodes, Stored, Write, WriteKind};

use super::SoakConfig;
use super::plan::{self, NODES, Step, StepKind};
use crate::crash_matrix::Case;
use crate::crash_matrix::deployment::{self, Keep, Workload, activation, nodes_config};
use crate::crash_matrix::invariants;
use crate::crash_matrix::world::{SOAK, World};

/// The workloads every epoch runs together.
pub const CASES: [Case; 8] = [
    Case::Turn,
    Case::Round,
    Case::Cancel,
    Case::Cell,
    Case::Process,
    Case::Signal,
    Case::Close,
    Case::Trigger,
];

/// A new wave of every workload starts every this many steps, so faults
/// land while work runs.
const WAVE_EVERY: usize = 3;

/// How long an epoch may run past its last step before it is stalled.
const AFTER_PLAN: Duration = Duration::from_secs(600);

/// One epoch's outcome.
#[derive(Clone, Debug)]
pub struct Epoch {
    /// The plan's seed, from which the epoch replays.
    pub seed: u64,
    /// The plan.
    pub steps: Vec<Step>,
    /// What the driver did at each step.
    pub applied: Vec<String>,
    /// Every invariant or law the epoch broke.
    pub violations: Vec<String>,
    /// Labelled writes the epoch made, and how many committed.
    pub writes: usize,
    pub committed: usize,
    /// Virtual time at the end.
    pub end_ms: u64,
}

impl Epoch {
    /// Whether every invariant held.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// What a failed epoch shows: its seed, its plan as applied and what it
    /// broke.
    #[must_use]
    pub fn evidence(&self) -> String {
        format!(
            "epoch seed {:#x}: {} steps, {} writes ({} committed), ended at {} ms\n  applied: {}\n  violations:\n    {}",
            self.seed,
            self.steps.len(),
            self.writes,
            self.committed,
            self.end_ms,
            self.applied.join("; "),
            self.violations.join("\n    ")
        )
    }
}

/// A window in which a node could not renew its lease while another node
/// served: once a reap fenced it, nothing it commits may land.
#[derive(Clone, Copy, Debug)]
struct Fenced {
    node: &'static str,
    from_ms: u64,
}

/// Run one epoch of `config` at `seed`.
pub async fn epoch(config: &SoakConfig, seed: u64) -> Epoch {
    let steps = plan::draw(seed, config.steps, &config.without_names());
    let mut epoch = Epoch {
        seed,
        steps: steps.clone(),
        applied: Vec::new(),
        violations: Vec::new(),
        writes: 0,
        committed: 0,
        end_ms: 0,
    };
    let world = Arc::new(World::default());
    world.note(SOAK);
    let clock = SimClock::new();
    let mut keep = Keep::new();
    let database = match deployment::open(&config.dialect, Arc::clone(&clock), &mut keep).await {
        Ok((backend, database)) => {
            world.set_parts(backend, Arc::clone(&clock));
            database
        }
        Err(error) => {
            epoch
                .violations
                .push(format!("the database did not open: {error}"));
            return epoch;
        }
    };
    let activation = match activation(&world, deployment::slow(seed)) {
        Ok(activation) => activation,
        Err(error) => {
            epoch.violations.push(error);
            return epoch;
        }
    };
    let nodes = Arc::new(SimNodes::new(
        database,
        Arc::clone(&clock),
        Script::new(),
        nodes_config(),
        activation,
    ));
    if let Err(error) = world.start_host(&nodes) {
        epoch.violations.push(error);
        return epoch;
    }
    let mut workloads: Vec<(Case, Box<dyn Workload>)> = Vec::new();
    epoch
        .violations
        .extend(wave(&world, &nodes, 0, &mut workloads).await);
    for node in NODES {
        nodes.start(node);
        nodes.quiesce().await;
    }

    let admin = deployment::isolated_url(&keep);
    let mut fenced = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        advance_to(&nodes, &clock, step.at).await;
        if index > 0 && index.is_multiple_of(WAVE_EVERY) {
            epoch
                .violations
                .extend(wave(&world, &nodes, index / WAVE_EVERY, &mut workloads).await);
        }
        let applied = apply(step, &nodes, &world, admin.as_deref(), &mut fenced).await;
        epoch
            .applied
            .push(format!("{}ms {}", clock.logical_ms(), applied));
    }
    // Every fault ends: partitions heal, paused nodes resume, dead ones
    // restart, as their supervisors would.
    for node in NODES {
        nodes.heal(node);
        nodes.resume(node);
        if !nodes.serving(node) {
            nodes.start(node);
        }
    }
    let horizon = clock.logical_ms() + u64::try_from(AFTER_PLAN.as_millis()).unwrap_or(u64::MAX);
    loop {
        if all_done(&workloads, &world, &nodes).await {
            break;
        }
        if clock.logical_ms() >= horizon {
            epoch.violations.push(format!(
                "not done {} s after the plan",
                AFTER_PLAN.as_secs()
            ));
            for (case, workload) in &workloads {
                if !workload.done(&world, &nodes).await {
                    epoch
                        .violations
                        .push(format!("{} is not done", case.name()));
                }
            }
            break;
        }
        if nodes.step().await.is_none() {
            epoch
                .violations
                .push("stalled: no timer is armed and the run is not done".to_owned());
            break;
        }
    }
    nodes.quiesce().await;

    let trace = nodes.script().trace();
    epoch.writes = trace.len();
    epoch.committed = trace.iter().filter(|write| write.committed()).count();
    epoch.end_ms = clock.logical_ms();
    epoch
        .violations
        .extend(invariants::check(&world, &nodes, None, u64::MAX, 1 + steps.len()).await);
    epoch.violations.extend(fencing(&fenced, &trace));
    for (case, workload) in &workloads {
        epoch.violations.extend(
            workload
                .laws(&world, &nodes, None)
                .await
                .into_iter()
                .map(|violation| format!("{}: {violation}", case.name())),
        );
    }
    world.stop_tasks();
    drop(keep);
    epoch
}

/// Seed wave `number` of every workload, its sessions named apart.
async fn wave(
    world: &Arc<World>,
    nodes: &Arc<SimNodes>,
    number: usize,
    workloads: &mut Vec<(Case, Box<dyn Workload>)>,
) -> Vec<String> {
    let mut violations = Vec::new();
    for case in CASES {
        let workload = case.workload_tagged(&format!("-w{number}"));
        if let Err(error) = workload.seed(world, nodes).await {
            violations.push(format!(
                "{} wave {number} did not seed: {error}",
                case.name()
            ));
        }
        workloads.push((case, workload));
    }
    violations
}

async fn all_done(
    workloads: &[(Case, Box<dyn Workload>)],
    world: &World,
    nodes: &SimNodes,
) -> bool {
    for (_, workload) in workloads {
        if !workload.done(world, nodes).await {
            return false;
        }
    }
    true
}

/// Drive the deployment until virtual time reaches `at`.
async fn advance_to(nodes: &SimNodes, clock: &SimClock, at: Duration) {
    let target = u64::try_from(at.as_millis()).unwrap_or(u64::MAX);
    while clock.logical_ms() < target {
        let due = clock.next_due();
        if due.is_none_or(|due| due > target) {
            clock.advance_to(target).await;
            nodes.quiesce().await;
        } else if nodes.step().await.is_none() {
            clock.advance_to(target).await;
        }
    }
}

/// Apply `step`, answering what it did.
async fn apply(
    step: &Step,
    nodes: &Arc<SimNodes>,
    world: &Arc<World>,
    admin: Option<&str>,
    fenced: &mut Vec<Fenced>,
) -> String {
    let now = nodes.clock().logical_ms();
    match step.kind {
        StepKind::Kill(node) => {
            nodes.kill(node);
            format!("kill {node}")
        }
        StepKind::Restart(node) => {
            nodes.restart(node);
            format!("restart {node}")
        }
        StepKind::Pause(node, length) => {
            if nodes.life(node) != lash_durable_test::Life::Running {
                return format!("pause {node}: not running");
            }
            nodes.pause(node);
            fenced.push(Fenced { node, from_ms: now });
            let resumed = Arc::downgrade(nodes);
            let timer = Arc::clone(world);
            world.spawn(async move {
                timer.sleep(length).await;
                if let Some(nodes) = resumed.upgrade() {
                    nodes.resume(node);
                }
            });
            format!("pause {node} for {} ms", length.as_millis())
        }
        StepKind::Partition(node, length) => {
            if !nodes.serving(node) {
                return format!("partition {node}: not serving");
            }
            nodes.partition(node);
            fenced.push(Fenced { node, from_ms: now });
            let healed = Arc::downgrade(nodes);
            let timer = Arc::clone(world);
            world.spawn(async move {
                timer.sleep(length).await;
                if let Some(nodes) = healed.upgrade() {
                    nodes.heal(node);
                }
            });
            format!("partition {node} for {} ms", length.as_millis())
        }
        StepKind::LockTimeout(length) => match admin {
            Some(url) => match super::postgres::hold_writer_fence(world, url, length).await {
                Ok(()) => format!("hold the writer fence for {} ms", length.as_millis()),
                Err(error) => format!("lock timeout not injected: {error}"),
            },
            None => "lock timeout: PostgreSQL only".to_owned(),
        },
        StepKind::DatabaseRestart => match admin {
            Some(url) => match super::postgres::terminate_connections(url).await {
                Ok(terminated) => format!("database restart: {terminated} connections terminated"),
                Err(error) => format!("database restart not injected: {error}"),
            },
            None => "database restart: PostgreSQL only".to_owned(),
        },
    }
}

/// F1 across the soak: once another node reaped a paused or partitioned
/// node, no owner commit that node makes later lands until it registers or
/// claims again.
fn fencing(fenced: &[Fenced], trace: &[Write]) -> Vec<String> {
    let mut violations = Vec::new();
    for window in fenced {
        let Some(reaped_at) = trace
            .iter()
            .find(|write| {
                write.point.label == CommitLabel::REAP
                    && *write.node != *window.node
                    && write.at_ms > window.from_ms
                    && write.stored == Stored::Committed { effective: true }
            })
            .map(|write| write.at_ms)
        else {
            continue;
        };
        for write in trace
            .iter()
            .filter(|write| *write.node == *window.node && write.at_ms > reaped_at)
        {
            let claimed_again = write.kind == WriteKind::Lease
                && (write.point.label == CommitLabel::NODE_REGISTER
                    || (write.point.label == CommitLabel::CLAIM
                        && write.stored == Stored::Committed { effective: true }));
            if claimed_again {
                break;
            }
            if write.kind == WriteKind::Actor && write.committed() {
                violations.push(format!(
                    "F1: {} wrote {write} after a reap fenced it at {reaped_at} ms",
                    window.node
                ));
            }
        }
    }
    violations
}
