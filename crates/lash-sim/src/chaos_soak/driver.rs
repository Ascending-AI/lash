//! One soak epoch: every crash-matrix workload at once in one deployment of
//! the production durable runtime, both nodes serving, under the epoch's
//! seeded fault plan; then every fault healed, the run driven to its end and
//! the matrix's invariants and the workloads' laws checked.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_durable::CommitLabel;
use lash_durable_test::{Script, SimClock, SimNodes, Stored, Write, WriteKind};

use super::SoakConfig;
use super::plan::{self, NODES, Step, StepKind};
use super::postgres::DatabaseFaults;
use crate::crash_matrix::Case;
use crate::crash_matrix::deployment::{self, Keep, Workload, activation, nodes_config};
use crate::crash_matrix::invariants;
use crate::crash_matrix::world::{SOAK, World};

/// The workloads every epoch runs together.
pub const CASES: [Case; 6] = [
    Case::Turn,
    Case::Round,
    Case::Cancel,
    Case::Cell,
    Case::Process,
    Case::Close,
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
    /// How many of the plan's faults fired. A step the deployment gave
    /// nothing to act on is not one of them.
    pub reached: usize,
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
            "epoch seed {:#x}: {} steps ({} reached), {} writes ({} committed), ended at {} ms\n  applied: {}\n  violations:\n    {}",
            self.seed,
            self.steps.len(),
            self.reached,
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

/// What applying one step did.
#[derive(Debug)]
enum Applied {
    /// The fault fired.
    Reached(String),
    /// The deployment gave the fault nothing to act on: a database fault on
    /// a dialect that has none, a pause of a node that is not running, a
    /// partition of one that is not serving, a database restart with no
    /// connection to end. The step is recorded and counts for nothing.
    Ineligible(String),
    /// The deployment could take the fault and its executor did not inject
    /// it: the epoch ran without a fault its plan claims, so it fails.
    Failed(String),
}

/// Run one epoch of `config` at `seed`, its database faults injected by the
/// dialect's own executor.
pub async fn epoch(config: &SoakConfig, seed: u64) -> Epoch {
    epoch_under(config, seed, None).await
}

/// [`epoch`] with its database faults injected by `faults` instead of the
/// dialect's own executor.
async fn epoch_under(
    config: &SoakConfig,
    seed: u64,
    faults: Option<Box<dyn DatabaseFaults>>,
) -> Epoch {
    let started = Instant::now();
    let steps = plan::draw(seed, config.steps, &config.without_names());
    let mut epoch = Epoch {
        seed,
        steps: steps.clone(),
        applied: Vec::new(),
        reached: 0,
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
    let node_config = match world.backend() {
        Ok(backend) => nodes_config(&backend),
        Err(error) => {
            epoch.violations.push(error);
            return epoch;
        }
    };
    let nodes = Arc::new(SimNodes::new(
        database,
        Arc::clone(&clock),
        Script::new(),
        node_config,
        activation,
    ));
    let setup = started.elapsed();
    let started = Instant::now();
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

    let faults = faults.or_else(|| {
        deployment::isolated_url(&keep)
            .map(|url| Box::new(super::postgres::Postgres::new(url)) as Box<dyn DatabaseFaults>)
    });
    // The first node step of a plan always finds its node running and
    // serving, and a database fault with an executor fires or fails: a plan
    // that asks this deployment for any fault reaches at least one.
    let floor = usize::from(
        steps
            .iter()
            .any(|step| faults.is_some() || !step.kind.needs_database()),
    );
    let mut fenced = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        advance_to(&nodes, &clock, step.at).await;
        if index > 0 && index.is_multiple_of(WAVE_EVERY) {
            epoch
                .violations
                .extend(wave(&world, &nodes, index / WAVE_EVERY, &mut workloads).await);
        }
        let applied = match apply(step, &nodes, &world, faults.as_deref(), &mut fenced).await {
            Applied::Reached(applied) => {
                epoch.reached += 1;
                applied
            }
            Applied::Ineligible(applied) => applied,
            Applied::Failed(applied) => {
                epoch
                    .violations
                    .push(format!("step {index} was not injected: {applied}"));
                applied
            }
        };
        epoch
            .applied
            .push(format!("{}ms {}", clock.logical_ms(), applied));
    }
    if epoch.reached < floor {
        epoch.violations.push(format!(
            "the epoch reached {} of its plan's {} faults, below its floor of {floor}",
            epoch.reached,
            steps.len()
        ));
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

    let run = started.elapsed();
    let started = Instant::now();

    let trace = nodes.script().trace();
    epoch.writes = trace.len();
    epoch.committed = trace.iter().filter(|write| write.committed()).count();
    epoch.end_ms = clock.logical_ms();
    epoch
        .violations
        // A turn restores, and a Repeatable re-runs, at most once more per
        // interruption: per plan step.
        .extend(
            invariants::check(
                &world,
                &nodes,
                None,
                u64::MAX,
                1 + steps.len(),
                1 + steps.len(),
            )
            .await,
        );
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
    eprintln!(
        "soak seed={seed:#x} steps={} setup={:.6}s run={:.6}s check={:.6}s",
        steps.len(),
        setup.as_secs_f64(),
        run.as_secs_f64(),
        started.elapsed().as_secs_f64(),
    );
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
    faults: Option<&dyn DatabaseFaults>,
    fenced: &mut Vec<Fenced>,
) -> Applied {
    let now = nodes.clock().logical_ms();
    match step.kind {
        StepKind::Kill(node) => {
            nodes.kill(node);
            Applied::Reached(format!("kill {node}"))
        }
        StepKind::Restart(node) => {
            nodes.restart(node);
            Applied::Reached(format!("restart {node}"))
        }
        StepKind::Pause(node, length) => {
            if nodes.life(node) != lash_durable_test::Life::Running {
                return Applied::Ineligible(format!("pause {node}: not running"));
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
            Applied::Reached(format!("pause {node} for {} ms", length.as_millis()))
        }
        StepKind::Partition(node, length) => {
            if !nodes.serving(node) {
                return Applied::Ineligible(format!("partition {node}: not serving"));
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
            Applied::Reached(format!("partition {node} for {} ms", length.as_millis()))
        }
        StepKind::LockTimeout(length) => match faults {
            Some(faults) => match faults.hold_writer_fence(world, length).await {
                Ok(()) => Applied::Reached(format!(
                    "hold the writer fence for {} ms",
                    length.as_millis()
                )),
                Err(error) => Applied::Failed(format!("lock timeout: {error}")),
            },
            None => Applied::Ineligible("lock timeout: PostgreSQL only".to_owned()),
        },
        StepKind::DatabaseRestart => match faults {
            Some(faults) => match faults.terminate_connections().await {
                Ok(0) => Applied::Ineligible("database restart: no connection to end".to_owned()),
                Ok(terminated) => Applied::Reached(format!(
                    "database restart: {terminated} connections terminated"
                )),
                Err(error) => Applied::Failed(format!("database restart: {error}")),
            },
            None => Applied::Ineligible("database restart: PostgreSQL only".to_owned()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crash_matrix::deployment::Dialect;

    /// An executor whose database never takes a fault.
    struct Refusing;

    #[async_trait::async_trait]
    impl DatabaseFaults for Refusing {
        async fn hold_writer_fence(&self, _: &Arc<World>, _: Duration) -> Result<(), String> {
            Err("the holder could not connect".to_owned())
        }

        async fn terminate_connections(&self) -> Result<i64, String> {
            Err("the terminating session could not connect".to_owned())
        }
    }

    /// A fault the plan asks an eligible deployment for and its executor
    /// does not inject fails the epoch: every workload ran to its end
    /// healthy, so nothing else is broken, and an epoch that passed would
    /// count a lock-timeout storm that never happened. With no fault
    /// reached the epoch is below its floor as well.
    #[tokio::test]
    async fn an_epoch_whose_requested_lock_fault_was_not_injected_fails() {
        let config = SoakConfig {
            seed: 0x5748,
            epochs: 1,
            steps: 1,
            without: StepKind::NAMES
                .into_iter()
                .filter(|name| *name != "lock_timeout")
                .map(str::to_owned)
                .collect(),
            dialect: Dialect::SqliteMemory,
            cap: Duration::from_secs(60),
        };
        let epoch = epoch_under(&config, config.seed, Some(Box::new(Refusing))).await;
        assert!(
            matches!(
                epoch.steps[..],
                [Step {
                    kind: StepKind::LockTimeout(_),
                    ..
                }]
            ),
            "the plan asks for one lock-timeout storm: {:?}",
            epoch.steps
        );
        assert_eq!(
            epoch.violations,
            [
                "step 0 was not injected: lock timeout: the holder could not connect",
                "the epoch reached 0 of its plan's 1 faults, below its floor of 1",
            ],
            "{}",
            epoch.evidence()
        );
    }
}
