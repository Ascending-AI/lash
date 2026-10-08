//! The crash matrix: a scenario cut at every labelled write under every
//! fault, recovered on another node, then checked against its laws.
//!
//! A [`Matrix`] runs its scenario once to record the writes it makes. Every
//! write that changed something becomes a cut point, named by node, label
//! and that node's occurrence, or, [across nodes](Matrix::across_nodes), by
//! label and its occurrence among every node's writes. The matrix then
//! re-runs a fresh scenario for each point under each [`Fault`] that means
//! something for that kind of write, drives virtual time until the scenario
//! is done (a paused node resumes only once its lease has expired and its
//! actors moved), and asks the scenario's laws whether they held.

use crate::clock::SimClock;
use crate::nodes::{SimNodes, SimNodesConfig};
use crate::script::{Cut, Fault, Script, Stored, Write, WriteKind};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, CommitLabel, DurableStore, LeaseConfig, LeaseSettings};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// One cut point: a node's `nth` write under a label, or, with no node, the
/// `nth` among every node's writes under it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CutPoint {
    pub node: Option<String>,
    pub label: CommitLabel,
    pub nth: usize,
    pub kind: WriteKind,
}

impl std::fmt::Display for CutPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(node) = &self.node {
            write!(f, "{node}:")?;
        }
        write!(f, "{}#{}", self.label, self.nth)
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
    /// Virtual timer steps in this cell.
    pub steps: usize,
    /// Timer steps while a fault holds a node paused awaiting failover.
    pub failover_steps: usize,
    /// Building the database, clock and nodes.
    pub setup: Duration,
    /// Seeding and driving the scenario to completion.
    pub run: Duration,
    /// Checking its domain laws and collecting its trace.
    pub check: Duration,
}

impl Cell {
    /// Total wall time spent in this cell's three phases.
    pub fn elapsed(&self) -> Duration {
        self.setup + self.run + self.check
    }
}

/// A whole matrix's result.
#[derive(Clone, Debug)]
pub struct MatrixReport {
    /// The uncut run's writes.
    pub baseline: Vec<Write>,
    pub cells: Vec<Cell>,
    /// Virtual timer steps across the uncut run and every cell.
    pub steps: usize,
    /// The actual configured pause before a node may resume after failover.
    pub failover_wait: Duration,
    /// Wall time of the baseline and all cells, including cleanup.
    pub elapsed: Duration,
}

impl MatrixReport {
    fn print_times(&self, law: &str) {
        let mut times: Vec<Duration> = self.cells.iter().map(Cell::elapsed).collect();
        times.sort();
        let p50 = times
            .get(times.len().saturating_sub(1) / 2)
            .copied()
            .unwrap_or_default();
        let slowest = self.cells.iter().max_by_key(|cell| cell.elapsed());
        let max = slowest.map(Cell::elapsed).unwrap_or_default();
        let name = slowest
            .map(|cell| format!("{}/{}", cell.point, cell.fault))
            .unwrap_or_else(|| "none".into());
        let mut output = std::io::stderr().lock();
        let _ = writeln!(
            output,
            "matrix law={law} cells={} total={:.6}s p50={:.6}s max={:.6}s slowest={name}",
            self.cells.len(),
            self.elapsed.as_secs_f64(),
            p50.as_secs_f64(),
            max.as_secs_f64()
        );
        for cell in &self.cells {
            let _ = writeln!(
                output,
                "matrix cell={}/{} setup={:.6}s run={:.6}s check={:.6}s",
                cell.point,
                cell.fault,
                cell.setup.as_secs_f64(),
                cell.run.as_secs_f64(),
                cell.check.as_secs_f64()
            );
        }
    }

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
#[derive(Clone)]
pub struct Matrix {
    faults: Vec<Fault>,
    horizon_ms: u64,
    labels: Option<Vec<CommitLabel>>,
    across_nodes: bool,
    activations_first: bool,
    parallelism: Option<NonZeroUsize>,
    census_runs: NonZeroUsize,
    lease: Option<LeaseConfig>,
}

impl Default for Matrix {
    fn default() -> Self {
        Self::new()
    }
}

impl Matrix {
    /// A short validated lease for crash laws. The heartbeat, self-stop and
    /// reap retain the default's ratios to TTL; claim polling and backoff
    /// keep their production bounds. Heartbeat and reap are multiples of
    /// the 250ms poll, avoiding extra interleaved timer ticks.
    #[must_use]
    #[expect(clippy::expect_used, reason = "the fixed test timings must validate")]
    pub fn test_lease() -> LeaseConfig {
        LeaseSettings {
            ttl: Duration::from_millis(3_750),
            heartbeat_every: Duration::from_millis(750),
            self_stop_after: Duration::from_millis(2_500),
            reap_every: Duration::from_millis(500),
            ..LeaseSettings::default()
        }
        .validate()
        .expect("the matrix test lease respects production lease validation")
    }

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
            across_nodes: false,
            activations_first: false,
            parallelism: None,
            census_runs: census_runs_from_env(),
            lease: None,
        }
    }

    /// Run the uncut scenario `runs` times, spread over the matrix's
    /// parallelism, and require the same census of cut points from every
    /// run: a census that varies between runs leaves cells whose cut a
    /// re-run never reaches. A matrix with no fault to cut runs once. By
    /// default `LASH_MATRIX_CENSUS_RUNS`, else once.
    pub fn census_runs(mut self, runs: NonZeroUsize) -> Self {
        self.census_runs = runs;
        self
    }

    /// Bound the number of cells running at once. By default, use
    /// `LASH_MATRIX_THREADS`, else the available CPUs. An explicit bound
    /// overrides that default.
    pub fn parallelism(mut self, parallelism: NonZeroUsize) -> Self {
        self.parallelism = Some(parallelism);
        self
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

    /// Number each cut among every node's writes under its label, not among
    /// one node's: for a scenario whose count of a label's writes is fixed
    /// but whose split between the nodes is not. Which node claims an actor
    /// first can turn on a store read the fault stores do not count, still
    /// in flight when the clock moves, so a node's own occurrence may never
    /// come in a re-run.
    pub fn across_nodes(mut self) -> Self {
        self.across_nodes = true;
        self
    }

    /// Resume a paused node as soon as none of the scenario's actors is its
    /// own, and run the activations its held reply wakes, and the tasks
    /// they spawn, before its runner ticks again
    /// ([`SimNodes::run_activations_first`]). A resumed process may take
    /// that order, since its runner ticks on a thread of its own: its
    /// activations carry on past its self-stop deadline while their actors'
    /// new owner already runs them. By default a paused node resumes a
    /// failover after its pause, and its runner may tick on a signal while
    /// it is paused.
    pub fn activations_resume_first(mut self) -> Self {
        self.activations_first = true;
        self
    }

    /// Give each run this much virtual time to finish.
    pub fn horizon(mut self, horizon: Duration) -> Self {
        self.horizon_ms = horizon.as_millis() as u64;
        self
    }

    /// Override the scenario's lease, for a verdict-equivalence proof
    /// against another timing configuration.
    pub fn lease(mut self, lease: LeaseConfig) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Run `make`'s scenario uncut, then cut at every point under every
    /// fault that applies there.
    pub async fn run<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport {
        let started = Instant::now();
        let baseline = self.run_one(&make(), None).await;
        assert_eq!(
            baseline.verdict,
            Verdict::Held,
            "the uncut run must hold before any cut means anything\n  {}",
            baseline.trace
        );
        let census = self.census(&baseline.writes);
        let jobs: Vec<Job> = census
            .iter()
            .cloned()
            .flat_map(|point| {
                self.faults
                    .iter()
                    .copied()
                    .filter(move |fault| applies(point.kind, *fault))
                    .map(move |fault| Some((point.clone(), fault)))
            })
            .collect();
        let mut cells: Vec<Cell> = self
            .pool(&make, jobs)
            .into_iter()
            .filter_map(|(cut, run, factory)| {
                let (point, fault) = cut?;
                Some(Cell {
                    point,
                    fault,
                    verdict: run.verdict,
                    trace: run.trace,
                    steps: run.steps,
                    failover_steps: run.failover_steps,
                    setup: factory + run.setup,
                    run: run.run,
                    check: run.check,
                })
            })
            .collect();
        cells.sort_by(|left, right| (&left.point, left.fault).cmp(&(&right.point, right.fault)));
        self.recount(&make, &census, &baseline.trace);
        let steps = baseline.steps + cells.iter().map(|cell| cell.steps).sum::<usize>();
        let report = MatrixReport {
            baseline: baseline.writes,
            cells,
            elapsed: started.elapsed(),
            steps,
            failover_wait: baseline.failover_wait,
        };
        report.print_times(std::thread::current().name().unwrap_or("unnamed"));
        report
    }

    /// Run the uncut scenario again until it ran [`Self::census_runs`]
    /// times, after the cells, so a factory still makes the uncut run's
    /// scenario first and each cell's in order, and require each run's
    /// census to be `census`, the first run's. A matrix that cuts nothing
    /// needs no census and runs once.
    fn recount<S: Scenario>(&self, make: &impl Fn() -> S, census: &[CutPoint], trace: &str) {
        if self.faults.is_empty() || self.census_runs == NonZeroUsize::MIN {
            return;
        }
        let reruns = vec![None; self.census_runs.get() - 1];
        for (run, (_, rerun, _)) in self.pool(make, reruns).into_iter().enumerate() {
            assert_eq!(
                rerun.verdict,
                Verdict::Held,
                "uncut run {} of {} did not hold\n  {}",
                run + 2,
                self.census_runs,
                rerun.trace
            );
            let other = self.census(&rerun.writes);
            assert!(
                other == census,
                "uncut run {} of {} cut other points than the first: a census must be a \
                 function of the scenario\n  first: {}\n  then:  {}\n  first trace: {}\n  \
                 then trace:  {}",
                run + 2,
                self.census_runs,
                render(census),
                render(&other),
                trace,
                rerun.trace
            );
        }
        let _ = writeln!(
            std::io::stderr().lock(),
            "matrix census runs={} points={}",
            self.census_runs,
            census.len()
        );
    }

    /// The uncut run's cut points under the matrix's labels.
    fn census(&self, writes: &[Write]) -> Vec<CutPoint> {
        cut_points(writes, self.across_nodes)
            .into_iter()
            .filter(|point| {
                self.labels
                    .as_ref()
                    .is_none_or(|labels| labels.contains(&point.label))
            })
            .collect()
    }

    /// Run a fresh scenario for each job, cut where it says or uncut, at
    /// most the matrix's parallelism at once. Answers each job with its run
    /// and the time its scenario took to make.
    fn pool<S: Scenario>(
        &self,
        make: &impl Fn() -> S,
        jobs: Vec<Job>,
    ) -> Vec<(Job, Run, Duration)> {
        let width = self.parallelism.unwrap_or_else(parallelism_from_env).get();
        std::thread::scope(|scope| {
            let (completed, ready) = std::sync::mpsc::channel();
            let mut jobs = jobs.into_iter();
            let mut running = 0;
            let mut runs = Vec::new();
            loop {
                while running < width {
                    let Some(cut) = jobs.next() else {
                        break;
                    };
                    let completed = completed.clone();
                    // Construct on the calling thread, preserving factories
                    // that collect scenario evidence in non-Sync state.
                    let started = Instant::now();
                    let scenario = make();
                    let factory = started.elapsed();
                    // Each run owns its thread and runtime. A completed run
                    // admits the next one, even while another is slow.
                    scope.spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let runtime = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .unwrap_or_else(|error| panic!("a matrix cell runtime: {error}"));
                            let finished = runtime.block_on(async {
                                let point = cut.as_ref().map(|(point, fault)| (point, *fault));
                                let run = self.run_one(&scenario, point).await;
                                (cut, run, factory)
                            });
                            // A finished run still owns its isolated database.
                            // Finish teardown before releasing cell admission,
                            // so a replacement cannot overlap its connections.
                            drop(runtime);
                            drop(scenario);
                            finished
                        }));
                        // Send a panic too, so a failed run cannot strand
                        // the receiver waiting for its result.
                        let _ = completed.send(result);
                    });
                    running += 1;
                }
                if running == 0 {
                    break;
                }
                let result = ready
                    .recv()
                    .unwrap_or_else(|error| panic!("a matrix cell result: {error}"));
                running -= 1;
                match result {
                    Ok(run) => runs.push(run),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            runs
        })
    }

    /// FIG-5279: run the same matrix with its scenario lease and the
    /// production default. Every domain cut identity and verdict must agree.
    /// Lease mechanics may have different censuses at different cadences,
    /// but every lease-mechanics cell present in either profile must hold.
    /// The configured lease-expiry wait must be shorter on the test lease.
    /// Prints both step counts and wall times for the performance proof.
    pub async fn run_lease_equivalence<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport {
        let start = Instant::now();
        let report = self.run(&make).await;
        eprintln!(
            "matrix test lease: cells={} steps={} wall={:?}",
            report.cells.len(),
            report.steps,
            start.elapsed(),
        );
        // An uncut run has no cell verdicts to compare. Running it twice
        // would also duplicate any observations the factory retains.
        if report.cells.is_empty() {
            return report;
        }
        let start = Instant::now();
        let reference = self.clone().lease(LeaseConfig::default()).run(&make).await;
        eprintln!(
            "matrix default lease: cells={} steps={} wall={:?}",
            reference.cells.len(),
            reference.steps,
            start.elapsed(),
        );
        for fault in &self.faults {
            let steps = |report: &MatrixReport| {
                report
                    .cells
                    .iter()
                    .filter(|cell| cell.fault == *fault)
                    .map(|cell| cell.steps)
                    .sum::<usize>()
            };
            eprintln!(
                "matrix {fault}: test steps={} default steps={}",
                steps(&report),
                steps(&reference)
            );
        }
        // Domain cuts retain their exact node, label, admitted ordinal,
        // kind and fault. Lease-mechanics cuts can vary with cadence: a
        // short heartbeat may land before a fast baseline ends while the
        // default heartbeat does not. Each such cut must still hold.
        let verdicts = |report: &MatrixReport| {
            report
                .cells
                .iter()
                .filter(|cell| cell.point.kind != WriteKind::Lease)
                .map(|cell| (cell.point.clone(), cell.fault, cell.verdict.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            verdicts(&reference),
            verdicts(&report),
            "FIG-5279: changing the lease must preserve every domain cut and verdict"
        );
        for (profile, report) in [("default", &reference), ("test", &report)] {
            for cell in report
                .cells
                .iter()
                .filter(|cell| cell.point.kind == WriteKind::Lease)
            {
                assert_eq!(
                    cell.verdict,
                    Verdict::Held,
                    "FIG-5279: every {profile} lease-mechanics cell must hold: {} {}\n  {}",
                    cell.point,
                    cell.fault,
                    cell.trace,
                );
            }
        }
        let failover_steps = |report: &MatrixReport| {
            report
                .cells
                .iter()
                .map(|cell| cell.failover_steps)
                .sum::<usize>()
        };
        let before = failover_steps(&reference);
        let after = failover_steps(&report);
        eprintln!("matrix lease-expiry waits: test steps={after} default steps={before}");
        assert!(
            report.failover_wait < reference.failover_wait,
            "FIG-5279: the lease-expiry wait must be shorter: test={:?} default={:?}",
            report.failover_wait,
            reference.failover_wait
        );
        report
    }

    async fn run_one<S: Scenario>(&self, scenario: &S, cut: Option<(&CutPoint, Fault)>) -> Run {
        let started = Instant::now();
        let clock = SimClock::new();
        let database = scenario.database(Arc::clone(&clock)).await;
        let script = Script::new();
        if let Some((point, fault)) = cut {
            match &point.node {
                Some(node) => script.cut_on(node, point.label, point.nth, fault),
                None => script.cut(point.label, point.nth, fault),
            };
        }
        let mut config = scenario.config();
        if let Some(lease) = self.lease {
            config.lease = lease;
        }
        let failover_ms = failover_ms(&config);
        let nodes = Arc::new(SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            config,
            scenario.activation(),
        ));
        if self.activations_first {
            nodes.run_activations_first();
        }
        let setup = started.elapsed();
        let started = Instant::now();
        let mut violations = Vec::new();
        if let Err(error) = scenario.start(&nodes).await {
            violations.push(format!("the scenario did not start: {error}"));
        }
        let mut paused_at: BTreeMap<String, u64> = BTreeMap::new();
        let mut steps = 0;
        let mut failover_steps = 0;
        while violations.is_empty() {
            let mut resumed = Vec::new();
            let mut waiting_for_failover = false;
            for node in nodes.paused() {
                let since = *paused_at.entry(node.clone()).or_insert(clock.logical_ms());
                if (self.activations_first || clock.logical_ms() >= since + failover_ms)
                    && actors_left(&nodes, scenario, &node).await
                {
                    nodes.resume(&node);
                    resumed.push(node);
                } else {
                    waiting_for_failover = true;
                }
            }
            if !resumed.is_empty() {
                // The resumed node's held write and overdue timers run now;
                // its runner too, unless its pause held it.
                nodes.quiesce().await;
                if self.activations_first {
                    for node in &resumed {
                        nodes.release_runner(node);
                    }
                    nodes.quiesce().await;
                }
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
            steps += 1;
            failover_steps += usize::from(waiting_for_failover);
            if nodes.step().await.is_none() {
                violations.push("stalled: no timer is armed and the run is not done".into());
            }
        }
        nodes.quiesce().await;
        let run = started.elapsed();
        let started = Instant::now();
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
            setup,
            run,
            check: started.elapsed(),
            steps,
            failover_steps,
            failover_wait: Duration::from_millis(failover_ms),
        }
    }
}

/// What one run of a pool does: cut at a point under a fault, or run uncut.
type Job = Option<(CutPoint, Fault)>;

struct Run {
    verdict: Verdict,
    trace: String,
    writes: Vec<Write>,
    setup: Duration,
    run: Duration,
    check: Duration,
    steps: usize,
    failover_steps: usize,
    failover_wait: Duration,
}

/// The host's cell budget, else the CPUs available to this action.
#[expect(
    clippy::disallowed_methods,
    reason = "the test host bounds simultaneous isolated cells against its service connection budget (FIG-5340)"
)]
fn parallelism_from_env() -> NonZeroUsize {
    match std::env::var("LASH_MATRIX_THREADS") {
        Ok(value) => value.parse().unwrap_or_else(|error| {
            panic!("LASH_MATRIX_THREADS must be a positive integer: {error}")
        }),
        Err(std::env::VarError::NotPresent) => {
            std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
        }
        Err(error) => panic!("read LASH_MATRIX_THREADS: {error}"),
    }
}

/// `LASH_MATRIX_CENSUS_RUNS`, else one.
#[expect(
    clippy::disallowed_methods,
    reason = "a test harness's knob: a host asks every matrix law for its census-stability check (FIG-5284)"
)]
fn census_runs_from_env() -> NonZeroUsize {
    match std::env::var("LASH_MATRIX_CENSUS_RUNS") {
        Ok(value) => value.parse().unwrap_or_else(|error| {
            panic!("LASH_MATRIX_CENSUS_RUNS must be a positive integer: {error}")
        }),
        Err(std::env::VarError::NotPresent) => NonZeroUsize::MIN,
        Err(error) => panic!("read LASH_MATRIX_CENSUS_RUNS: {error}"),
    }
}

fn render(census: &[CutPoint]) -> String {
    census
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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
/// label and occurrence (`across_nodes`: once per label and occurrence
/// among every node's writes); a node's heartbeats only once, since a pause
/// or a kill already covers every later one.
fn cut_points(writes: &[Write], across_nodes: bool) -> Vec<CutPoint> {
    let mut points: Vec<CutPoint> = writes
        .iter()
        .filter(|write| matches!(write.stored, Stored::Committed { effective: true }))
        .filter(|write| write.point.label != CommitLabel::HEARTBEAT || write.node_nth == 1)
        .map(|write| CutPoint {
            node: (!across_nodes).then(|| write.node.to_string()),
            label: write.point.label,
            nth: if across_nodes {
                write.point.nth
            } else {
                write.node_nth
            },
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::testing::{FORMATS, TestDatabase, actor, sqlite};
    use lash_durable::runner::{Exit, Owned};
    use lash_durable::{
        ActorState, FormatSet, LeaseConfig, LeaseSettings, MailKind, MailTx, Release,
    };
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const LABELS: [CommitLabel; 3] = [
        CommitLabel::new("matrix.z"),
        CommitLabel::new("matrix.y"),
        CommitLabel::new("matrix.x"),
    ];

    struct CellLease(Arc<AtomicUsize>);

    impl Drop for CellLease {
        fn drop(&mut self) {
            // Model synchronous database teardown: its connections remain
            // live until the cell's scenario has finished dropping.
            std::thread::sleep(Duration::from_millis(25));
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct MailScenario {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        _cell: Option<CellLease>,
    }

    #[async_trait::async_trait]
    impl Activation for MailScenario {
        async fn activate(&self, _owned: Owned) -> Exit {
            panic!("this producer-only scenario starts no nodes")
        }
    }

    #[async_trait::async_trait]
    impl Scenario for MailScenario {
        async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let database = sqlite(clock).await;
            // Keep independent setups overlapping, so the pool's bound is
            // observable without relying on the speed of SQLite setup.
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            database
        }

        fn config(&self) -> SimNodesConfig {
            SimNodesConfig {
                lease: Matrix::test_lease(),
                decodes: vec![FormatSet::new(FORMATS)],
                max_active: 1,
            }
        }

        fn activation(&self) -> Arc<dyn Activation> {
            Arc::new(Self {
                active: Arc::clone(&self.active),
                peak: Arc::clone(&self.peak),
                _cell: None,
            })
        }

        async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
            let producer = nodes.producer("producer");
            for label in LABELS {
                let mut error = None;
                for _ in 0..2 {
                    let mut tx = MailTx::new();
                    tx.create_actor(actor(label.as_str()), FormatSet::new(FORMATS))
                        .append(actor(label.as_str()), MailKind::new("test"), "input");
                    match producer.commit_mail(tx, label).await {
                        Ok(_) => {
                            error = None;
                            break;
                        }
                        Err(failed) => error = Some(failed),
                    }
                }
                if let Some(error) = error {
                    return Err(error.to_string());
                }
            }
            Ok(())
        }

        fn actors(&self) -> Vec<ActorKey> {
            LABELS.iter().map(|label| actor(label.as_str())).collect()
        }

        async fn done(&self, _nodes: &SimNodes) -> bool {
            true
        }

        async fn check(&self, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
            let mut violations = Vec::new();
            for actor in self.actors() {
                if !matches!(nodes.database().actor(&actor).await, Ok(Some(row)) if row.pending_mail == 1)
                {
                    violations.push(format!("{actor} must retain exactly one input"));
                }
            }
            violations
        }
    }

    /// FIG-5340: a serial law's internal cells must obey the host's
    /// connection admission, rather than fan out over all available CPUs.
    #[tokio::test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the witness sets its child process's host budget without mutating the test process's environment"
    )]
    async fn default_matrix_cells_obey_the_host_thread_budget() {
        const NAME: &str = "matrix::tests::default_matrix_cells_obey_the_host_thread_budget";
        if std::env::var("LASH_MATRIX_BUDGET_WITNESS").as_deref() != Ok("1") {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("LASH_MATRIX_BUDGET_WITNESS", "1")
                .env("LASH_MATRIX_THREADS", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let live_peak = Arc::new(AtomicUsize::new(0));
        let report = Matrix::new()
            .faults(&[Fault::LostWake, Fault::FailBefore])
            .run(|| {
                let cells = live.fetch_add(1, Ordering::SeqCst) + 1;
                live_peak.fetch_max(cells, Ordering::SeqCst);
                MailScenario {
                    active: Arc::clone(&active),
                    peak: Arc::clone(&peak),
                    _cell: Some(CellLease(Arc::clone(&live))),
                }
            })
            .await;
        report.assert_held();
        assert_eq!(
            live_peak.load(Ordering::SeqCst),
            1,
            "LASH_MATRIX_THREADS must bound live cells through database teardown"
        );
    }

    /// FIG-5278: scheduling changes neither a matrix's verdicts nor its
    /// traces, and at most the requested number of isolated cells run.
    #[tokio::test]
    async fn serial_and_parallel_matrices_have_identical_verdicts_and_traces() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let make = || MailScenario {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
            _cell: None,
        };
        let matrix = || Matrix::new().faults(&[Fault::LostWake, Fault::FailBefore]);
        let serial = matrix()
            .parallelism(NonZeroUsize::new(1).unwrap())
            .run(make)
            .await;
        serial.assert_held();
        assert_eq!(peak.swap(0, Ordering::SeqCst), 1);
        let parallel = matrix()
            .parallelism(NonZeroUsize::new(4).unwrap())
            .run(make)
            .await;
        parallel.assert_held();
        assert!(
            (2..=4).contains(&peak.load(Ordering::SeqCst)),
            "the bounded pool must overlap independent cells"
        );
        assert_eq!(serial.baseline, parallel.baseline);
        let without_times = |report: MatrixReport| {
            report
                .cells
                .into_iter()
                .map(|cell| (cell.point, cell.fault, cell.verdict, cell.trace))
                .collect::<Vec<_>>()
        };
        assert_eq!(without_times(serial), without_times(parallel));
    }

    /// Two nodes over one seeded actor: both claim at once, at the first
    /// instant, and whichever claims it acknowledges its mail and lets it
    /// idle. `slow`'s registration is held up on the wall clock.
    struct Contended {
        lease: LeaseConfig,
        slow: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl Activation for Contended {
        async fn activate(&self, owned: Owned) -> Exit {
            if let Ok(mut tx) = owned.begin().await {
                tx.ack_seen().give_up(Release::Idle);
                let _ = owned.commit(tx, CommitLabel::new("contended.ack")).await;
            }
            Exit::Released
        }
    }

    #[async_trait::async_trait]
    impl Scenario for Contended {
        async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
            let database = TestDatabase::new(sqlite(clock).await);
            Arc::new(match self.slow {
                Some(node) => database.slow_registration(node, Duration::from_millis(30)),
                None => database,
            })
        }

        fn config(&self) -> SimNodesConfig {
            SimNodesConfig {
                lease: self.lease,
                decodes: vec![FormatSet::new(FORMATS)],
                max_active: 1,
            }
        }

        fn activation(&self) -> Arc<dyn Activation> {
            Arc::new(Self {
                lease: self.lease,
                slow: self.slow,
            })
        }

        async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
            let mut seed = MailTx::new();
            seed.create_actor(actor("one"), FormatSet::new(FORMATS))
                .append(actor("one"), MailKind::new("test"), "input");
            nodes
                .database()
                .commit_mail(seed, CommitLabel::new("contended.seed"))
                .await
                .map_err(|error| error.to_string())?;
            nodes.start("b");
            nodes.start("a");
            Ok(())
        }

        fn actors(&self) -> Vec<ActorKey> {
            vec![actor("one")]
        }

        async fn done(&self, nodes: &SimNodes) -> bool {
            matches!(
                nodes.database().actor(&actor("one")).await,
                Ok(Some(row)) if row.state == ActorState::Idle && row.pending_mail == 0
            )
        }

        async fn check(&self, _nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
            Vec::new()
        }
    }

    /// FIG-5284: which node claims a contended actor is fixed by the
    /// scenario, never by timing. Two nodes claim one actor at one instant
    /// while either one's registration is held up on the wall clock; under
    /// the default and a short lease, twenty uncut runs one at a time and
    /// twenty at once all cut the same points, the first node by name
    /// claims in every one, and a cell that fails either registration
    /// still holds.
    #[tokio::test]
    async fn which_node_claims_is_fixed_by_the_scenario_never_by_timing() {
        let short = LeaseSettings {
            ttl: Duration::from_millis(3_750),
            heartbeat_every: Duration::from_millis(750),
            self_stop_after: Duration::from_millis(2_500),
            reap_every: Duration::from_millis(500),
            ..LeaseConfig::default().settings()
        }
        .validate()
        .unwrap();
        let runs = NonZeroUsize::new(20).unwrap();
        for lease in [LeaseConfig::default(), short] {
            let mut censuses = Vec::new();
            for slow in [None, Some("a"), Some("b")] {
                for width in [NonZeroUsize::MIN, runs] {
                    let report = Matrix::new()
                        .faults(&[Fault::FailBefore])
                        .labels(&[CommitLabel::NODE_REGISTER])
                        .census_runs(runs)
                        .parallelism(width)
                        .run(|| Contended { lease, slow })
                        .await;
                    report.assert_held();
                    let claims: Vec<String> = report
                        .baseline
                        .iter()
                        .filter(|write| {
                            write.point.label == CommitLabel::CLAIM
                                && matches!(write.stored, Stored::Committed { effective: true })
                        })
                        .map(|write| write.node.to_string())
                        .collect();
                    assert_eq!(
                        claims,
                        vec!["a".to_string()],
                        "slow {slow:?}, width {width}: the first node by name claims"
                    );
                    censuses.push((slow, width, render(&cut_points(&report.baseline, false))));
                }
            }
            let (_, _, first) = &censuses[0];
            for (slow, width, census) in &censuses {
                assert_eq!(census, first, "slow {slow:?}, width {width}");
            }
        }
    }
}
