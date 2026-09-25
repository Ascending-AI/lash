//! The Restate server double.
//!
//! [`RestateTestServer`] plays `restate-server` for one endpoint: its ingress
//! and admin APIs are an in-process [`HttpTransport`], its invoker drives the
//! endpoint's real `Endpoint::handle` with protocol streams, and its partition
//! processor keeps journals, keys, promises and timers in memory. Time is
//! virtual; ids, random seeds, timer tie-breaks and random crashes come from
//! one seed.

mod attempt;
mod body;
pub mod catalog;
mod commands;
mod crash;
mod ids;
mod ingress;
mod model;
mod processor;
mod query;
mod serial;
mod timers;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use lash_http_transport::HttpTransport;
use restate_sdk::endpoint::Endpoint;
use tokio::sync::Notify;

pub use catalog::{HandlerKind, OnMaxAttempts, ServiceKind};
pub use crash::{CrashPoint, CrashRule, RandomCrashes};
pub use ids::InvocationId;
pub use model::TimerView;
pub use processor::{RetryPolicy, Stats};

use catalog::{Catalog, HandlerSpec};
use model::{InvKey, Status};
use processor::{ControlResult, Flow, State};

use crate::protocol::{Frame, MessageType, ProtocolVersion};

/// How virtual time moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeMode {
    /// Only [`RestateTestServer::advance`] and friends move time: a sim
    /// clock or the test decides when every timer fires, retry backoffs
    /// included.
    Manual,
    /// When the server has been quiescent — every live attempt blocked on
    /// the server — for `idle` of wall time, it moves on by itself:
    ///
    /// * a pending invoker retry starts at once, without moving time (its
    ///   backoff is compressed; retry timing is no contract);
    /// * otherwise the next timer fires, moving time to it, if it is due
    ///   within `horizon` of virtual now.
    ///
    /// Between moves virtual time also flows at wall speed, so a longer
    /// timer (a minute-long sleep, a deadline) fires when a real server
    /// would, or earlier on an explicit advance — work outside the server,
    /// a test about to resolve or cancel something, is never overtaken by a
    /// timer a real server would not fire yet.
    AutoAdvance { idle: Duration, horizon: Duration },
}

impl TimeMode {
    /// [`TimeMode::AutoAdvance`] with a 5 ms idle grace and a one-second
    /// horizon.
    pub const fn auto() -> Self {
        Self::AutoAdvance {
            idle: Duration::from_millis(5),
            horizon: Duration::from_secs(1),
        }
    }
}

/// How the server runs attempts that are live at the same time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scheduling {
    /// Every live attempt runs whenever Tokio polls it, as against a real
    /// server: lash's concurrent handlers race.
    #[default]
    Concurrent,
    /// One attempt runs at a time and the turn passes in a seeded order, so
    /// a scenario's attempts interleave the same way on every run of one
    /// seed. See the `serial` module docs for what it orders and what it
    /// cannot.
    Serial,
}

/// The server's configuration. [`Default`] is what production runs:
/// protocol V6, streaming attempts, Restate 1.7's default retry policy.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The seed of every id, random seed, timer tie-break and random crash.
    pub seed: u64,
    pub protocol: ProtocolVersion,
    /// Close each attempt's input right after the replayed journal, so the
    /// handler suspends at every await the journal cannot answer and every
    /// step replays — the `INACTIVITY_TIMEOUT=0s` mode.
    pub always_replay: bool,
    pub time: TimeMode,
    pub scheduling: Scheduling,
    /// Virtual epoch milliseconds the server starts at.
    pub start_time_ms: u64,
    /// Virtual idle time after which the invoker closes a starved stream.
    pub inactivity_timeout: Duration,
    /// The invoker retry policy handlers inherit unless their deployment
    /// overrides it.
    pub retry: RetryPolicy,
    /// The base URL clients address; any value works, nothing listens.
    pub ingress_url: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            protocol: ProtocolVersion::V6,
            always_replay: false,
            time: TimeMode::auto(),
            scheduling: Scheduling::Concurrent,
            start_time_ms: 1_800_000_000_000,
            inactivity_timeout: Duration::from_secs(60),
            retry: RetryPolicy {
                initial_interval: Duration::from_millis(500),
                exponentiation_factor: 2.0,
                max_interval: Duration::from_secs(60),
                max_attempts: Some(70),
                on_max_attempts: OnMaxAttempts::Pause,
            },
            ingress_url: "http://restate.test".to_owned(),
        }
    }
}

impl ServerConfig {
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub fn always_replay(mut self, always_replay: bool) -> Self {
        self.always_replay = always_replay;
        self
    }

    pub fn time(mut self, time: TimeMode) -> Self {
        self.time = time;
        self
    }

    pub fn scheduling(mut self, scheduling: Scheduling) -> Self {
        self.scheduling = scheduling;
        self
    }

    pub fn protocol(mut self, protocol: ProtocolVersion) -> Self {
        self.protocol = protocol;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// `spec`'s effective retry policy: its deployment overrides over this
    /// server's defaults.
    fn retry_policy(&self, spec: &HandlerSpec) -> RetryPolicy {
        let overrides = spec.retry;
        RetryPolicy {
            initial_interval: overrides
                .initial_interval_ms
                .map_or(self.retry.initial_interval, Duration::from_millis),
            exponentiation_factor: overrides
                .exponentiation_factor
                .unwrap_or(self.retry.exponentiation_factor),
            max_interval: overrides
                .max_interval_ms
                .map_or(self.retry.max_interval, Duration::from_millis),
            max_attempts: overrides.max_attempts.or(self.retry.max_attempts),
            on_max_attempts: overrides
                .on_max_attempts
                .unwrap_or(self.retry.on_max_attempts),
        }
    }
}

/// The registered deployment: the endpoint and what it serves.
struct Deployment {
    endpoint: Endpoint,
    catalog: Catalog,
}

pub(crate) struct Shared {
    deployment: OnceLock<Deployment>,
    config: ServerConfig,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
    /// Pulsed whenever an attempt starts, blocks, or ends, or time moves.
    activity: Arc<Notify>,
    /// Serial scheduling: pulsed whenever the turn is granted. A handler
    /// resuming from its own ingress request waits on this rather than on
    /// `activity`, which its own park pulses.
    granted: Notify,
    /// Told the new virtual time whenever it moves, so clocks the handlers
    /// read (a store set's) move with it.
    time_listener: OnceLock<Arc<dyn Fn(u64) + Send + Sync>>,
    /// The server's tasks still alive: attempts and time and turn drivers.
    tasks: Arc<AtomicUsize>,
}

/// What woke an attempt that waits for the serial turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    /// The server: a request of its handler's answered.
    Server,
    /// Something outside the server: the attempt has a frame to write.
    Outside,
}

/// An outside ingress request waiting to land, or landing: once admitted,
/// the request has landed when this drops.
struct Landing {
    shared: Arc<Shared>,
    admitted: tokio::sync::oneshot::Receiver<()>,
}

impl Drop for Landing {
    fn drop(&mut self) {
        // Admitted (or its admission dropped by a shut server): the next
        // request may go.
        if !matches!(
            self.admitted.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            self.shared.lock().landed();
            self.shared.activity.notify_waiters();
        }
    }
}

/// A test's declaration that a handler waits, inside a `ctx.run` closure,
/// on something outside the server that the test drives — a scripted
/// provider's next event, a task the handler spawned whose own requests
/// must land first.
///
/// Under [`Scheduling::Serial`] a holder parked inside a `ctx.run` closure
/// keeps the turn: the server cannot tell a closure waiting on work that
/// finishes by itself (a store call) from one waiting on the test. While
/// gates are [entered](Self::enter), attempts parked inside `ctx.run`
/// closures with no request of their own in flight — no more of them than
/// there are entered gates — are taken to wait on those gates: the turn
/// moves on and outside work goes on between turns, at the same point on
/// every run. A weak handle: it does not keep the server alive.
#[derive(Clone, Debug)]
pub struct OutsideGates {
    shared: Weak<Shared>,
}

impl OutsideGates {
    /// A handler is about to wait on a gate; it is held until the guard
    /// drops. Enter it only once the gate is known closed, so the wait
    /// really parks.
    pub fn enter(&self) -> OutsideGate {
        if let Some(shared) = self.shared.upgrade() {
            shared.lock().gate_entered();
            shared.activity.notify_waiters();
        }
        OutsideGate {
            shared: self.shared.clone(),
        }
    }
}

/// A gate a handler waits on, entered through [`OutsideGates::enter`];
/// dropping it says the wait is over.
#[derive(Debug)]
pub struct OutsideGate {
    shared: Weak<Shared>,
}

impl Drop for OutsideGate {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.lock().gate_left();
            shared.activity.notify_waiters();
        }
    }
}

/// Counts one server task while it lives.
pub(crate) struct TaskGuard(Arc<AtomicUsize>);

impl TaskGuard {
    fn new(tasks: &Arc<AtomicUsize>) -> Self {
        tasks.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(tasks))
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    /// Spawn a server task, counted while it lives.
    pub(crate) fn spawn<F>(&self, task: F) -> tokio::task::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let guard = TaskGuard::new(&self.tasks);
        self.runtime.spawn(async move {
            let _guard = guard;
            task.await;
        })
    }
}

impl Shared {
    fn time_moved(&self, now_ms: u64) {
        if let Some(listener) = self.time_listener.get() {
            listener(now_ms);
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the registered deployment serves; empty before registration, so
    /// every target is "not found" as on a server with no deployment.
    fn catalog(&self) -> &Catalog {
        static EMPTY: OnceLock<Catalog> = OnceLock::new();
        self.deployment.get().map_or_else(
            || EMPTY.get_or_init(Catalog::default),
            |deployment| &deployment.catalog,
        )
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.deployment.get().map(|deployment| &deployment.endpoint)
    }

    fn invocation_id(&self, key: InvKey) -> String {
        self.lock().invocations[key.0].id.as_str().to_owned()
    }

    fn on_frame(
        self: &Arc<Self>,
        key: InvKey,
        number: u32,
        frame: Frame,
        received_us: u128,
    ) -> Flow {
        let mut state = self.lock();
        let flow = state.on_frame(self, key, number, frame, received_us);
        let granted = state.schedule();
        drop(state);
        if granted {
            self.turn_granted();
        }
        flow
    }

    fn stream_ended(self: &Arc<Self>, key: InvKey, number: u32, detail: String) {
        let mut state = self.lock();
        state.stream_ended(self, key, number, detail);
        let granted = state.schedule();
        drop(state);
        if granted {
            self.turn_granted();
        }
    }

    /// An attempt's handler parked: move the serial turn if that frees it.
    fn parked(&self) {
        if self.config.scheduling == Scheduling::Serial && self.lock().schedule() {
            self.granted.notify_waiters();
        }
        self.activity.notify_waiters();
    }

    /// Serial scheduling: the turn moved to an attempt.
    fn turn_granted(&self) {
        self.granted.notify_waiters();
        self.activity.notify_waiters();
    }

    /// Serial scheduling: an ingress request issued by the handler of
    /// `turn` is in flight; the turn may move on.
    fn ingress_began(&self, turn: serial::Turn) -> u64 {
        let ticket = self.lock().ingress_began(turn);
        self.activity.notify_waiters();
        ticket
    }

    /// Serial scheduling: the request answered. Returns once `turn` holds
    /// the turn again (or is no longer live), so its handler resumes alone.
    async fn ingress_ended(&self, turn: serial::Turn, ticket: u64) {
        self.lock().ingress_ended(ticket);
        self.activity.notify_waiters();
        self.await_turn(turn, Wake::Server).await;
    }

    /// Serial scheduling: return once `turn` holds the turn (or is no
    /// longer live), queueing it for the turn meanwhile — at once when the
    /// server woke it, between turns when something outside did.
    async fn await_turn(&self, turn: serial::Turn, wake: Wake) {
        loop {
            let notified = self.granted.notified();
            let granted = {
                let mut state = self.lock();
                if state.may_run(turn) {
                    return;
                }
                match wake {
                    Wake::Server => state.make_ready(turn),
                    Wake::Outside => state.woke(turn),
                }
                state.schedule()
            };
            if granted {
                self.turn_granted();
            }
            let _ = tokio::time::timeout(Duration::from_millis(2), notified).await;
        }
    }

    /// Serial scheduling: an ingress request the handler of a turn issued
    /// was dropped unanswered.
    fn ingress_abandoned(&self, ticket: u64) {
        self.lock().ingress_abandoned(ticket);
        self.activity.notify_waiters();
    }

    /// Serial scheduling: wait until an ingress request from outside every
    /// attempt may land. The request has landed once the returned guard
    /// drops: drop it once the request has reached the server. A request
    /// its caller drops once admitted frees the next one all the same.
    async fn wait_to_land(self: &Arc<Self>, url: &str, body: &bytes::Bytes) -> Landing {
        let (admit, admitted) = tokio::sync::oneshot::channel();
        self.lock()
            .wait_to_land(url.to_owned(), body.clone(), admit);
        self.activity.notify_waiters();
        let mut landing = Landing {
            shared: Arc::clone(self),
            admitted,
        };
        let _ = (&mut landing.admitted).await;
        landing
    }

    /// Serial scheduling: the turn of the attempt whose task is running
    /// this code, when that attempt is one of this server's.
    fn current_turn(self: &Arc<Self>) -> Option<serial::Turn> {
        if self.config.scheduling != Scheduling::Serial {
            return None;
        }
        attempt::current_turn(self)
    }
}

/// An in-process Restate server for one endpoint. Cheap to clone.
#[derive(Clone)]
pub struct RestateTestServer {
    shared: Arc<Shared>,
    _shutdown: Arc<Shutdown>,
}

/// Held by every handle of one server; the last one dropped stops the
/// server's live attempts, whose tasks would otherwise hold it forever.
struct Shutdown {
    shared: Weak<Shared>,
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut state = shared.lock();
        for invocation in &mut state.invocations {
            if let Status::Running(attempt) = &mut invocation.status {
                attempt.close();
                if let Some(task) = attempt.task.take() {
                    task.abort();
                }
            }
            // Ingress callers still waiting on an outcome hold the server
            // through their route; dropping their senders answers them, so
            // their tasks end instead of keeping a dead server alive.
            invocation.waiters.clear();
        }
        state.timers.clear();
        state.shut_down();
        drop(state);
        shared.activity.notify_waiters();
    }
}

/// Watches a server's release: taken from a live server, it tells when the
/// last handle is gone, the server's state is freed and none of its tasks is
/// left.
#[derive(Clone)]
pub struct DropWatch {
    shared: Weak<Shared>,
    tasks: Arc<AtomicUsize>,
}

impl DropWatch {
    /// Whether the server is freed and all its tasks have ended.
    pub fn is_freed(&self) -> bool {
        self.shared.strong_count() == 0 && self.live_tasks() == 0
    }

    /// The server's tasks still alive.
    pub fn live_tasks(&self) -> usize {
        self.tasks.load(Ordering::SeqCst)
    }

    /// Wait until the server is freed, for at most `within` of wall time.
    /// Returns whether it was.
    pub async fn freed_within(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while !self.is_freed() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }
}

impl std::fmt::Debug for RestateTestServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateTestServer")
            .field("seed", &self.shared.config.seed)
            .finish_non_exhaustive()
    }
}

/// Why the server could not start or register a deployment.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("the endpoint's discovery document could not be read: {0}")]
    Discovery(String),
    #[error("the server needs a Tokio runtime to run attempts on")]
    NoRuntime,
    #[error("the server already has a deployment registered")]
    AlreadyRegistered,
}

/// What an introspection read reports about one invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct InvocationView {
    pub id: String,
    pub target: String,
    pub status: &'static str,
    pub attempts: u32,
    pub suspensions: u32,
    pub journal_len: usize,
    /// The `retry_count` restate reports: failed attempts in this loop.
    pub retry_count: u32,
    pub last_failure: Option<(u32, String)>,
    /// For a running attempt: whether it is blocked on the server (its SDK
    /// waits on input and everything it wrote is applied) rather than on
    /// its own work.
    pub blocked_on_server: Option<bool>,
}

/// One journal entry as introspection reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntryView {
    pub ty: MessageType,
    pub name: Option<String>,
    /// A digest of the entry's bytes, for comparing journals across runs.
    pub digest: u64,
    /// The entry's protobuf payload.
    pub payload: bytes::Bytes,
}

impl JournalEntryView {
    /// What a journaled `ctx.run` settled on — the value bytes it produced, or
    /// the failure it was recorded with — on a `RunCompletionNotification`
    /// entry; `None` on every other entry.
    pub fn run_completion(&self) -> Option<Result<bytes::Bytes, (u32, String)>> {
        use crate::protocol::generated::{
            RunCompletionNotificationMessage,
            run_completion_notification_message::Result as RunResult,
        };
        use prost::Message as _;
        if self.ty != MessageType::RunCompletionNotification {
            return None;
        }
        let notification = RunCompletionNotificationMessage::decode(self.payload.clone()).ok()?;
        match notification.result? {
            RunResult::Value(value) => Some(Ok(value.content)),
            RunResult::Failure(failure) => Some(Err((failure.code, failure.message))),
        }
    }
}

impl RestateTestServer {
    /// Start a server with no deployment. Its [`transport`](Self::transport)
    /// works at once, so a deployment whose services need a connection to
    /// this very server can be built before it is
    /// [`register`](Self::register)ed. Must run inside a Tokio runtime.
    pub fn new(config: ServerConfig) -> Result<Self, StartError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| StartError::NoRuntime)?;
        let state = State::new(config.seed, config.start_time_ms, config.scheduling);
        let shared = Arc::new(Shared {
            deployment: OnceLock::new(),
            config,
            runtime,
            state: Mutex::new(state),
            activity: Arc::new(Notify::new()),
            granted: Notify::new(),
            time_listener: OnceLock::new(),
            tasks: Arc::new(AtomicUsize::new(0)),
        });
        if let TimeMode::AutoAdvance { idle, horizon } = shared.config.time {
            shared.spawn(auto_advance(Arc::downgrade(&shared), idle, horizon));
        }
        if shared.config.scheduling == Scheduling::Serial {
            shared.spawn(serial::drive(Arc::downgrade(&shared)));
        }
        let shutdown = Arc::new(Shutdown {
            shared: Arc::downgrade(&shared),
        });
        Ok(Self {
            shared,
            _shutdown: shutdown,
        })
    }

    /// Register `endpoint` as the server's one deployment, reading its
    /// discovery document as `restate-server` does.
    pub async fn register(&self, endpoint: Endpoint) -> Result<(), StartError> {
        let catalog = Catalog::discover(&endpoint)
            .await
            .map_err(StartError::Discovery)?;
        self.shared
            .deployment
            .set(Deployment { endpoint, catalog })
            .map_err(|_| StartError::AlreadyRegistered)
    }

    /// Start a server and register `endpoint` on it.
    pub async fn start(endpoint: Endpoint, config: ServerConfig) -> Result<Self, StartError> {
        let server = Self::new(config)?;
        server.register(endpoint).await?;
        Ok(server)
    }

    /// Where a test declares the gates it holds handlers on: see
    /// [`OutsideGates`].
    pub fn outside_gates(&self) -> OutsideGates {
        OutsideGates {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// A watch on this server's release, for tests that prove a dropped
    /// server frees everything it held.
    pub fn drop_watch(&self) -> DropWatch {
        DropWatch {
            shared: Arc::downgrade(&self.shared),
            tasks: Arc::clone(&self.shared.tasks),
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.shared.config
    }

    /// The base URL of the server's ingress and admin APIs.
    pub fn ingress_url(&self) -> &str {
        &self.shared.config.ingress_url
    }

    /// The server's ingress and admin APIs as an HTTP transport: hand it to
    /// a Restate connection in place of a network client.
    pub fn transport(&self) -> Arc<dyn HttpTransport> {
        Arc::new(ingress::IngressTransport::new(&self.shared))
    }

    /// The service names the registered endpoint serves.
    pub fn service_names(&self) -> Vec<String> {
        self.shared.catalog().names().map(str::to_owned).collect()
    }

    // --- virtual time ----------------------------------------------------

    pub fn now_ms(&self) -> u64 {
        self.shared.lock().now_ms
    }

    /// Call `listener` with the new virtual epoch milliseconds every time
    /// virtual time moves (once; a second listener is refused). A
    /// `TestClock` the stores read follows the server this way.
    pub fn on_time_moved(&self, listener: Arc<dyn Fn(u64) + Send + Sync>) -> bool {
        self.shared.time_listener.set(listener).is_ok()
    }

    /// Move virtual time forward by `duration`, firing every timer due.
    pub fn advance(&self, duration: Duration) -> usize {
        let mut state = self.shared.lock();
        let target = state
            .now_ms
            .saturating_add(processor::duration_ms(duration));
        state.advance_to(&self.shared, target)
    }

    /// Move virtual time to `epoch_ms` (never backwards), firing every timer
    /// due. A sim clock drives the server through this.
    pub fn advance_to(&self, epoch_ms: u64) -> usize {
        self.shared.lock().advance_to(&self.shared, epoch_ms)
    }

    /// Fire the earliest pending timer, moving time to it. Returns the new
    /// virtual time, or `None` when no timer is pending.
    pub fn fire_next_timer(&self) -> Option<u64> {
        self.shared.lock().fire_next(&self.shared)
    }

    pub fn timers(&self) -> Vec<TimerView> {
        self.shared.lock().timers()
    }

    /// Wait until every live attempt is blocked on the server (or there is
    /// none). In [`TimeMode::AutoAdvance`] this also waits for the retries
    /// and in-horizon timers it fires by itself; every other timer is the
    /// caller's to fire.
    pub async fn settle(&self) {
        loop {
            let notified = self.shared.activity.notified();
            {
                let state = self.shared.lock();
                let timers_pending = match self.shared.config.time {
                    TimeMode::AutoAdvance { horizon, .. } => {
                        state.has_auto_timer(processor::duration_ms(horizon))
                    }
                    TimeMode::Manual => false,
                };
                if state.is_quiescent() && !timers_pending {
                    return;
                }
            }
            let _ = tokio::time::timeout(Duration::from_millis(5), notified).await;
        }
    }

    // --- crashes and operator commands -------------------------------------

    /// Script a crash.
    pub fn crash_on(&self, rule: CrashRule) {
        self.shared.lock().crash_plan.add(rule);
    }

    /// Crash attempts at random frames, seeded by the server's seed.
    pub fn crash_randomly(&self, random: Option<RandomCrashes>) {
        self.shared.lock().crash_plan.set_random(random);
    }

    pub fn clear_crashes(&self) {
        self.shared.lock().crash_plan.clear();
    }

    /// Crash `invocation`'s running attempt now and replay it. Returns
    /// whether an attempt was running.
    pub fn crash(&self, invocation: &str) -> bool {
        let mut state = self.shared.lock();
        match state.lookup(invocation) {
            Some(key) => state.crash(&self.shared, key),
            None => false,
        }
    }

    /// Cancel `invocation` as the admin API does.
    pub fn cancel(&self, invocation: &str) -> Option<bool> {
        let mut state = self.shared.lock();
        let key = state.lookup(invocation)?;
        Some(state.cancel(&self.shared, key) != ControlResult::AlreadyCompleted)
    }

    /// Kill `invocation` as the admin API does.
    pub fn kill(&self, invocation: &str) -> Option<bool> {
        let mut state = self.shared.lock();
        let key = state.lookup(invocation)?;
        let mut tasks = Vec::new();
        Some(state.kill(&self.shared, key, &mut tasks) != ControlResult::AlreadyCompleted)
    }

    /// Kill `invocation` as the admin API does, and wait until every attempt
    /// task the kill stopped has actually ended. An abort is cooperative —
    /// the runtime drops the task only once its poll in flight returns, and
    /// a replay resolves every step inline, so a killed attempt can still
    /// run the rest of a poll after `kill` returns. A caller about to make
    /// a killed invocation's work visible again — resubmitting its workflow
    /// key, say — waits here so that last poll cannot overtake it.
    pub async fn kill_and_await(&self, invocation: &str) -> Option<bool> {
        let (killed, tasks) = {
            let mut state = self.shared.lock();
            let key = state.lookup(invocation)?;
            let mut tasks = Vec::new();
            let killed =
                state.kill(&self.shared, key, &mut tasks) != ControlResult::AlreadyCompleted;
            (killed, tasks)
        };
        for task in tasks {
            let _ = task.await;
        }
        Some(killed)
    }

    /// Purge a completed `invocation` as the admin API does: its journal, its
    /// id and, for a workflow run, its key's state and promises are gone, and
    /// the next submission of that workflow key starts over with an empty
    /// journal. `Some(false)` when the invocation has not completed.
    pub fn purge(&self, invocation: &str) -> Option<bool> {
        let mut state = self.shared.lock();
        let key = state.lookup(invocation)?;
        Some(state.purge(key))
    }

    /// Resume a paused `invocation` as the admin API does.
    pub fn resume(&self, invocation: &str) -> Option<bool> {
        let mut state = self.shared.lock();
        let key = state.lookup(invocation)?;
        Some(state.resume(&self.shared, key))
    }

    // --- introspection ------------------------------------------------------

    pub fn stats(&self) -> Stats {
        self.shared.lock().stats.clone()
    }

    /// Every retained invocation: a purged one is gone.
    pub fn invocations(&self) -> Vec<InvocationView> {
        let state = self.shared.lock();
        state
            .invocations
            .iter()
            .enumerate()
            .filter(|(index, _)| state.is_retained(InvKey(*index)))
            .map(|(_, invocation)| InvocationView {
                id: invocation.id.as_str().to_owned(),
                target: invocation.target.display(),
                status: invocation.status.name(),
                attempts: invocation.attempts,
                suspensions: invocation.suspensions,
                journal_len: invocation.journal.len(),
                retry_count: invocation.retry.failures_in_loop,
                blocked_on_server: match &invocation.status {
                    // A closed input is work the attempt has not drained
                    // yet, not a wait on the server: its starved flag still
                    // reads true until the SDK polls the end of input.
                    Status::Running(attempt) => Some(attempt.is_open() && attempt.probe.is_idle()),
                    _ => None,
                },
                last_failure: invocation
                    .retry
                    .last_failure
                    .as_ref()
                    .map(|failure| (failure.code, failure.message.clone())),
            })
            .collect()
    }

    /// Under [`Scheduling::Serial`], every grant of the turn so far, in
    /// order: the invocation id and attempt number that ran. Equal across
    /// two runs of one seed exactly when their attempts interleaved the
    /// same way. Empty under [`Scheduling::Concurrent`].
    pub fn schedule_trace(&self) -> Vec<(String, u32)> {
        let state = self.shared.lock();
        state.serial.as_ref().map_or_else(Vec::new, |serial| {
            serial
                .trace()
                .iter()
                .map(|&(key, number)| (state.invocations[key.0].id.as_str().to_owned(), number))
                .collect()
        })
    }

    /// `invocation`'s journal, commands and notifications in stored order.
    pub fn journal(&self, invocation: &str) -> Option<Vec<JournalEntryView>> {
        let state = self.shared.lock();
        let key = state.lookup(invocation)?;
        Some(
            state.invocations[key.0]
                .journal
                .iter()
                .map(|entry| JournalEntryView {
                    ty: entry.frame.ty,
                    name: command_name(&entry.frame),
                    digest: fnv1a(&stable_payload(&entry.frame)),
                    payload: entry.frame.payload.clone(),
                })
                .collect(),
        )
    }

    /// A digest of every invocation's id, target and journal, taken in id
    /// order: equal across two runs exactly when both produced the same
    /// invocations with the same journals, however concurrent handlers
    /// interleaved.
    pub fn journal_digest(&self) -> u64 {
        let state = self.shared.lock();
        let mut invocations: Vec<_> = state
            .invocations
            .iter()
            .enumerate()
            .filter(|(index, _)| state.is_retained(InvKey(*index)))
            .map(|(_, invocation)| invocation)
            .collect();
        invocations.sort_by(|left, right| left.id.cmp(&right.id));
        let mut digest = FNV_OFFSET;
        for invocation in invocations {
            digest = fnv1a_extend(digest, invocation.id.as_str().as_bytes());
            digest = fnv1a_extend(digest, invocation.target.display().as_bytes());
            for entry in &invocation.journal {
                digest = fnv1a_extend(digest, &entry.frame.ty.code().to_be_bytes());
                digest = fnv1a_extend(digest, &stable_payload(&entry.frame));
            }
        }
        digest
    }

    /// Whether `invocation` has completed, and with what: `Ok(bytes)` or
    /// `Err((code, message))`.
    pub fn outcome(&self, invocation: &str) -> Option<Result<bytes::Bytes, (u32, String)>> {
        let state = self.shared.lock();
        let key = state.lookup(invocation)?;
        match &state.invocations[key.0].status {
            Status::Completed(model::Outcome::Success(bytes)) => Some(Ok(bytes.clone())),
            Status::Completed(model::Outcome::Failure(failure)) => {
                Some(Err((failure.code, failure.message.clone())))
            }
            _ => None,
        }
    }
}

/// The virtual-time driver of [`TimeMode::AutoAdvance`].
async fn auto_advance(shared: Weak<Shared>, idle: Duration, horizon: Duration) {
    let horizon_ms = processor::duration_ms(horizon);
    loop {
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let activity = Arc::clone(&strong.activity);
        let quiescent_with_timer = {
            let mut state = strong.lock();
            // Time flows at wall speed between jumps: a timer past the
            // horizon fires when a real server would have fired it.
            let flowed = state.wall_flowed_ms();
            if state
                .next_timer_ms()
                .is_some_and(|fire_at| fire_at <= flowed)
            {
                state.advance_to(&strong, flowed);
            }
            state.is_quiescent() && state.next_timer_ms().is_some()
        };
        drop(strong);
        if !quiescent_with_timer {
            let _ = tokio::time::timeout(Duration::from_millis(50), activity.notified()).await;
            continue;
        }
        // Stay quiet for `idle` before time moves: give work outside the
        // server (a test, a worker) the chance to act first.
        if tokio::time::timeout(idle, activity.notified())
            .await
            .is_ok()
        {
            continue;
        }
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let moved = {
            let mut state = strong.lock();
            if !state.is_quiescent() || state.fire_next_retry(&strong) {
                true
            } else {
                let within_horizon = state
                    .next_timer_ms()
                    .is_some_and(|fire_at| fire_at <= state.now_ms.saturating_add(horizon_ms));
                within_horizon && state.fire_next(&strong).is_some()
            }
        };
        drop(strong);
        if !moved {
            // Nothing may move by itself: wait for activity or an explicit
            // advance.
            let _ = tokio::time::timeout(Duration::from_millis(50), activity.notified()).await;
        }
    }
}

fn command_name(frame: &Frame) -> Option<String> {
    use prost::Message as _;
    // Every command carries its entry name at field 12 (`name`, or
    // `entry_name` on SendSignal); the notification template reserves it.
    #[derive(Clone, PartialEq, prost::Message)]
    struct Named {
        #[prost(string, tag = "12")]
        name: String,
    }
    if !frame.ty.is_command() {
        return None;
    }
    Named::decode(frame.payload.clone())
        .ok()
        .map(|named| named.name)
        .filter(|name| !name.is_empty())
}

/// An entry's payload with the SDK's wall-clock stamps (a sleep's wake-up
/// time, a delayed send's invoke time) zeroed: the bytes a digest compares
/// across runs.
fn stable_payload(frame: &Frame) -> bytes::Bytes {
    use crate::protocol::generated::{OneWayCallCommandMessage, SleepCommandMessage};
    use prost::Message as _;
    match frame.ty {
        MessageType::SleepCommand => frame
            .decode::<SleepCommandMessage>()
            .map(|mut sleep| {
                sleep.wake_up_time = 0;
                bytes::Bytes::from(sleep.encode_to_vec())
            })
            .unwrap_or_else(|_| frame.payload.clone()),
        MessageType::OneWayCallCommand => frame
            .decode::<OneWayCallCommandMessage>()
            .map(|mut send| {
                send.invoke_time = 0;
                bytes::Bytes::from(send.encode_to_vec())
            })
            .unwrap_or_else(|_| frame.payload.clone()),
        _ => frame.payload.clone(),
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_extend(FNV_OFFSET, bytes)
}

fn fnv1a_extend(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}
