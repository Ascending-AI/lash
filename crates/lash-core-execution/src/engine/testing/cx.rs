//! `LocalTestCx`: the engine-free local test context, and the single-thread
//! executor that drives a drive future over it.
//!
//! The context records every operation a drive issues — its replay key, kind
//! and canonical command bytes — into a [`DriveTranscript`], runs the
//! operation's body on a fresh run and journals the outcome, and on a replay
//! serves the journaled outcome without running the body, after checking the
//! reissued command against the journaled one.
//!
//! The executor is the wake rule. A drive may be resumed only by one of its
//! operations settling: the executor wakes an operation's waker itself, inside
//! a marked region, and any other wake of the drive — a Tokio channel, a
//! spawned task, a timer, a `yield_now`, a wake from another thread — fails the
//! run as [`RunFailure::NonOpWake`]. A drive that is pending while no
//! operation is in flight is awaiting something that is not an operation, and
//! fails as [`RunFailure::NonOpAwait`]. This is Temporal's non-SDK-wake rule
//! (TMPRL1100) and the engine-free generalization of the synchronous-wake
//! tracker the Restate adapter guards its context futures with.
//!
//! Operation bodies are the execution side, not drive code. They run under a
//! Tokio runtime the executor owns, polled with a step waker that never reaches
//! the drive: a body's own wakes, from any thread, only tell the executor the
//! body has progressed. Drive code runs outside any runtime context, so a
//! `tokio::spawn` or a Tokio timer in drive code fails loudly.

use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use lash_sansio::sync::MutexExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::schedule::{Schedule, Scheduler};
use super::transcript::{DriveTranscript, TranscriptEntry, byte_difference};

thread_local! {
    /// Set while the executor wakes a settled operation's waker: the one wake
    /// of the drive that is caused by an operation.
    static OPERATION_WAKE: Cell<bool> = const { Cell::new(false) };
}

/// How long the executor waits for an in-flight operation body to make
/// progress before it fails the run.
pub const DEFAULT_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// One journaled operation: the command a drive issued and, once the body
/// settled, its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The operation's replay identity.
    pub key: String,
    /// The operation's kind.
    pub kind: String,
    /// The command's canonical bytes.
    pub command: String,
    /// The outcome's bytes, or `None` when the body never settled.
    pub outcome: Option<String>,
}

/// The recorded history of one fresh run: what a replay is served from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveJournal {
    /// Every operation the fresh run issued, in issue order.
    pub entries: Vec<JournalEntry>,
}

impl DriveJournal {
    /// Encode the journal for another worker.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RunFailure> {
        serde_json::to_vec(self).map_err(|error| RunFailure::Codec {
            key: "journal".to_string(),
            message: error.to_string(),
        })
    }

    /// Decode a journal another worker encoded.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RunFailure> {
        serde_json::from_slice(bytes).map_err(|error| RunFailure::Codec {
            key: "journal".to_string(),
            message: error.to_string(),
        })
    }

    fn entry(&self, key: &str) -> Option<&JournalEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }
}

/// A replayed operation that does not match the journal it is served from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayDivergence {
    /// The replay issued an operation the journal does not hold.
    UnrecordedCommand { key: String, kind: String },
    /// The replay issued an operation whose kind or bytes differ from the
    /// journaled command at the same key.
    CommandMismatch {
        key: String,
        recorded_kind: String,
        issued_kind: String,
        difference: String,
    },
    /// The run issued a second operation under a key it already used.
    DuplicateKey { key: String },
}

impl fmt::Display for ReplayDivergence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrecordedCommand { key, kind } => write!(
                formatter,
                "the replay issued `{kind}` at `{key}`, which the journal does not hold"
            ),
            Self::CommandMismatch {
                key,
                recorded_kind,
                issued_kind,
                difference,
            } => write!(
                formatter,
                "the replay issued `{issued_kind}` at `{key}` over the journaled \
                 `{recorded_kind}`; its bytes differ {difference}"
            ),
            Self::DuplicateKey { key } => {
                write!(formatter, "the run issued a second operation at `{key}`")
            }
        }
    }
}

/// Why a run did not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFailure {
    /// A replayed command did not match its journal.
    Divergence(ReplayDivergence),
    /// Something other than an operation woke the drive.
    NonOpWake { round: u64 },
    /// The drive was pending while no operation was in flight: it awaited a
    /// future that is not an operation.
    NonOpAwait { round: u64 },
    /// An operation body made no progress within the body timeout.
    BodyTimeout { key: String },
    /// A command, outcome or commit did not encode or decode.
    Codec { key: String, message: String },
    /// The drive panicked.
    Panicked { message: String },
    /// The engine under test refused the run, in its own words.
    Engine { message: String },
}

impl fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Divergence(divergence) => write!(formatter, "replay divergence: {divergence}"),
            Self::NonOpWake { round } => write!(
                formatter,
                "round {round}: the drive was woken by something other than an operation"
            ),
            Self::NonOpAwait { round } => write!(
                formatter,
                "round {round}: the drive is pending with no operation in flight; it awaits a \
                 future that is not an operation"
            ),
            Self::BodyTimeout { key } => {
                write!(formatter, "the body of `{key}` made no progress in time")
            }
            Self::Codec { key, message } => write!(formatter, "`{key}` did not encode: {message}"),
            Self::Panicked { message } => write!(formatter, "the drive panicked: {message}"),
            Self::Engine { message } => write!(formatter, "the engine refused the run: {message}"),
        }
    }
}

impl std::error::Error for RunFailure {}

/// What a completed run left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRecord {
    /// The run's command stream and commit bytes.
    pub transcript: DriveTranscript,
    /// The run's journal: what it recorded on a fresh run, what it was served
    /// from on a replay.
    pub journal: DriveJournal,
}

/// Whether a context records a fresh run or replays a journal.
#[derive(Clone, Debug)]
pub enum CxMode {
    /// Run every body and journal its outcome.
    Fresh,
    /// Serve outcomes from this journal.
    Replay(Arc<DriveJournal>),
}

/// The engine-free local test context. See the module documentation.
pub struct LocalTestCx {
    shared: Arc<Shared>,
}

struct Shared {
    mode: CxMode,
    runtime: tokio::runtime::Handle,
    body_timeout: Duration,
    state: Mutex<State>,
    progress: Condvar,
}

struct State {
    scheduler: Scheduler,
    slots: Vec<Slot>,
    transcript: DriveTranscript,
    failure: Option<RunFailure>,
}

struct Slot {
    key: String,
    kind: String,
    command: String,
    waker: Option<Waker>,
    hold: u32,
    progressed: bool,
    status: SlotStatus,
}

enum SlotStatus {
    /// The body is running.
    Running,
    /// The outcome is known and waits for delivery.
    Settled(String),
    /// The drive received the outcome.
    Delivered(String),
    /// The drive dropped the operation before its outcome was delivered.
    Dropped,
    /// The operation diverged from the journal; the run is over.
    Refused,
}

impl LocalTestCx {
    /// A context in `mode`, scheduling under `schedule`, running bodies on
    /// `runtime`.
    pub fn new(mode: CxMode, schedule: Schedule, runtime: tokio::runtime::Handle) -> Self {
        Self::with_body_timeout(mode, schedule, runtime, DEFAULT_BODY_TIMEOUT)
    }

    /// As [`new`](Self::new), with an explicit body timeout.
    pub fn with_body_timeout(
        mode: CxMode,
        schedule: Schedule,
        runtime: tokio::runtime::Handle,
        body_timeout: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                mode,
                runtime,
                body_timeout,
                state: Mutex::new(State {
                    scheduler: Scheduler::new(schedule),
                    slots: Vec::new(),
                    transcript: DriveTranscript::default(),
                    failure: None,
                }),
                progress: Condvar::new(),
            }),
        }
    }

    /// Whether this context replays a journal.
    pub fn is_replaying(&self) -> bool {
        matches!(self.shared.mode, CxMode::Replay(_))
    }

    /// Issue a recorded operation.
    ///
    /// The command is recorded at the call, in program order. On a fresh run
    /// `body` runs and its outcome is journaled; on a replay the journaled
    /// outcome is served and `body` is dropped unpolled, unless the journal
    /// holds the command without an outcome, when the body runs live as an
    /// engine would. Either way the drive receives the outcome decoded from its
    /// journaled bytes, so a fresh run and its replay see the same value.
    pub fn op<'a, C, T, F>(
        &'a self,
        key: impl Into<String>,
        kind: impl Into<String>,
        command: &C,
        body: F,
    ) -> Op<'a, T>
    where
        C: Serialize + ?Sized,
        T: Serialize + DeserializeOwned + 'a,
        F: Future<Output = T> + Send + 'a,
    {
        let key = key.into();
        let kind = kind.into();
        let command = match serde_json::to_string(command) {
            Ok(command) => command,
            Err(error) => {
                let failure = RunFailure::Codec {
                    key: key.clone(),
                    message: error.to_string(),
                };
                return self.refused_op(key, kind, String::new(), failure);
            }
        };
        self.op_with_command_bytes(key, kind, command, body)
    }

    /// As [`op`](Self::op), for a command whose canonical bytes the caller
    /// already holds.
    pub fn op_with_command_bytes<'a, T, F>(
        &'a self,
        key: String,
        kind: String,
        command: String,
        body: F,
    ) -> Op<'a, T>
    where
        T: Serialize + DeserializeOwned + 'a,
        F: Future<Output = T> + Send + 'a,
    {
        let mut state = self.shared.state.lock_recover();
        state.transcript.entries.push(TranscriptEntry::Command {
            key: key.clone(),
            kind: kind.clone(),
            bytes: command.clone(),
        });
        if state.slots.iter().any(|slot| slot.key == key) {
            drop(state);
            let failure =
                RunFailure::Divergence(ReplayDivergence::DuplicateKey { key: key.clone() });
            return self.refused_op(key, kind, command, failure);
        }
        let recorded = match &self.shared.mode {
            CxMode::Fresh => None,
            CxMode::Replay(journal) => match journal.entry(&key) {
                None => {
                    drop(state);
                    let failure = RunFailure::Divergence(ReplayDivergence::UnrecordedCommand {
                        key: key.clone(),
                        kind: kind.clone(),
                    });
                    return self.refused_op(key, kind, command, failure);
                }
                Some(entry) if entry.kind != kind || entry.command != command => {
                    let divergence = ReplayDivergence::CommandMismatch {
                        key: key.clone(),
                        recorded_kind: entry.kind.clone(),
                        issued_kind: kind.clone(),
                        difference: byte_difference(&entry.command, &command),
                    };
                    drop(state);
                    return self.refused_op(key, kind, command, RunFailure::Divergence(divergence));
                }
                Some(entry) => entry.outcome.clone(),
            },
        };
        let (status, hold, body) = match recorded {
            Some(outcome) => (SlotStatus::Settled(outcome), state.scheduler.hold(), None),
            None => {
                let body: Pin<Box<dyn Future<Output = T> + Send + 'a>> = Box::pin(body);
                (SlotStatus::Running, 0, Some(body))
            }
        };
        let index = state.slots.len();
        state.slots.push(Slot {
            key,
            kind,
            command,
            waker: None,
            hold,
            progressed: false,
            status,
        });
        Op {
            cx: self,
            index,
            body,
            step_waker: None,
            _output: PhantomData,
        }
    }

    fn refused_op<'a, T>(
        &'a self,
        key: String,
        kind: String,
        command: String,
        failure: RunFailure,
    ) -> Op<'a, T> {
        let mut state = self.shared.state.lock_recover();
        state.failure.get_or_insert(failure);
        let index = state.slots.len();
        state.slots.push(Slot {
            key,
            kind,
            command,
            waker: None,
            hold: 0,
            progressed: false,
            status: SlotStatus::Refused,
        });
        Op {
            cx: self,
            index,
            body: None,
            step_waker: None,
            _output: PhantomData,
        }
    }

    /// Record the bytes the drive commits. A drive's commit is part of what
    /// every run of it must repeat exactly.
    pub fn record_commit<C: Serialize + ?Sized>(&self, commit: &C) {
        let mut state = self.shared.state.lock_recover();
        match serde_json::to_string(commit) {
            Ok(bytes) => state
                .transcript
                .entries
                .push(TranscriptEntry::Commit { bytes }),
            Err(error) => {
                state.failure.get_or_insert(RunFailure::Codec {
                    key: "commit".to_string(),
                    message: error.to_string(),
                });
            }
        }
    }

    /// The replay keys the journal this context replays holds, in `[lower,
    /// upper]` compared bytewise.
    pub(super) fn recorded_keys_in(&self, lower: &str, upper: &str) -> Vec<String> {
        let CxMode::Replay(journal) = &self.shared.mode else {
            return Vec::new();
        };
        let mut keys = journal
            .entries
            .iter()
            .filter(|entry| lower <= entry.key.as_str() && entry.key.as_str() <= upper)
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    /// Drive `drive` to completion on this thread.
    ///
    /// The drive may be `!Send`. It is polled only here, and resumed only when
    /// one of its operations settles.
    pub fn run<F: Future<Output = ()>>(&self, drive: F) -> Result<RunRecord, RunFailure> {
        let root = Arc::new(RootWake::default());
        let waker = Waker::from(Arc::clone(&root));
        let mut drive = std::pin::pin!(drive);
        let mut round: u64 = 0;
        loop {
            round += 1;
            let polled = std::panic::catch_unwind(AssertUnwindSafe(|| {
                drive.as_mut().poll(&mut Context::from_waker(&waker))
            }));
            let ready = match polled {
                Ok(poll) => poll.is_ready(),
                Err(panic) => {
                    return Err(RunFailure::Panicked {
                        message: crate::panic_containment::payload_message(panic.as_ref()),
                    });
                }
            };
            if let Some(failure) = self.shared.state.lock_recover().failure.clone() {
                return Err(failure);
            }
            if root.foreign.load(Ordering::Acquire) {
                return Err(RunFailure::NonOpWake { round });
            }
            if ready {
                return Ok(self.finish());
            }
            self.release_next(round)?;
        }
    }

    /// Wait until at least one operation can be delivered to the drive, then
    /// wake the released operations in scheduling order.
    fn release_next(&self, round: u64) -> Result<(), RunFailure> {
        let mut state = self.shared.state.lock_recover();
        loop {
            let mut released = Vec::new();
            let mut held = false;
            let mut running = None;
            for (index, slot) in state.slots.iter_mut().enumerate() {
                match slot.status {
                    SlotStatus::Settled(_) if slot.waker.is_some() => {
                        if slot.hold > 0 {
                            slot.hold -= 1;
                            held = true;
                        } else {
                            released.push(index);
                        }
                    }
                    SlotStatus::Running if slot.waker.is_some() => {
                        if slot.progressed {
                            slot.progressed = false;
                            released.push(index);
                        } else if running.is_none() {
                            running = Some(index);
                        }
                    }
                    SlotStatus::Running => {}
                    _ => slot.progressed = false,
                }
            }
            if !released.is_empty() {
                state.scheduler.order(&mut released);
                let wakers = released
                    .into_iter()
                    .filter_map(|index| state.slots.get_mut(index)?.waker.take())
                    .collect::<Vec<_>>();
                drop(state);
                OPERATION_WAKE.with(|marker| marker.set(true));
                for waker in wakers {
                    waker.wake();
                }
                OPERATION_WAKE.with(|marker| marker.set(false));
                return Ok(());
            }
            if held {
                // A held operation counts down one round per drive poll.
                return Ok(());
            }
            let Some(running) = running else {
                return Err(RunFailure::NonOpAwait { round });
            };
            let (next, timeout) =
                self.shared
                    .progress
                    .wait_timeout_while(state, self.shared.body_timeout, |state| {
                        !state.slots.iter().any(|slot| {
                            slot.progressed && matches!(slot.status, SlotStatus::Running)
                        })
                    })
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                let key = state
                    .slots
                    .get(running)
                    .map(|slot| slot.key.clone())
                    .unwrap_or_default();
                return Err(RunFailure::BodyTimeout { key });
            }
        }
    }

    fn finish(&self) -> RunRecord {
        let mut state = self.shared.state.lock_recover();
        let transcript = std::mem::take(&mut state.transcript);
        let journal = match &self.shared.mode {
            CxMode::Replay(journal) => journal.as_ref().clone(),
            CxMode::Fresh => DriveJournal {
                entries: state
                    .slots
                    .iter()
                    .filter(|slot| !matches!(slot.status, SlotStatus::Refused))
                    .map(|slot| JournalEntry {
                        key: slot.key.clone(),
                        kind: slot.kind.clone(),
                        command: slot.command.clone(),
                        outcome: match &slot.status {
                            SlotStatus::Settled(outcome) | SlotStatus::Delivered(outcome) => {
                                Some(outcome.clone())
                            }
                            SlotStatus::Running | SlotStatus::Dropped | SlotStatus::Refused => None,
                        },
                    })
                    .collect(),
            },
        };
        RunRecord {
            transcript,
            journal,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock_recover()
    }
}

/// The drive's own waker. Every wake that is not an operation's is recorded
/// as foreign and fails the run.
#[derive(Default)]
struct RootWake {
    foreign: AtomicBool,
}

impl Wake for RootWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !OPERATION_WAKE.with(Cell::get) {
            self.foreign.store(true, Ordering::Release);
        }
    }
}

/// The waker an operation body is polled with. It never reaches the drive: it
/// marks the body as progressed and wakes the executor.
struct StepWake {
    shared: Arc<Shared>,
    index: usize,
}

impl Wake for StepWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let mut state = self.shared.state.lock_recover();
        if let Some(slot) = state.slots.get_mut(self.index) {
            slot.progressed = true;
        }
        drop(state);
        self.shared.progress.notify_all();
    }
}

/// A recorded operation's future. It resolves when the executor delivers the
/// operation's outcome.
pub struct Op<'a, T> {
    cx: &'a LocalTestCx,
    index: usize,
    body: Option<Pin<Box<dyn Future<Output = T> + Send + 'a>>>,
    step_waker: Option<Waker>,
    _output: PhantomData<fn() -> T>,
}

impl<T> Future for Op<'_, T>
where
    T: Serialize + DeserializeOwned,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if let Some(body) = this.body.as_mut() {
            let shared = &this.cx.shared;
            let step = this.step_waker.get_or_insert_with(|| {
                Waker::from(Arc::new(StepWake {
                    shared: Arc::clone(shared),
                    index: this.index,
                }))
            });
            let polled = {
                let _runtime = shared.runtime.enter();
                body.as_mut().poll(&mut Context::from_waker(step))
            };
            if let Poll::Ready(value) = polled {
                this.body = None;
                let encoded = serde_json::to_string(&value);
                let mut state = this.cx.state();
                let hold = state.scheduler.hold();
                let State { slots, failure, .. } = &mut *state;
                if let Some(slot) = slots.get_mut(this.index) {
                    match encoded {
                        Ok(outcome) => {
                            slot.status = SlotStatus::Settled(outcome);
                            slot.hold = hold;
                        }
                        Err(error) => {
                            failure.get_or_insert(RunFailure::Codec {
                                key: slot.key.clone(),
                                message: error.to_string(),
                            });
                        }
                    }
                }
            }
        }
        let mut state = this.cx.state();
        if state.failure.is_some() {
            return Poll::Pending;
        }
        let State { slots, failure, .. } = &mut *state;
        let Some(slot) = slots.get_mut(this.index) else {
            return Poll::Pending;
        };
        let deliverable = match &slot.status {
            SlotStatus::Settled(outcome) if slot.hold == 0 => Some(outcome.clone()),
            _ => None,
        };
        let Some(outcome) = deliverable else {
            slot.waker = Some(context.waker().clone());
            return Poll::Pending;
        };
        match serde_json::from_str::<T>(&outcome) {
            Ok(value) => {
                slot.status = SlotStatus::Delivered(outcome);
                slot.waker = None;
                Poll::Ready(value)
            }
            Err(error) => {
                failure.get_or_insert(RunFailure::Codec {
                    key: slot.key.clone(),
                    message: error.to_string(),
                });
                Poll::Pending
            }
        }
    }
}

impl<T> Drop for Op<'_, T> {
    fn drop(&mut self) {
        let mut state = self.cx.state();
        if let Some(slot) = state.slots.get_mut(self.index)
            && matches!(slot.status, SlotStatus::Running | SlotStatus::Settled(_))
        {
            // A dropped fresh body never settles; a dropped settled outcome
            // keeps its journal entry, as an engine keeps a loser's entry.
            if matches!(slot.status, SlotStatus::Running) {
                slot.status = SlotStatus::Dropped;
            }
            slot.waker = None;
        }
    }
}
