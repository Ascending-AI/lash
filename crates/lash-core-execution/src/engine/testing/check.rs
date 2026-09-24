//! The determinism check and the seam every engine leg plugs into.
//!
//! [`DeterminismEngine`] is what an engine gives the check: one fresh run under
//! a seed, returning its transcript and whatever history the engine replays
//! from, and a replay of that history in each [`ReplayMode`]. The check runs
//! the fresh run once, then a cold replay, a replay on a separate worker, and
//! replays under seeded scheduling perturbation, and compares every replay's
//! transcript with the fresh one.
//!
//! [`LocalEngine`] is the engine-free leg over [`LocalTestCx`]. An engine
//! adapter implements the same trait over its own test double.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::cx::{CxMode, DEFAULT_BODY_TIMEOUT, DriveJournal, LocalTestCx, RunFailure, RunRecord};
use super::schedule::{Schedule, derived_seed};
use super::transcript::{DriveTranscript, TranscriptDivergence};

/// How a replay re-runs a fresh run's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    /// A new drive over the recorded history, on the worker that ran it fresh,
    /// with every recorded outcome delivered as soon as it is awaited.
    Cold,
    /// A new drive over the recorded history, decoded from bytes on a worker
    /// that shares no process-local state with the one that ran it fresh.
    SeparateWorker,
    /// A cold replay whose outcome delivery is held and reordered under
    /// `seed`.
    Perturbed {
        /// The perturbation seed.
        seed: u64,
    },
}

/// Which run of a check failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// The fresh run.
    Fresh,
    /// A replay.
    Replay(ReplayMode),
}

/// A fresh run: its transcript, and the history a replay is served from.
#[derive(Clone, Debug)]
pub struct EngineRun<H> {
    /// The fresh run's command stream and commit bytes.
    pub transcript: DriveTranscript,
    /// What the engine recorded.
    pub history: H,
}

/// One engine's leg of the determinism check.
///
/// The drive under test is the engine's to hold: an implementation is built
/// over one drive and runs it on each call.
pub trait DeterminismEngine {
    /// What the engine recorded on a fresh run.
    type History;

    /// Run the drive fresh, with the engine's scheduling seeded by `seed`.
    fn fresh(&self, seed: u64) -> Result<EngineRun<Self::History>, RunFailure>;

    /// Re-run the drive over `history` in `mode`.
    fn replay(
        &self,
        history: &Self::History,
        mode: ReplayMode,
    ) -> Result<DriveTranscript, RunFailure>;
}

/// The check: one fresh run, then cold, separate-worker and perturbed
/// replays, each compared with the fresh transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeterminismCheck {
    /// The fresh run's seed, from which every perturbation seed derives.
    pub seed: u64,
    /// How many perturbed replays to run.
    pub perturbed_replays: u32,
}

impl DeterminismCheck {
    /// A check under `seed` with four perturbed replays.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            perturbed_replays: 4,
        }
    }

    /// Set the number of perturbed replays.
    pub fn perturbed_replays(mut self, count: u32) -> Self {
        self.perturbed_replays = count;
        self
    }

    /// The replay modes this check runs, in order.
    pub fn replay_modes(&self) -> Vec<ReplayMode> {
        let mut modes = vec![ReplayMode::Cold, ReplayMode::SeparateWorker];
        modes.extend(
            (0..self.perturbed_replays).map(|index| ReplayMode::Perturbed {
                seed: derived_seed(self.seed, index),
            }),
        );
        modes
    }

    /// Run the check against `engine`.
    pub fn run<E: DeterminismEngine>(
        &self,
        engine: &E,
    ) -> Result<DeterminismReport, DeterminismFailure> {
        let fresh = engine
            .fresh(self.seed)
            .map_err(|failure| DeterminismFailure {
                mode: RunMode::Fresh,
                cause: FailureCause::Run(failure),
            })?;
        let modes = self.replay_modes();
        for mode in &modes {
            let failed = |cause| DeterminismFailure {
                mode: RunMode::Replay(*mode),
                cause,
            };
            let replayed = engine
                .replay(&fresh.history, *mode)
                .map_err(|failure| failed(FailureCause::Run(failure)))?;
            fresh
                .transcript
                .compare(&replayed)
                .map_err(|divergence| failed(FailureCause::Diverged(divergence)))?;
        }
        Ok(DeterminismReport {
            transcript: fresh.transcript,
            replays: modes,
        })
    }
}

/// A passed check.
#[derive(Clone, Debug)]
pub struct DeterminismReport {
    /// The fresh run's transcript, which every replay reproduced.
    pub transcript: DriveTranscript,
    /// The replays that reproduced it.
    pub replays: Vec<ReplayMode>,
}

/// Why a run failed the check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureCause {
    /// The run did not complete.
    Run(RunFailure),
    /// The run completed with a different transcript.
    Diverged(Box<TranscriptDivergence>),
}

/// A failed check: the run that failed and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeterminismFailure {
    /// The run that failed.
    pub mode: RunMode,
    /// Why it failed.
    pub cause: FailureCause,
}

impl fmt::Display for DeterminismFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            RunMode::Fresh => "the fresh run".to_string(),
            RunMode::Replay(ReplayMode::Cold) => "the cold replay".to_string(),
            RunMode::Replay(ReplayMode::SeparateWorker) => {
                "the replay on a separate worker".to_string()
            }
            RunMode::Replay(ReplayMode::Perturbed { seed }) => {
                format!("the replay perturbed under seed {seed:#x}")
            }
        };
        match &self.cause {
            FailureCause::Run(failure) => write!(formatter, "{mode} failed: {failure}"),
            FailureCause::Diverged(divergence) => {
                write!(
                    formatter,
                    "{mode} diverged from the fresh run: {divergence}"
                )
            }
        }
    }
}

impl std::error::Error for DeterminismFailure {}

/// A drive the local engine runs: given the worker's state and a context,
/// the drive future. The future may be `!Send`.
pub type LocalDrive<W> =
    dyn for<'c> Fn(&'c W, &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>> + Send + Sync;

/// Builds one worker's process-local state: registries, caches, anything a
/// drive reads that is not recorded. Each worker builds its own.
pub type WorkerState<W> = dyn Fn() -> W + Send + Sync;

/// The engine-free leg of the check over [`LocalTestCx`].
///
/// The fresh run, the cold replay and the perturbed replays run on one home
/// worker thread, over one worker state. The separate-worker replay runs on a
/// new thread over a new worker state, from the journal encoded to bytes, so a
/// drive that reads process-local state it did not record diverges there.
pub struct LocalEngine<W: 'static> {
    worker: Arc<WorkerState<W>>,
    drive: Arc<LocalDrive<W>>,
    body_timeout: Duration,
    home: Result<Home, String>,
}

struct Home {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

struct Job {
    mode: CxMode,
    schedule: Schedule,
    reply: mpsc::Sender<Result<RunRecord, RunFailure>>,
}

impl<W: 'static> LocalEngine<W> {
    /// An engine over `drive`, whose workers build their state with `worker`.
    pub fn new<M, D>(worker: M, drive: D) -> Self
    where
        M: Fn() -> W + Send + Sync + 'static,
        D: for<'c> Fn(&'c W, &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>>
            + Send
            + Sync
            + 'static,
    {
        Self::with_body_timeout(worker, drive, DEFAULT_BODY_TIMEOUT)
    }

    /// As [`new`](Self::new), with an explicit operation-body timeout.
    pub fn with_body_timeout<M, D>(worker: M, drive: D, body_timeout: Duration) -> Self
    where
        M: Fn() -> W + Send + Sync + 'static,
        D: for<'c> Fn(&'c W, &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>>
            + Send
            + Sync
            + 'static,
    {
        let worker: Arc<WorkerState<W>> = Arc::new(worker);
        let drive: Arc<LocalDrive<W>> = Arc::new(drive);
        let home = spawn_home(Arc::clone(&worker), Arc::clone(&drive), body_timeout);
        Self {
            worker,
            drive,
            body_timeout,
            home,
        }
    }

    fn on_home(&self, mode: CxMode, schedule: Schedule) -> Result<RunRecord, RunFailure> {
        let home = self.home.as_ref().map_err(|message| RunFailure::Engine {
            message: message.clone(),
        })?;
        let (reply, answer) = mpsc::channel();
        home.jobs
            .as_ref()
            .ok_or_else(|| lost_worker("the home worker"))?
            .send(Job {
                mode,
                schedule,
                reply,
            })
            .map_err(|_| lost_worker("the home worker"))?;
        answer.recv().map_err(|_| lost_worker("the home worker"))?
    }

    fn on_separate_worker(&self, journal: Vec<u8>) -> Result<RunRecord, RunFailure> {
        let worker = Arc::clone(&self.worker);
        let drive = Arc::clone(&self.drive);
        let body_timeout = self.body_timeout;
        let thread = std::thread::Builder::new()
            .name("lash-determinism-separate-worker".to_string())
            .spawn(move || {
                let runtime = body_runtime()?;
                let state = worker();
                let journal = DriveJournal::from_bytes(&journal)?;
                run_job(
                    &state,
                    drive.as_ref(),
                    &runtime,
                    body_timeout,
                    CxMode::Replay(Arc::new(journal)),
                    Schedule::Immediate,
                )
            })
            .map_err(|error| RunFailure::Engine {
                message: format!("could not start a separate worker: {error}"),
            })?;
        thread
            .join()
            .map_err(|_| lost_worker("the separate worker"))?
    }
}

impl<W: 'static> DeterminismEngine for LocalEngine<W> {
    type History = DriveJournal;

    fn fresh(&self, seed: u64) -> Result<EngineRun<DriveJournal>, RunFailure> {
        let record = self.on_home(CxMode::Fresh, Schedule::Perturbed { seed })?;
        Ok(EngineRun {
            transcript: record.transcript,
            history: record.journal,
        })
    }

    fn replay(
        &self,
        history: &DriveJournal,
        mode: ReplayMode,
    ) -> Result<DriveTranscript, RunFailure> {
        let record = match mode {
            ReplayMode::Cold => self.on_home(
                CxMode::Replay(Arc::new(history.clone())),
                Schedule::Immediate,
            )?,
            ReplayMode::SeparateWorker => self.on_separate_worker(history.to_bytes()?)?,
            ReplayMode::Perturbed { seed } => self.on_home(
                CxMode::Replay(Arc::new(history.clone())),
                Schedule::Perturbed { seed },
            )?,
        };
        Ok(record.transcript)
    }
}

impl<W: 'static> Drop for LocalEngine<W> {
    fn drop(&mut self) {
        if let Ok(home) = self.home.as_mut() {
            home.jobs.take();
            if let Some(thread) = home.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

fn spawn_home<W: 'static>(
    worker: Arc<WorkerState<W>>,
    drive: Arc<LocalDrive<W>>,
    body_timeout: Duration,
) -> Result<Home, String> {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let thread = std::thread::Builder::new()
        .name("lash-determinism-home-worker".to_string())
        .spawn(move || {
            let runtime = body_runtime();
            let state = worker();
            for job in inbox {
                let result = match &runtime {
                    Ok(runtime) => run_job(
                        &state,
                        drive.as_ref(),
                        runtime,
                        body_timeout,
                        job.mode,
                        job.schedule,
                    ),
                    Err(failure) => Err(failure.clone()),
                };
                let _ = job.reply.send(result);
            }
        })
        .map_err(|error| format!("could not start the home worker: {error}"))?;
    Ok(Home {
        jobs: Some(jobs),
        thread: Some(thread),
    })
}

fn run_job<W>(
    state: &W,
    drive: &LocalDrive<W>,
    runtime: &tokio::runtime::Runtime,
    body_timeout: Duration,
    mode: CxMode,
    schedule: Schedule,
) -> Result<RunRecord, RunFailure> {
    let cx = LocalTestCx::with_body_timeout(mode, schedule, runtime.handle().clone(), body_timeout);
    cx.run(drive(state, &cx))
}

/// The runtime operation bodies run on: the execution side of a worker.
fn body_runtime() -> Result<tokio::runtime::Runtime, RunFailure> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("lash-determinism-bodies")
        .enable_all()
        .build()
        .map_err(|error| RunFailure::Engine {
            message: format!("could not build the operation-body runtime: {error}"),
        })
}

fn lost_worker(which: &str) -> RunFailure {
    RunFailure::Engine {
        message: format!("{which} stopped before it answered"),
    }
}
