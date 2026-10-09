//! Process observation: the snapshot, cursor, stream events and bounded live
//! replay of one process, apart from session live replay (D-PROCOBS).
//!
//! A process is observed as a session is (ADR 0002): a durable snapshot with
//! a cursor bound to its revision, then a bounded replay of what was
//! published after it, with typed gaps where the replay cannot continue. The
//! revision is the process's durable event sequence ([`ProcessSequence`]).
//! The stream carries provisional language execution and each committed
//! lifecycle fact; node history is never durable, so what a gap loses of it
//! stays lost.
//!
//! Process and session replay share no store, window or budget: a process
//! burst cannot evict a session's window, by construction.

use crate::ProcessId;
use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
pub use lash_trace::{LanguageExecutionObservation, StepBodyStartedObservation};

use super::super::{
    ObservedProcess, ObservedProcessEvent, ProcessEffectReport, RetiredProcessStatus,
};

#[path = "process_replay/memory.rs"]
mod memory;
#[cfg(test)]
use memory::memory_window_bytes;
pub use memory::{InMemoryProcessReplayStore, InMemoryProcessReplayStoreConfig};

const PROCESS_OBSERVATION_CURSOR_PREFIX: &str = "lashpo1:";

/// A process's durable event sequence: the revision its observation cursor
/// binds to. Zero is a registered process with no event yet.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ProcessSequence(pub u64);

impl ProcessSequence {
    pub fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Where an observer of one process stands: a replay-store incarnation, the
/// process, the durable sequence the observer holds and a live position.
///
/// The token is opaque to ordinary consumers and carries no lexical order.
/// Positions are exclusive and comparable only within one incarnation.
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ProcessObservationCursor(String);

impl ProcessObservationCursor {
    pub fn new(
        replay_incarnation_id: impl AsRef<str>,
        process_id: &ProcessId,
        sequence: ProcessSequence,
        live_position: u64,
    ) -> Self {
        Self(format!(
            "{PROCESS_OBSERVATION_CURSOR_PREFIX}{}:{}:{live_position}:{}",
            replay_incarnation_id.as_ref(),
            sequence.0,
            process_id
        ))
    }

    /// Validate and adopt a cursor token a custom process replay store
    /// persisted.
    ///
    /// Integrator class (ADR 0051): **custom process-replay store implementors**.
    pub fn from_store_token(
        token: impl Into<String>,
    ) -> Result<Self, ProcessObservationCursorError> {
        let cursor = Self(token.into());
        cursor.parse()?;
        Ok(cursor)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse the cursor, refusing one that names another process. A cursor
    /// for the wrong process is a request error, never a retargeted
    /// snapshot.
    pub fn parse_for_process(
        &self,
        expected_process_id: &ProcessId,
    ) -> Result<ParsedProcessObservationCursor<'_>, ProcessObservationCursorError> {
        let parsed = self.parse()?;
        if parsed.process_id != *expected_process_id {
            return Err(ProcessObservationCursorError::WrongProcess {
                expected_process_id: expected_process_id.clone(),
                actual_process_id: parsed.process_id,
            });
        }
        Ok(parsed)
    }

    /// Read the incarnation, process, sequence and live position this cursor
    /// names. A store learns the process a cursor addresses here.
    ///
    /// Integrator class (ADR 0051): **custom process-replay store implementors**.
    pub fn parse(
        &self,
    ) -> Result<ParsedProcessObservationCursor<'_>, ProcessObservationCursorError> {
        let malformed = |message: &str| ProcessObservationCursorError::Malformed {
            message: message.to_string(),
        };
        let payload = self
            .0
            .strip_prefix(PROCESS_OBSERVATION_CURSOR_PREFIX)
            .ok_or_else(|| malformed("missing cursor prefix"))?;
        let mut parts = payload.splitn(4, ':');
        let replay_incarnation_id = parts
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| malformed("missing replay incarnation id"))?;
        let mut number = |name: &str| {
            parts
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| malformed(&format!("missing or invalid {name}")))
        };
        let sequence = number("process sequence")?;
        let live_position = number("live replay position")?;
        let process_id = parts
            .next()
            .and_then(|value| ProcessId::parse(value).ok())
            .ok_or_else(|| malformed("missing process id"))?;
        Ok(ParsedProcessObservationCursor {
            replay_incarnation_id,
            process_id,
            sequence: ProcessSequence(sequence),
            live_position,
        })
    }
}

impl fmt::Debug for ProcessObservationCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProcessObservationCursor(<opaque>)")
    }
}

impl fmt::Display for ProcessObservationCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub struct ParsedProcessObservationCursor<'a> {
    pub replay_incarnation_id: &'a str,
    pub process_id: ProcessId,
    pub sequence: ProcessSequence,
    pub live_position: u64,
}

#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessObservationCursorError {
    #[error("malformed process observation cursor: {message}")]
    Malformed { message: String },
    #[error(
        "process observation cursor belongs to `{actual_process_id}`, not `{expected_process_id}`"
    )]
    WrongProcess {
        expected_process_id: ProcessId,
        actual_process_id: ProcessId,
    },
}

/// A process's durable state at one sequence, with the cursor a feed
/// continues from.
#[derive(Clone, Debug)]
pub struct ProcessObservation {
    pub read_view: ProcessReadView,
    pub cursor: ProcessObservationCursor,
}

/// What the durable store holds for a process id. A pruned process and an id
/// no row or tombstone names are different answers, and neither is a new
/// process at sequence zero.
#[derive(Clone, Debug)]
pub enum ProcessReadView {
    Retained(Box<RetainedProcessView>),
    /// The process was pruned: its tombstone remains.
    Retired {
        terminal_label: RetiredProcessStatus,
        pruned_at_ms: u64,
    },
    /// No retained process or tombstone has this id.
    Unknown,
}

impl ProcessReadView {
    /// The durable sequence this view stands at; zero when no process is
    /// retained.
    pub fn sequence(&self) -> ProcessSequence {
        match self {
            Self::Retained(view) => ProcessSequence(view.process.last_event_sequence),
            Self::Retired { .. } | Self::Unknown => ProcessSequence(0),
        }
    }
}

/// One retained process at `process.last_event_sequence`: its row, the
/// effect evidence folded through that sequence and the document it runs.
/// Appends after it belong to a later view.
#[derive(Clone, Debug)]
pub struct RetainedProcessView {
    pub process: ObservedProcess,
    pub effects: ProcessEffectEvidence,
    pub document: ProcessDocumentIdentity,
}

/// Which workflow document a process runs: a reference, never the graph. A
/// host reads the graph by it and joins provisional node evidence only
/// within the document and execution it names. It is independent of the
/// lifecycle and of effect coverage: a process whose artifact was released
/// is still observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessDocumentIdentity {
    Available(lash_trace::WorkflowDocumentRef),
    /// Nothing retains an artifact the definition reads.
    ArtifactUnavailable {
        artifact: crate::ArtifactName,
    },
    /// The process's engine has no workflow document.
    Unsupported,
}

/// The bounded effect evidence of a process: retained occurrences and
/// omission counts, never "every call the process made". While a process
/// runs, absent omission totals do not prove nothing was omitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEffectEvidence {
    pub report: ProcessEffectReport,
    /// The last sequence the fold read. It is the view's sequence when
    /// `coverage` is complete or names a released prefix.
    pub observed_through: ProcessSequence,
    pub coverage: ProcessEffectCoverage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessEffectCoverage {
    Complete,
    Incomplete { reason: ProcessEffectGapReason },
}

/// Why an effect fold does not cover every event through its view's
/// sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEffectGapReason {
    /// The acquisition read its page budget before reaching the sequence.
    AcquisitionBudgetExhausted,
    /// An effect fact could not be decoded and is missing from the report.
    Undecodable,
    /// The host released a prefix of the history: the report folds only the
    /// events after it.
    HistoryReleased,
}

/// One published event of a process's observation stream.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ProcessObservationEvent {
    pub cursor: ProcessObservationCursor,
    pub payload: ProcessObservationEventPayload,
}

impl ProcessObservationEvent {
    /// Construct an event from a store's cursor, validated first so the
    /// identity accessors parse it infallibly.
    ///
    /// Integrator class (ADR 0051): **custom process-replay store implementors**.
    pub fn new(
        cursor: ProcessObservationCursor,
        payload: ProcessObservationEventPayload,
    ) -> Result<Self, ProcessObservationCursorError> {
        cursor.parse()?;
        Ok(Self { cursor, payload })
    }

    #[expect(clippy::expect_used, reason = "the constructor validated the cursor")]
    fn parsed(&self) -> ParsedProcessObservationCursor<'_> {
        self.cursor
            .parse()
            .expect("a process observation event's cursor parses")
    }

    pub fn process_id(&self) -> ProcessId {
        self.parsed().process_id
    }

    pub fn replay_incarnation_id(&self) -> &str {
        self.parsed().replay_incarnation_id
    }

    /// The durable sequence the event was published at: a commit's own
    /// sequence, or the sequence its publisher knew for a provisional
    /// observation.
    pub fn sequence(&self) -> ProcessSequence {
        self.parsed().sequence
    }

    pub fn live_position(&self) -> u64 {
        self.parsed().live_position
    }
}

#[derive(Clone, Debug, PartialEq)]
// justification: the enclosing replay event is already Arc-owned, so another allocation would not bound retained event storage.
#[allow(clippy::large_enum_variant)]
pub enum ProcessObservationEventPayload {
    /// Provisional evidence of what a language execution reported. It never
    /// proves a durable advance, and an `ExecutionFinished` in it does not
    /// settle the process: only a committed terminal fact or a terminal
    /// snapshot does.
    LanguageExecution(LanguageExecutionObservation),
    /// Provisional evidence that the admitted body of one of the process's
    /// steps started: it binds a site occurrence of the process's workflow
    /// document to the call the admission pinned. A refused step never
    /// produces one; a retried body produces one more, for its next attempt.
    StepBodyStarted(StepBodyStartedObservation),
    /// One committed lifecycle fact. It extends the process at
    /// `event.sequence - 1`: a consumer holding that sequence applies the
    /// fact, one holding `event.sequence` or later already has it, and any
    /// other consumer reads the durable process again.
    Committed { event: ObservedProcessEvent },
}

impl ProcessObservationEventPayload {
    /// The identity a redelivery of this payload repeats.
    pub fn identity(&self) -> ProcessObservationIdentity {
        match self {
            Self::LanguageExecution(observation) => ProcessObservationIdentity::LanguageExecution {
                event_key: observation.execution.event_key.clone(),
            },
            Self::StepBodyStarted(observation) => ProcessObservationIdentity::StepBodyStarted {
                event_key: observation.step.event_key(),
            },
            Self::Committed { event } => ProcessObservationIdentity::Committed {
                sequence: ProcessSequence(event.sequence),
            },
        }
    }

    /// Whether `other` states the same fact as this payload. Observation
    /// time is not part of a provisional fact.
    pub fn same_fact(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::LanguageExecution(left), Self::LanguageExecution(right)) => {
                left.same_fact(right)
            }
            (Self::StepBodyStarted(left), Self::StepBodyStarted(right)) => left.step == right.step,
            (Self::Committed { event: left }, Self::Committed { event: right }) => left == right,
            _ => false,
        }
    }
}

/// What makes two publications to one process the same observation,
/// whichever replay incarnation or position carried each. A committed fact
/// is its sequence; a provisional observation is its producer's event key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessObservationIdentity {
    Committed { sequence: ProcessSequence },
    LanguageExecution { event_key: String },
    StepBodyStarted { event_key: String },
}

impl fmt::Display for ProcessObservationIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Committed { sequence } => write!(f, "committed sequence {}", sequence.0),
            Self::LanguageExecution { event_key } => {
                write!(f, "language execution `{event_key}`")
            }
            Self::StepBodyStarted { event_key } => write!(f, "step body start `{event_key}`"),
        }
    }
}

/// Why a process replay store cannot continue from a cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessReplayGapReason {
    /// Retention dropped events after the cursor.
    Trimmed,
    /// The cursor names another incarnation, a position past the tail, or
    /// continuity that was invalidated.
    Unavailable,
}

/// Why a process feed replaced its consumer's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessObservationGapCause {
    /// The replay store could not continue from the cursor.
    Replay { reason: ProcessReplayGapReason },
    /// The cursor names a sequence past the durable process.
    AheadOfDurableProcess,
    /// The replay holds no committed fact for some sequence between the one
    /// the consumer holds and the durable process's.
    CommitUnbridged,
    /// No process is retained under this id: the replacement says whether it
    /// was pruned or is unknown, and the feed ends.
    NotRetained,
}

/// A break in a process feed. The replacement snapshot that travels with it
/// is authoritative: the consumer replaces its durable state, discards its
/// provisional state and folds what the feed replays from `latest_cursor`.
/// Node events the replay no longer holds are not restored.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProcessReplayGap {
    pub process_id: ProcessId,
    pub requested_cursor: ProcessObservationCursor,
    pub latest_cursor: ProcessObservationCursor,
    pub latest_sequence: ProcessSequence,
    pub cause: ProcessObservationGapCause,
}

#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessReplayStoreError {
    #[error("{0}")]
    Cursor(#[from] ProcessObservationCursorError),
    /// A publication repeated an identity the process's window holds with a
    /// different fact. Nothing of its batch was published and the process's
    /// continuity was invalidated.
    #[error("process `{process_id}` republished {identity} with a different fact")]
    ConflictingRedelivery {
        process_id: ProcessId,
        identity: ProcessObservationIdentity,
    },
    #[error("process replay store error: {0}")]
    Store(String),
    #[error("process replay subscriber lagged by {0} events")]
    SubscriberLagged(u64),
    #[error("process replay channel closed")]
    Closed,
}

#[derive(Clone, Debug)]
pub enum ProcessReplayOutcome {
    Replayed(Vec<Arc<ProcessObservationEvent>>),
    Gap(ProcessReplayGapReason),
}

pub enum ProcessReplaySubscribeOutcome {
    Subscribed(ProcessReplaySubscription),
    Gap(ProcessReplayGapReason),
}

/// One event handed to [`ProcessReplayStore::publish`], before the store
/// assigns its position.
#[derive(Clone, Debug)]
pub struct ProcessReplayEventDraft {
    sequence: ProcessSequence,
    payload: ProcessObservationEventPayload,
}

impl ProcessReplayEventDraft {
    /// A provisional observation, stamped with the durable sequence its
    /// publisher knows. A stamp behind the process's sequence is legal: it
    /// moves a consumer's position, never its durable state.
    pub fn language_execution(
        sequence: ProcessSequence,
        observation: LanguageExecutionObservation,
    ) -> Self {
        Self {
            sequence,
            payload: ProcessObservationEventPayload::LanguageExecution(observation),
        }
    }

    /// The start of an admitted step body, stamped as a provisional
    /// observation is.
    pub fn step_body_started(
        sequence: ProcessSequence,
        observation: StepBodyStartedObservation,
    ) -> Self {
        Self {
            sequence,
            payload: ProcessObservationEventPayload::StepBodyStarted(observation),
        }
    }

    /// A committed fact, published after its commit at its own sequence.
    pub fn committed(event: ObservedProcessEvent) -> Self {
        Self {
            sequence: ProcessSequence(event.sequence),
            payload: ProcessObservationEventPayload::Committed { event },
        }
    }

    pub fn sequence(&self) -> ProcessSequence {
        self.sequence
    }

    pub fn payload(&self) -> &ProcessObservationEventPayload {
        &self.payload
    }

    pub fn into_payload(self) -> ProcessObservationEventPayload {
        self.payload
    }
}

/// One item of a process replay subscription's tail.
type ProcessReplayItem = Result<Arc<ProcessObservationEvent>, ProcessReplayStoreError>;

/// A process replay subscription: the retained events after the subscribed
/// cursor, then the store's live tail.
///
/// A store hands every retained event it replays in `replay`, not in `live`:
/// the feed reads the replayed prefix to judge whether it bridges a stale
/// cursor to the durable process. The live tail ends a lagging subscriber
/// with [`ProcessReplayStoreError::SubscriberLagged`] and a closed one with
/// [`ProcessReplayStoreError::Closed`]; the feed then resubscribes from its
/// cursor.
///
/// Integrator class (ADR 0051): **custom process-replay store implementors**.
pub struct ProcessReplaySubscription {
    replay: VecDeque<Arc<ProcessObservationEvent>>,
    live: Pin<Box<dyn Stream<Item = ProcessReplayItem> + Send>>,
}

impl ProcessReplaySubscription {
    /// A subscription that yields `replay` in order, then `live`.
    pub fn new(
        replay: Vec<Arc<ProcessObservationEvent>>,
        live: impl Stream<Item = ProcessReplayItem> + Send + 'static,
    ) -> Self {
        Self {
            replay: replay.into(),
            live: Box::pin(live),
        }
    }

    /// Whether the replayed prefix holds the committed fact of every
    /// sequence after `held` through `durable`.
    pub fn bridges(&self, held: ProcessSequence, durable: ProcessSequence) -> bool {
        commits_bridge(self.replay.iter().map(Arc::as_ref), held, durable)
    }
}

/// Whether `events`, in position order, hold the committed fact of every
/// sequence after `held` through `durable`. Unlike a session's bridge, one
/// later commit is not enough: each fact is a delta the consumer applies.
pub fn commits_bridge<'a>(
    events: impl IntoIterator<Item = &'a ProcessObservationEvent>,
    held: ProcessSequence,
    durable: ProcessSequence,
) -> bool {
    let mut next = held.0;
    for event in events {
        if next >= durable.0 {
            break;
        }
        if let ProcessObservationEventPayload::Committed { event } = &event.payload
            && event.sequence == next + 1
        {
            next += 1;
        }
    }
    next >= durable.0
}

impl fmt::Debug for ProcessReplaySubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessReplaySubscription")
            .field("replayed", &self.replay.len())
            .finish_non_exhaustive()
    }
}

impl Stream for ProcessReplaySubscription {
    type Item = ProcessReplayItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.replay.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
        self.live.as_mut().poll_next(cx)
    }
}

/// Bounded, best-effort live replay of process observation: the tail of
/// every process feed.
///
/// [`InMemoryProcessReplayStore`] is the default and holds what one OS
/// process published. Cores that share one store share every publication; a
/// store shared across OS processes is how provisional node history crosses
/// replicas. A feed's snapshot is the durable process either way, so the
/// store decides which provisional evidence an observer sees, never the
/// snapshot's consistency.
///
/// # Obligations
///
/// The laws in `lash-conformance` (`process_replay_tests!`) certify these;
/// the first group is the session store's contract
/// ([`LiveReplayStore`](super::LiveReplayStore)), kept rule for rule.
///
/// - **One position universe per process, one total order across
///   writers.** Every event published for a process, by any writer, takes
///   its position from one sequence. A batch's events are contiguous and in
///   batch order.
/// - **Incarnation is the publisher epoch.** A cursor naming another
///   incarnation answers [`ProcessReplayGapReason::Unavailable`], never an
///   empty replay. An incarnation survives a restart only with the history
///   behind it.
/// - **Positions never repeat** across retention, eviction and recreation
///   of a process's window.
/// - **Replay and subscription are exact and linearizable.** Both yield
///   exactly the process's retained events past the cursor's position, in
///   position order, each once, and never another process's. A
///   subscription yields its replayed prefix before any live event, with
///   nothing lost or repeated across that boundary.
/// - **The window is bounded, and its gaps are typed.** A position
///   retention dropped answers [`ProcessReplayGapReason::Trimmed`]; one past
///   the tail, or before an invalidation, answers
///   [`ProcessReplayGapReason::Unavailable`].
/// - **Lag means resubscribe**, and **invalidation reaches every
///   subscriber**: every existing cursor answers `Unavailable` and every
///   live subscription to the process closes.
/// - **Current cursors stay behind newer sequences.** A cursor at sequence
///   `N` sits before every event stamped past `N` that the store holds or
///   will hold. Stamps need not grow with position.
/// - **A redelivery is published once, and a conflicting one is never
///   applied.** A draft whose [`ProcessObservationIdentity`] the process's
///   window already holds with the same fact is dropped. One that holds it
///   with a different fact fails the batch with
///   [`ProcessReplayStoreError::ConflictingRedelivery`] and invalidates the
///   process's continuity.
/// - **A refused publication is a gap, never a silent loss.** A batch the
///   store cannot hold takes no position and invalidates the process's
///   continuity, so no cursor replays cleanly across it.
#[async_trait::async_trait]
pub trait ProcessReplayStore: Send + Sync {
    /// Assign `events` the process's next positions, in order, make them
    /// replay-visible, notify subscribers in position order, and answer the
    /// published events. Redeliveries are dropped, so the answer may be
    /// shorter than `events`, or empty.
    async fn publish(
        &self,
        process_id: &ProcessId,
        events: Vec<ProcessReplayEventDraft>,
    ) -> Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError>;

    /// The process's retained events after `cursor`, or the gap that
    /// prevents continuing from it.
    async fn replay_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplayOutcome, ProcessReplayStoreError>;

    /// Subscribe after `cursor`, replaying retained events before live
    /// events, or answer the gap that prevents continuing from it.
    async fn subscribe_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplaySubscribeOutcome, ProcessReplayStoreError>;

    /// The cursor after everything the store holds for the process that is
    /// stamped at or before `sequence`: a snapshot at `sequence` that raced
    /// a newer commit still replays that commit.
    async fn current_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError>;

    /// The cursor every event the store still retains for the process comes
    /// after. A snapshot attaches here, so an observer that arrives late or
    /// recovers from a gap folds the whole retained window.
    async fn earliest_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError>;

    /// Mark this process's replay continuity unavailable: existing cursors
    /// answer `Gap(Unavailable)` and live subscriptions close. A cursor
    /// acquired afterwards establishes fresh continuity.
    async fn invalidate_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), ProcessReplayStoreError>;

    /// Mark every process's replay continuity unavailable: every existing
    /// cursor of every process answers `Gap(Unavailable)` and every live
    /// subscription closes. A publisher whose bounded ingress overflowed
    /// calls this when it can no longer name the processes it lost
    /// observations of. A store may rotate its incarnation to do it.
    async fn invalidate_all(&self) -> Result<(), ProcessReplayStoreError>;

    /// Apply retention to the process's window.
    async fn trim_process(&self, process_id: &ProcessId) -> Result<(), ProcessReplayStoreError>;
}

#[cfg(test)]
#[path = "process_replay/tests.rs"]
mod tests;
