//! The server's durable model: invocations, their journals, keys, workflow
//! promises and timers. Everything here is plain data behind the server lock.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use super::body::InputProbe;
use super::catalog::HandlerSpec;
use super::ids::{DeploymentId, InvocationId};
use crate::protocol::Frame;
use crate::protocol::generated::Failure;

/// An index into the server's invocation table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InvKey(pub usize);

/// What an invocation addresses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target {
    pub service: String,
    pub handler: String,
    /// The object or workflow key; `None` for a plain service.
    pub key: Option<String>,
}

impl Target {
    /// The `sys_invocation.target` form: `Svc/key/handler` or `Svc/handler`.
    pub fn display(&self) -> String {
        match &self.key {
            Some(key) => format!("{}/{}/{}", self.service, key, self.handler),
            None => format!("{}/{}", self.service, self.handler),
        }
    }

    pub fn service_key(&self) -> Option<(String, String)> {
        self.key
            .as_ref()
            .map(|key| (self.service.clone(), key.clone()))
    }
}

/// How an invocation ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Success(Bytes),
    Failure(Failure),
}

impl Outcome {
    pub fn failure(code: u32, message: impl Into<String>) -> Self {
        Self::Failure(Failure {
            code,
            message: message.into(),
            metadata: Vec::new(),
        })
    }
}

/// A notification's address inside its invocation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NotificationKey {
    Completion(u32),
    Signal(u32),
    Named(String),
}

/// One journal entry: a command the SDK wrote or a notification the server
/// stored, in stored order. Replay re-sends exactly these frames.
#[derive(Clone, Debug)]
pub struct Entry {
    pub frame: Frame,
    pub notification: Option<NotificationKey>,
}

/// The notifications a suspended invocation waits for; any one of them
/// arriving resumes it (the SDK re-suspends if its await is still open).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WaitSet {
    pub keys: BTreeSet<NotificationKey>,
}

impl WaitSet {
    pub fn contains(&self, key: &NotificationKey) -> bool {
        self.keys.contains(key)
    }
}

/// The live half of a running attempt.
#[derive(Debug)]
pub struct LiveAttempt {
    pub number: u32,
    /// The request-body sender; `None` once the input is closed on the
    /// wire.
    input: Option<mpsc::UnboundedSender<Bytes>>,
    /// Whether the server holds the input open.
    open: bool,
    pub probe: Arc<InputProbe>,
    /// The attempt's task: aborted to stop it, joined to know it stopped.
    /// Tokio drops an aborted task only at its next yield, so a poll in
    /// flight — a replay, say, that resolves every step inline — runs to
    /// its end first; callers that must not be overtaken by that last poll
    /// take the handle and await it.
    pub task: Option<tokio::task::JoinHandle<()>>,
    /// The journal index of the first notification stored after the input
    /// closed: from there on, the attempt has not seen the journal.
    pub unseen_from: Option<usize>,
    /// Virtual time since which the attempt has been starved, as last
    /// observed by a time advance; the inactivity timeout counts from here.
    pub starved_since_ms: Option<u64>,
}

impl LiveAttempt {
    /// A new attempt whose input is `input` (`None`: closed from the start).
    pub fn new(
        number: u32,
        input: Option<mpsc::UnboundedSender<Bytes>>,
        probe: Arc<InputProbe>,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            number,
            open: input.is_some(),
            input,
            probe,
            task: Some(task),
            unseen_from: None,
            starved_since_ms: None,
        }
    }

    /// Whether the server holds the attempt's input open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Push `frame` down the attempt's open input. Returns whether it was delivered.
    pub fn push(&mut self, frame: Bytes) -> bool {
        if !self.open {
            return false;
        }
        self.starved_since_ms = None;
        self.send(frame)
    }

    fn send(&mut self, frame: Bytes) -> bool {
        // Fed before the frame is queued, never after: the send wakes the
        // attempt, which can read the frame and block on its input again on
        // another worker before this returns, and marking it fed then would
        // overwrite that park for good. Every reader of the probe holds the
        // server lock this runs under, so none sees the mark without the
        // frame.
        self.probe.fed();
        self.input
            .as_ref()
            .is_some_and(|input| input.send(frame).is_ok())
    }

    /// Close the input: the SDK suspends at its next unresolved await.
    pub fn close(&mut self) {
        self.open = false;
        self.input = None;
    }
}

/// Where an invocation is in its lifecycle.
#[derive(Debug)]
pub enum Status {
    /// A delayed one-way call waiting for its invoke time.
    Scheduled,
    /// Queued behind its key's lock.
    Inboxed,
    Running(LiveAttempt),
    Suspended(WaitSet),
    /// Waiting for its retry timer.
    BackingOff,
    /// Stopped after exhausting its attempts; resumed only by an operator.
    Paused,
    Completed(Outcome),
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Inboxed => "pending",
            Self::Running(_) => "running",
            Self::Suspended(_) => "suspended",
            Self::BackingOff => "backing-off",
            Self::Paused => "paused",
            Self::Completed(_) => "completed",
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed(_))
    }
}

/// Who is told when an invocation completes.
#[derive(Debug)]
pub enum Waiter {
    /// A `CallCommand`'s result notification.
    Call { caller: InvKey, completion_id: u32 },
    /// An `AttachInvocationCommand`'s result notification.
    Attach { caller: InvKey, completion_id: u32 },
    /// An ingress request/response call or attach.
    Ingress { sender: oneshot::Sender<Outcome> },
}

/// The invoker's retry bookkeeping for one invocation.
#[derive(Clone, Debug, Default)]
pub struct RetryState {
    /// Failed attempts since the journal last grew: what the SDK reads as
    /// `retry_count_since_last_stored_entry`.
    pub failures_since_last_entry: u32,
    /// Failed attempts in the current retry loop; the policy's max attempts
    /// counts these, and an operator resume starts a new loop.
    pub failures_in_loop: u32,
    /// The last attempt failure, reported by introspection.
    pub last_failure: Option<AttemptFailure>,
}

/// Why an attempt failed.
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptFailure {
    pub code: u32,
    pub message: String,
    pub related_command: Option<String>,
}

#[derive(Debug)]
pub struct Invocation {
    pub id: InvocationId,
    pub target: Target,
    pub spec: HandlerSpec,
    /// The deployment this invocation runs on, fixed at submission: retries,
    /// suspension resumes and replays all dispatch to it until an operator
    /// resume re-pins it.
    pub pinned_deployment: DeploymentId,
    pub idempotency_key: Option<String>,
    pub random_seed: u64,
    pub journal: Vec<Entry>,
    pub status: Status,
    pub waiters: Vec<Waiter>,
    /// The result the journal's `OutputCommand` holds, once written.
    pub output: Option<Outcome>,
    pub retry: RetryState,
    /// Virtual time the journal last grew, for the SDK's retry telemetry.
    pub last_entry_ms: u64,
    pub created_ms: u64,
    /// The server's modification sequence at this invocation's last change.
    pub modified_seq: u64,
    pub attempts: u32,
    pub suspensions: u32,
    /// The invocation that called or sent this one, if any.
    pub parent: Option<InvKey>,
    /// Invocations this one's journal called or sent, for kill's cascade.
    pub children: Vec<InvKey>,
    /// Completion ids of journaled `ctx.run`s whose result is not stored yet:
    /// closures in flight (or to re-run on the next attempt).
    pub pending_runs: BTreeSet<u32>,
}

/// A durable promise of a workflow key.
#[derive(Debug)]
pub enum PromiseState {
    Pending(Vec<(InvKey, u32)>),
    Completed(Outcome),
}

/// Per-`(service, key)` record: the exclusive lock, its inbox, the key's
/// state, and — for a workflow — its run and promises.
#[derive(Debug, Default)]
pub struct KeyRecord {
    pub locked_by: Option<InvKey>,
    pub inbox: VecDeque<InvKey>,
    /// The longest the inbox ever was: how many invocations ever waited on
    /// the lock at once.
    pub inbox_high_water: usize,
    pub state: BTreeMap<String, Bytes>,
    pub workflow_run: Option<InvKey>,
    pub promises: BTreeMap<String, PromiseState>,
}

/// What a timer does when virtual time reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerAction {
    /// Complete a `SleepCommand`.
    Sleep {
        invocation: InvKey,
        completion_id: u32,
    },
    /// Start a delayed one-way call.
    Start { invocation: InvKey },
    /// Retry a backing-off invocation.
    Retry { invocation: InvKey },
}

/// A pending timer, as introspection reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerView {
    pub fire_at_ms: u64,
    pub invocation: String,
    pub target: String,
    pub kind: &'static str,
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Wake, Waker};

    use bytes::Bytes;
    use http_body::Body;
    use tokio::sync::mpsc;

    use super::LiveAttempt;
    use crate::server::body::{AttemptBody, InputProbe};

    /// The attempt's side of the race: woken by the server's send, it reads
    /// everything queued and blocks on its empty input again before the
    /// server's push has returned, as a handler on another worker can.
    struct DrainOnWake(Mutex<AttemptBody>);

    impl DrainOnWake {
        fn drain(&self, waker: &Waker) {
            let mut body = self.0.lock().expect("the body's lock");
            let mut cx = Context::from_waker(waker);
            while Pin::new(&mut *body).poll_frame(&mut cx).is_ready() {}
        }
    }

    impl Wake for DrainOnWake {
        fn wake(self: Arc<Self>) {
            self.drain(Waker::noop());
        }
    }

    /// An attempt that read a pushed frame and blocked on its input again
    /// reads starved once the push returns, however early it got there. A
    /// push that marked the attempt fed after queuing the frame overwrote
    /// that park: the attempt then read as busy for good, so no time advance
    /// ever ran its inactivity timeout and a wait for it to park never ended
    /// (FIG-4511).
    #[tokio::test]
    async fn an_attempt_that_parks_again_inside_a_push_reads_starved() {
        let (input, receiver) = mpsc::unbounded_channel::<Bytes>();
        let probe = Arc::new(InputProbe::default());
        let attempt_side = Arc::new(DrainOnWake(Mutex::new(AttemptBody::new(
            receiver,
            Arc::clone(&probe),
            Arc::new(|| {}),
        ))));
        attempt_side.drain(&Waker::from(Arc::clone(&attempt_side)));
        assert!(probe.is_starved(), "blocked on its empty input");
        let mut attempt =
            LiveAttempt::new(1, Some(input), Arc::clone(&probe), tokio::spawn(async {}));

        assert!(attempt.push(Bytes::from_static(b"frame")));

        assert!(
            probe.is_starved(),
            "the attempt drained the frame and blocked on its input again"
        );
    }
}
