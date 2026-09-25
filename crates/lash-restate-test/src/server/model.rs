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
    /// Whether the server holds the input open. Under serial scheduling an
    /// attempt that does not hold the turn sees a close only when it gets
    /// the turn back, so this can be `false` while `input` is still set.
    open: bool,
    /// Serial scheduling: the attempt does not hold the turn, so what the
    /// server delivers waits in `held` until it does.
    gated: bool,
    held: Vec<Bytes>,
    /// Serial scheduling: the attempt task waits on this before it first
    /// polls the endpoint.
    start_gate: Option<oneshot::Sender<()>>,
    pub probe: Arc<InputProbe>,
    pub abort: Option<tokio::task::AbortHandle>,
    /// The journal index of the first notification stored after the input
    /// closed: from there on, the attempt has not seen the journal.
    pub unseen_from: Option<usize>,
    /// Virtual time since which the attempt has been starved, as last
    /// observed by a time advance; the inactivity timeout counts from here.
    pub starved_since_ms: Option<u64>,
}

impl LiveAttempt {
    /// A new attempt whose input is `input` (`None`: closed from the
    /// start). A `start_gate` makes it a serially scheduled attempt that
    /// does not run until [`release`](Self::release)d.
    pub fn new(
        number: u32,
        input: Option<mpsc::UnboundedSender<Bytes>>,
        probe: Arc<InputProbe>,
        abort: tokio::task::AbortHandle,
        start_gate: Option<oneshot::Sender<()>>,
    ) -> Self {
        Self {
            number,
            open: input.is_some(),
            input,
            gated: start_gate.is_some(),
            held: Vec::new(),
            start_gate,
            probe,
            abort: Some(abort),
            unseen_from: None,
            starved_since_ms: None,
        }
    }

    /// Whether the server holds the attempt's input open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Push `frame` down the attempt's open input, or hold it for the
    /// attempt's next turn. Returns whether it was delivered.
    pub fn push(&mut self, frame: Bytes) -> bool {
        if !self.open {
            return false;
        }
        self.starved_since_ms = None;
        if self.gated {
            self.held.push(frame);
            return true;
        }
        self.send(frame)
    }

    fn send(&mut self, frame: Bytes) -> bool {
        let pushed = self
            .input
            .as_ref()
            .is_some_and(|input| input.send(frame).is_ok());
        if pushed {
            self.probe.fed();
        }
        pushed
    }

    /// Close the input: the SDK sees end of input and suspends at its next
    /// await the journal cannot resolve. A gated attempt sees it on its
    /// next turn.
    pub fn close(&mut self) {
        self.open = false;
        if !self.gated {
            self.input = None;
        }
    }

    /// Serial scheduling: whether the attempt has anything to act on once
    /// it gets the turn — its first poll, held frames, or a held close.
    pub fn has_held_work(&self) -> bool {
        self.start_gate.is_some() || !self.held.is_empty() || (!self.open && self.input.is_some())
    }

    /// Serial scheduling: take the turn away; deliveries hold until the
    /// next [`release`](Self::release).
    pub fn gate(&mut self) {
        self.gated = true;
    }

    /// Serial scheduling: give the attempt the turn: start it, or deliver
    /// what it was held.
    pub fn release(&mut self) {
        self.gated = false;
        // Not parked until its handler parks again: whatever it parked on
        // before the grant — a request of its own that has answered, say —
        // may be what the turn lets it go on with.
        self.probe.set_response_drained(false);
        if let Some(gate) = self.start_gate.take() {
            let _ = gate.send(());
        }
        for frame in std::mem::take(&mut self.held) {
            self.send(frame);
        }
        if !self.open {
            self.input = None;
        }
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
    /// An ingress request/response call or attach, with the serial
    /// scheduler's ticket when an attempt's handler issued it.
    Ingress {
        sender: oneshot::Sender<Outcome>,
        ticket: Option<u64>,
    },
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
