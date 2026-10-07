//! The crash matrix: a scenario cut at every labelled write under every
//! fault, recovered on another node, then checked against its laws.
//!
//! A [`Matrix`] runs its scenario once to record the writes it makes. Every
//! write that changed something becomes a cut point, named by node, label
//! and that node's occurrence. The matrix then re-runs a fresh scenario for
//! each point under each [`Fault`] that means something for that kind of
//! write, drives virtual time until the scenario is done (a paused node
//! resumes only once its lease has expired and its actors moved), and asks
//! the scenario's laws whether they held.

use crate::clock::SimClock;
use crate::nodes::{SimNodes, SimNodesConfig};
use crate::script::{Cut, Fault, Script, Stored, Write, WriteKind};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, CommitLabel, DurableStore};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// One run of a deployment, built fresh for every cell of a matrix.
#[async_trait::async_trait]
pub trait Scenario: Send + Sync {
    /// A fresh, empty database whose clock is `clock`.
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore>;

    /// How the deployment's nodes run.
    fn config(&self) -> SimNodesConfig;

    /// What a node does with each actor it claims.
    fn activation(&self) -> Arc<dyn Activation>;

    /// Start the nodes and seed the work.
    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String>;

    /// The actors whose ownership a paused node must lose before it
    /// resumes.
    fn actors(&self) -> Vec<ActorKey>;

    /// Whether the run reached its end.
    async fn done(&self, nodes: &SimNodes) -> bool;

    /// The scenario's laws after the run; one line per violation.
    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String>;
}

/// Whether a cell's laws held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Held,
    Violated(Vec<String>),
    /// The re-run never reached the write the cell cuts.
    Unreached,
}

/// One cut point: a node's `nth` write under a label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CutPoint {
    pub node: String,
    pub label: CommitLabel,
    pub nth: usize,
    pub kind: WriteKind,
}

impl std::fmt::Display for CutPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}#{}", self.node, self.label, self.nth)
    }
}

/// One cell of a matrix and its verdict.
#[derive(Clone, Debug)]
pub struct Cell {
    pub point: CutPoint,
    pub fault: Fault,
    pub verdict: Verdict,
    /// The run's write trace, rendered.
    pub trace: String,
}

/// A whole matrix's result.
#[derive(Clone, Debug)]
pub struct MatrixReport {
    /// The uncut run's writes.
    pub baseline: Vec<Write>,
    pub cells: Vec<Cell>,
}

impl MatrixReport {
    /// Every cell whose laws did not hold, or whose cut was never reached.
    pub fn failures(&self) -> Vec<&Cell> {
        self.cells
            .iter()
            .filter(|cell| cell.verdict != Verdict::Held)
            .collect()
    }

    /// Panic with every failing cell, its violations and its trace.
    pub fn assert_held(&self) {
        let failures = self.failures();
        if failures.is_empty() {
            return;
        }
        let rendered: Vec<String> = failures
            .iter()
            .map(|cell| {
                let why = match &cell.verdict {
                    Verdict::Held => String::new(),
                    Verdict::Unreached => "cut never reached".to_string(),
                    Verdict::Violated(violations) => violations.join("; "),
                };
                format!("{} {}: {why}\n  {}", cell.point, cell.fault, cell.trace)
            })
            .collect();
        panic!(
            "{} of {} matrix cells failed:\n{}",
            failures.len(),
            self.cells.len(),
            rendered.join("\n")
        );
    }

    /// The distinct labels the matrix cut.
    pub fn labels(&self) -> Vec<CommitLabel> {
        let mut labels: Vec<CommitLabel> = self.cells.iter().map(|cell| cell.point.label).collect();
        labels.sort();
        labels.dedup();
        labels
    }
}

/// A crash matrix over one scenario.
pub struct Matrix {
    faults: Vec<Fault>,
    horizon_ms: u64,
    labels: Option<Vec<CommitLabel>>,
}

impl Default for Matrix {
    fn default() -> Self {
        Self::new()
    }
}

impl Matrix {
    /// Every fault, a 2 s delayed ack, and a ten-minute virtual horizon.
    pub fn new() -> Self {
        Self {
            faults: vec![
                Fault::FailBefore,
                Fault::Abort,
                Fault::CommitThenAbort,
                Fault::AckHidden,
                Fault::DelayedAck(Duration::from_secs(2)),
                Fault::StaleEpoch,
                Fault::Zombie,
                Fault::LostWake,
            ],
            horizon_ms: 600_000,
            labels: None,
        }
    }

    /// Cut with these faults only.
    pub fn faults(mut self, faults: &[Fault]) -> Self {
        self.faults = faults.to_vec();
        self
    }

    /// Cut only writes under these labels.
    pub fn labels(mut self, labels: &[CommitLabel]) -> Self {
        self.labels = Some(labels.to_vec());
        self
    }

    /// Give each run this much virtual time to finish.
    pub fn horizon(mut self, horizon: Duration) -> Self {
        self.horizon_ms = horizon.as_millis() as u64;
        self
    }

    /// Run `make`'s scenario uncut, then cut at every point under every
    /// fault that applies there.
    pub async fn run<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport {
        let baseline = self.run_one(&make(), None).await;
        assert_eq!(
            baseline.verdict,
            Verdict::Held,
            "the uncut run must hold before any cut means anything\n  {}",
            baseline.trace
        );
        let mut cells = Vec::new();
        for point in cut_points(&baseline.writes).into_iter().filter(|point| {
            self.labels
                .as_ref()
                .is_none_or(|labels| labels.contains(&point.label))
        }) {
            for fault in self
                .faults
                .iter()
                .copied()
                .filter(|fault| applies(point.kind, *fault))
            {
                let run = self.run_one(&make(), Some((&point, fault))).await;
                cells.push(Cell {
                    point: point.clone(),
                    fault,
                    verdict: run.verdict,
                    trace: run.trace,
                });
            }
        }
        MatrixReport {
            baseline: baseline.writes,
            cells,
        }
    }

    async fn run_one<S: Scenario>(&self, scenario: &S, cut: Option<(&CutPoint, Fault)>) -> Run {
        let clock = SimClock::new();
        let database = scenario.database(Arc::clone(&clock)).await;
        let script = Script::new();
        if let Some((point, fault)) = cut {
            script.cut_on(&point.node, point.label, point.nth, fault);
        }
        let config = scenario.config();
        let failover_ms = failover_ms(&config);
        let nodes = Arc::new(SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            config,
            scenario.activation(),
        ));
        let mut violations = Vec::new();
        if let Err(error) = scenario.start(&nodes).await {
            violations.push(format!("the scenario did not start: {error}"));
        }
        let mut paused_at: BTreeMap<String, u64> = BTreeMap::new();
        while violations.is_empty() {
            let mut resumed = false;
            for node in nodes.paused() {
                let since = *paused_at.entry(node.clone()).or_insert(clock.logical_ms());
                if clock.logical_ms() >= since + failover_ms
                    && actors_left(&nodes, scenario, &node).await
                {
                    nodes.resume(&node);
                    resumed = true;
                }
            }
            if resumed {
                // The resumed node's held write and overdue timers run now.
                nodes.quiesce().await;
            }
            if nodes.paused().is_empty() && scenario.done(&nodes).await {
                break;
            }
            if clock.logical_ms() >= self.horizon_ms {
                violations.push(format!(
                    "not done after {} ms of virtual time",
                    self.horizon_ms
                ));
                break;
            }
            if nodes.step().await.is_none() {
                violations.push("stalled: no timer is armed and the run is not done".into());
            }
        }
        nodes.quiesce().await;
        let cuts = nodes.script().cuts();
        violations.extend(scenario.check(&nodes, cuts.first()).await);
        let unreached = !nodes.script().disarm_unfired().is_empty();
        let trace = nodes.script().rendered_trace();
        let writes = nodes.script().trace();
        let verdict = if unreached {
            Verdict::Unreached
        } else if violations.is_empty() {
            Verdict::Held
        } else {
            Verdict::Violated(violations)
        };
        Run {
            verdict,
            trace,
            writes,
        }
    }
}

struct Run {
    verdict: Verdict,
    trace: String,
    writes: Vec<Write>,
}

/// How long a paused node stays paused: past its lease, a reap and a claim,
/// so another node has fenced and taken its actors.
fn failover_ms(config: &SimNodesConfig) -> u64 {
    let settings = config.lease.settings();
    (settings.ttl + settings.reap_every + settings.claim_poll).as_millis() as u64
}

/// Whether `node` owns none of the scenario's actors any more.
async fn actors_left<S: Scenario>(nodes: &SimNodes, scenario: &S, node: &str) -> bool {
    for actor in scenario.actors() {
        if let Ok(Some(snapshot)) = nodes.database().actor(&actor).await
            && snapshot
                .owner
                .is_some_and(|owner| owner.node.as_str() == node)
        {
            return false;
        }
    }
    true
}

/// Every write of the uncut run that changed something, once per node,
/// label and occurrence; a node's heartbeats only once, since a pause or
/// a kill already covers every later one.
fn cut_points(writes: &[Write]) -> Vec<CutPoint> {
    let mut points: Vec<CutPoint> = writes
        .iter()
        .filter(|write| matches!(write.stored, Stored::Committed { effective: true }))
        .filter(|write| write.point.label != CommitLabel::HEARTBEAT || write.node_nth == 1)
        .map(|write| CutPoint {
            node: write.node.to_string(),
            label: write.point.label,
            nth: write.node_nth,
            kind: write.kind,
        })
        .collect();
    points.sort();
    points.dedup();
    points
}

/// Whether `fault` means something on a write of `kind`.
fn applies(kind: WriteKind, fault: Fault) -> bool {
    match fault {
        Fault::LostWake => kind == WriteKind::Mail,
        Fault::StaleEpoch => kind == WriteKind::Actor,
        Fault::Zombie => kind != WriteKind::Mail,
        Fault::FailBefore
        | Fault::Abort
        | Fault::CommitThenAbort
        | Fault::AckHidden
        | Fault::DelayedAck(_) => true,
    }
}
