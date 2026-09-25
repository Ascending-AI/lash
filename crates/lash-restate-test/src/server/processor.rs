//! The partition processor: every server-side reaction to an SDK message, an
//! ingress request, a timer or an operator command, applied under the server
//! lock in one synchronous step.
//!
//! Nothing here awaits. Attempts run as tasks that call back into these
//! methods frame by frame, so the journal order is the order the server
//! applied frames in — the same total order `restate-server` imposes by
//! appending to its partition log.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

use super::Shared;
use super::body::{AttemptBody, InputProbe};
use super::catalog::{HandlerKind, OnMaxAttempts, Unresolved};
use super::crash::{CrashPlan, CrashSite};
use super::ids::SeededIds;
use super::model::{
    AttemptFailure, Entry, InvKey, Invocation, KeyRecord, LiveAttempt, NotificationKey, Outcome,
    RetryState, Status, Target, TimerAction, TimerView, WaitSet, Waiter,
};
use crate::protocol::generated::{
    self as pb, ErrorMessage, Failure, InputCommandMessage, NotificationTemplate,
    ProposeRunCompletionAckMessage, ProposeRunCompletionMessage, RunCommandMessage, StartMessage,
    SuspensionMessage, notification_template,
};
use crate::protocol::{CANCEL_SIGNAL_ID, Frame, MessageType, ProtocolVersion, SuspensionMessageV6};

pub use super::timers::{duration_ms, wall_delay_ms};

/// How an operator command (cancel, kill) applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlResult {
    /// The cancel signal was appended for the handler to act on (HTTP 202).
    Appended,
    /// The invocation was ended by the command itself (HTTP 200).
    Done,
    /// The invocation had already completed (HTTP 409).
    AlreadyCompleted,
}

/// What an attempt task does after the server applied one of its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading and drop the handler: the attempt is over (it ended,
    /// suspended or failed), stale, or crashed by a crash plan.
    Stop,
}

/// Whether a submission made a new invocation or found an existing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    Fresh,
    /// A request with an idempotency key the server already holds: the
    /// submitter attaches to that invocation's result.
    Idempotent,
    /// A workflow `run` whose key already ran or runs: a caller is refused
    /// with `409 the workflow method was already invoked`, a send reports
    /// `PreviouslyAccepted`.
    WorkflowRunExists,
}

/// The failure a second `run` of one workflow key gets.
pub const WORKFLOW_ALREADY_INVOKED: (u32, &str) = (409, "the workflow method was already invoked");

/// A request to run something, from ingress or from a handler.
#[derive(Clone, Debug)]
pub struct Submission {
    pub target: Target,
    pub input: Bytes,
    pub headers: Vec<pb::Header>,
    pub idempotency_key: Option<String>,
    /// Virtual time before which a one-way call must not start.
    pub start_at_ms: Option<u64>,
    pub parent: Option<InvKey>,
}

/// Why a submission was refused before any invocation existed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Unresolved(Unresolved),
    MissingKey(String),
}

impl SubmitError {
    pub fn message(&self) -> String {
        match self {
            Self::Unresolved(Unresolved::Service(service)) => {
                format!("Service '{service}' not found. Make sure the service is registered.")
            }
            Self::Unresolved(Unresolved::Handler { service, handler }) => format!(
                "Service handler '{service}/{handler}' not found. Make sure the handler is registered."
            ),
            Self::MissingKey(service) => format!("The service '{service}' requires a key"),
        }
    }
}

/// Counters over the server's lifetime.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub invocations: u64,
    pub attempts: u64,
    /// Attempts that started over a non-trivial journal: suspensions resumed,
    /// retries and crash recoveries all replay.
    pub replays: u64,
    pub suspensions: u64,
    pub retries: u64,
    pub crashes: u64,
    pub timers_fired: u64,
    /// Serial scheduling: turns taken from a holder that stalled without
    /// the server seeing why (see the `serial` module docs).
    pub stall_preemptions: u64,
}

/// The effective invoker retry policy of one handler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    pub initial_interval: Duration,
    pub exponentiation_factor: f64,
    pub max_interval: Duration,
    /// Attempts per retry loop before `on_max_attempts`; `None` retries forever.
    pub max_attempts: Option<u32>,
    pub on_max_attempts: OnMaxAttempts,
}

impl RetryPolicy {
    /// The delay before retry number `failures` (1-based) of a loop.
    pub fn delay(&self, failures: u32) -> Duration {
        let exponent = i32::try_from(failures.saturating_sub(1)).unwrap_or(i32::MAX);
        let scaled = self.initial_interval.as_secs_f64()
            * self.exponentiation_factor.max(1.0).powi(exponent);
        let capped = scaled.min(self.max_interval.as_secs_f64());
        Duration::from_secs_f64(if capped.is_finite() {
            capped.max(0.0)
        } else {
            0.0
        })
    }
}

pub struct State {
    pub now_ms: u64,
    pub ids: SeededIds,
    pub seq: u64,
    pub invocations: Vec<Invocation>,
    pub by_id: HashMap<String, InvKey>,
    /// `(service, key, handler, idempotency key)` → the invocation it names.
    pub idempotency: HashMap<(String, Option<String>, String, String), InvKey>,
    /// Ingress submissions seen per `(target, input)`, for their ids.
    pub ingress_counts: HashMap<(String, Bytes), u64>,
    pub keys: HashMap<(String, String), KeyRecord>,
    /// Timers by `(fire_at_ms, seeded tie-break, sequence)`.
    pub timers: BTreeMap<(u64, u64, u64), TimerAction>,
    pub crash_plan: CrashPlan,
    pub stats: Stats,
    /// The serial scheduler, under [`Scheduling::Serial`](super::Scheduling::Serial).
    pub serial: Option<super::serial::Serial>,
    /// Virtual time at the last move, and the wall instant it happened: auto
    /// advance lets virtual time flow at wall speed from here.
    pub anchor: (u64, std::time::Instant),
    /// Wall-clock microseconds the frame being applied was read at.
    pub frame_received_us: u128,
    /// The last handle is gone: nothing starts, and ingress answers 503.
    pub shut: bool,
}

impl State {
    /// Virtual now as auto-advance sees it: the last virtual time plus the
    /// wall time elapsed since it was set.
    pub fn wall_flowed_ms(&self) -> u64 {
        let elapsed = u64::try_from(self.anchor.1.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.anchor.0.saturating_add(elapsed)
    }

    pub fn new(seed: u64, start_ms: u64, scheduling: super::Scheduling) -> Self {
        Self {
            serial: (scheduling == super::Scheduling::Serial).then(super::serial::Serial::default),
            anchor: (start_ms, std::time::Instant::now()),
            frame_received_us: 0,
            shut: false,
            now_ms: start_ms,
            ids: SeededIds::new(seed),
            seq: 0,
            invocations: Vec::new(),
            by_id: HashMap::new(),
            idempotency: HashMap::new(),
            ingress_counts: HashMap::new(),
            keys: HashMap::new(),
            timers: BTreeMap::new(),
            crash_plan: CrashPlan::default(),
            stats: Stats::default(),
        }
    }

    /// The server's last handle is gone: start nothing more, and let every
    /// outside request that waits to land go, to a 503.
    pub(super) fn shut_down(&mut self) {
        self.shut = true;
        if let Some(serial) = &mut self.serial {
            serial.shut_down();
        }
    }

    pub(super) fn touch(&mut self, key: InvKey) {
        self.seq += 1;
        self.invocations[key.0].modified_seq = self.seq;
    }

    pub fn lookup(&self, printed: &str) -> Option<InvKey> {
        self.by_id.get(printed).copied()
    }

    pub(super) fn key_record(&mut self, service_key: (String, String)) -> &mut KeyRecord {
        self.keys.entry(service_key).or_default()
    }

    /// Replace one key's whole state, as the admin API's state modification.
    pub fn replace_state(&mut self, service_key: (String, String), state: BTreeMap<String, Bytes>) {
        self.key_record(service_key).state = state;
    }

    // ---------------------------------------------------------------------
    // Submission and scheduling
    // ---------------------------------------------------------------------

    /// Submit `submission`: dedupe a workflow run by its key and any request
    /// by its idempotency key, otherwise create the invocation and schedule it.
    pub fn submit(
        &mut self,
        sh: &Arc<Shared>,
        submission: Submission,
    ) -> Result<(InvKey, Submitted), SubmitError> {
        let spec = sh
            .catalog()
            .resolve(&submission.target.service, &submission.target.handler)
            .map_err(SubmitError::Unresolved)?;
        if spec.kind.is_keyed() && submission.target.key.is_none() {
            return Err(SubmitError::MissingKey(submission.target.service.clone()));
        }
        let target = Target {
            key: if spec.kind.is_keyed() {
                submission.target.key.clone()
            } else {
                None
            },
            ..submission.target.clone()
        };
        if spec.kind == HandlerKind::WorkflowRun
            && let Some(service_key) = target.service_key()
            && let Some(existing) = self
                .keys
                .get(&service_key)
                .and_then(|record| record.workflow_run)
        {
            return Ok((existing, Submitted::WorkflowRunExists));
        }
        let idempotency = submission.idempotency_key.clone().map(|idempotency_key| {
            (
                target.service.clone(),
                target.key.clone(),
                target.handler.clone(),
                idempotency_key,
            )
        });
        if let Some(existing) = idempotency
            .as_ref()
            .and_then(|key| self.idempotency.get(key))
        {
            return Ok((*existing, Submitted::Idempotent));
        }

        let (id, random_seed) = self.derive_id(&spec, &target, &submission);
        let input = InputCommandMessage {
            headers: submission.headers.clone(),
            value: Some(pb::Value {
                content: submission.input.clone(),
            }),
            name: String::new(),
        };
        let key = InvKey(self.invocations.len());
        self.invocations.push(Invocation {
            id: id.clone(),
            target: target.clone(),
            spec,
            idempotency_key: submission.idempotency_key.clone(),
            random_seed,
            journal: vec![Entry {
                frame: Frame::of(MessageType::InputCommand, &input),
                notification: None,
            }],
            status: Status::Inboxed,
            waiters: Vec::new(),
            output: None,
            retry: RetryState::default(),
            last_entry_ms: self.now_ms,
            created_ms: self.now_ms,
            modified_seq: 0,
            attempts: 0,
            suspensions: 0,
            parent: submission.parent,
            children: Vec::new(),
            pending_runs: std::collections::BTreeSet::new(),
        });
        self.by_id.insert(id.as_str().to_owned(), key);
        if let Some(idempotency) = idempotency {
            self.idempotency.insert(idempotency, key);
        }
        if spec.kind == HandlerKind::WorkflowRun
            && let Some(service_key) = target.service_key()
        {
            self.key_record(service_key).workflow_run = Some(key);
        }
        self.stats.invocations += 1;
        self.touch(key);
        match submission.start_at_ms {
            Some(at) if at > self.now_ms => {
                self.invocations[key.0].status = Status::Scheduled;
                self.add_timer(at, TimerAction::Start { invocation: key });
            }
            _ => self.enqueue(sh, key),
        }
        Ok((key, Submitted::Fresh))
    }

    /// The id (and random seed) of the invocation `submission` creates: from
    /// its workflow key, its idempotency key, the parent command that made
    /// it, or — for ingress — its target, input and how many identical
    /// requests came before it. Never from creation order.
    pub(super) fn derive_id(
        &mut self,
        spec: &super::catalog::HandlerSpec,
        target: &Target,
        submission: &Submission,
    ) -> (super::ids::InvocationId, u64) {
        let service = target.service.as_bytes();
        let key = target.key.as_deref().unwrap_or_default().as_bytes();
        let handler = target.handler.as_bytes();
        let derived = if spec.kind == HandlerKind::WorkflowRun {
            self.ids.derive(&[b"wf", service, key])
        } else if let Some(idempotency_key) = &submission.idempotency_key {
            self.ids
                .derive(&[b"ik", service, key, handler, idempotency_key.as_bytes()])
        } else if let Some(parent) = submission.parent {
            let parent_journal = &self.invocations[parent.0];
            let command_index = parent_journal
                .journal
                .iter()
                .filter(|entry| entry.frame.ty.is_command())
                .count() as u64;
            self.ids.derive(&[
                b"call",
                parent_journal.id.bytes(),
                &command_index.to_be_bytes(),
            ])
        } else {
            let count = self
                .ingress_counts
                .entry((target.display(), submission.input.clone()))
                .or_insert(0);
            *count += 1;
            let count = *count;
            self.ids.derive(&[
                b"ingress",
                target.display().as_bytes(),
                &submission.input,
                &count.to_be_bytes(),
            ])
        };
        if self.by_id.contains_key(derived.0.as_str()) {
            (self.ids.invocation_id(), self.ids.next_u64())
        } else {
            derived
        }
    }

    /// Start `key` now, or queue it behind its key's lock.
    pub(super) fn enqueue(&mut self, sh: &Arc<Shared>, key: InvKey) {
        let invocation = &self.invocations[key.0];
        if invocation.spec.kind.takes_lock()
            && let Some(service_key) = invocation.target.service_key()
        {
            let record = self.key_record(service_key);
            if record.locked_by.is_some() {
                record.inbox.push_back(key);
                self.invocations[key.0].status = Status::Inboxed;
                self.touch(key);
                return;
            }
            record.locked_by = Some(key);
        }
        self.start_attempt(sh, key);
    }

    /// Release `key`'s lock (if it holds one) and start the next queued
    /// invocation that is still waiting for it.
    pub(super) fn release_lock(&mut self, sh: &Arc<Shared>, key: InvKey) {
        let Some(service_key) = self.invocations[key.0].target.service_key() else {
            return;
        };
        let Some(record) = self.keys.get_mut(&service_key) else {
            return;
        };
        if record.locked_by != Some(key) {
            return;
        }
        record.locked_by = None;
        while let Some(next) = self
            .keys
            .get_mut(&service_key)
            .and_then(|record| record.inbox.pop_front())
        {
            if matches!(self.invocations[next.0].status, Status::Inboxed) {
                if let Some(record) = self.keys.get_mut(&service_key) {
                    record.locked_by = Some(next);
                }
                self.start_attempt(sh, next);
                return;
            }
        }
    }

    /// Run a new attempt of `key`: replay its whole journal to a fresh
    /// `Endpoint::handle` stream.
    pub fn start_attempt(&mut self, sh: &Arc<Shared>, key: InvKey) {
        if self.shut {
            return;
        }
        let protocol = sh.config.protocol;
        let always_replay = sh.config.always_replay;
        let now_ms = self.now_ms;
        let state_map = self.state_snapshot(key);
        let invocation = &mut self.invocations[key.0];
        // A plain service has no state: restate-server marks its (empty)
        // eager map partial, and a keyed target gets its whole state.
        let partial_state = !invocation.spec.kind.is_keyed();
        invocation.attempts += 1;
        // Only this attempt's own runs count as in flight: a replay need not
        // reach a run an earlier attempt left open.
        invocation.pending_runs.clear();
        let number = invocation.attempts;
        let journal_len = invocation.journal.len();
        let start = StartMessage {
            id: invocation.id.bytes().clone(),
            debug_id: invocation.id.as_str().to_owned(),
            known_entries: u32::try_from(journal_len).unwrap_or(u32::MAX),
            state_map,
            partial_state,
            key: invocation.target.key.clone().unwrap_or_default(),
            retry_count_since_last_stored_entry: invocation.retry.failures_since_last_entry,
            duration_since_last_stored_entry: now_ms.saturating_sub(invocation.last_entry_ms),
            random_seed: invocation.random_seed,
            scope: None,
            limit_key: None,
            idempotency_key: match protocol {
                ProtocolVersion::V7 => invocation.idempotency_key.clone(),
                ProtocolVersion::V6 => None,
            },
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = sender.send(Frame::of(MessageType::Start, &start).encode());
        for entry in &invocation.journal {
            let _ = sender.send(entry.frame.encode());
        }
        let probe = Arc::new(InputProbe::default());
        let (start_gate, started) = if self.serial.is_some() {
            let (gate, started) = tokio::sync::oneshot::channel();
            (Some(gate), Some(started))
        } else {
            (None, None)
        };
        let invocation = &mut self.invocations[key.0];
        let wake = Arc::clone(&sh.activity);
        let body = AttemptBody::new(
            receiver,
            Arc::clone(&probe),
            Arc::new(move || wake.notify_waiters()),
        );
        let handle = sh.spawn(super::attempt::run(
            Arc::clone(sh),
            key,
            number,
            invocation.target.service.clone(),
            invocation.target.handler.clone(),
            body,
            Arc::clone(&probe),
            started,
        ));
        invocation.status = Status::Running(LiveAttempt::new(
            number,
            (!always_replay).then_some(sender),
            probe,
            handle.abort_handle(),
            start_gate,
        ));
        self.make_ready((key, number));
        self.stats.attempts += 1;
        if number > 1 || journal_len > 1 {
            self.stats.replays += 1;
        }
        self.touch(key);
        sh.activity.notify_waiters();
    }

    /// The eager state an attempt of `key` starts with: its key's whole
    /// state, which makes every state read the SDK issues an eager one.
    pub(super) fn state_snapshot(&self, key: InvKey) -> Vec<pb::start_message::StateEntry> {
        self.invocations[key.0]
            .target
            .service_key()
            .and_then(|service_key| self.keys.get(&service_key))
            .map(|record| {
                record
                    .state
                    .iter()
                    .map(|(name, value)| pb::start_message::StateEntry {
                        key: Bytes::copy_from_slice(name.as_bytes()),
                        value: value.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn running_attempt(&mut self, key: InvKey, number: u32) -> Option<&mut LiveAttempt> {
        match &mut self.invocations.get_mut(key.0)?.status {
            Status::Running(attempt) if attempt.number == number => Some(attempt),
            _ => None,
        }
    }

    /// Close the live attempt's input, if any: the SDK sees end of input and
    /// suspends at its next await that the journal cannot resolve.
    pub(super) fn close_input(&mut self, key: InvKey) {
        if let Status::Running(attempt) = &mut self.invocations[key.0].status {
            attempt.close();
        }
        self.delivered(key);
    }

    // ---------------------------------------------------------------------
    // Journal writes
    // ---------------------------------------------------------------------

    pub(super) fn append_command(&mut self, key: InvKey, frame: Frame) {
        let now_ms = self.now_ms;
        let invocation = &mut self.invocations[key.0];
        if frame.ty == MessageType::RunCommand
            && let Ok(run) = frame.decode::<RunCommandMessage>()
        {
            invocation.pending_runs.insert(run.result_completion_id);
        }
        invocation.journal.push(Entry {
            frame,
            notification: None,
        });
        invocation.last_entry_ms = now_ms;
        invocation.retry.failures_since_last_entry = 0;
    }

    /// Store a notification for `key` and deliver it: straight down a live
    /// stream, as a resume of a suspension waiting for it, or into the
    /// journal for the next attempt to replay.
    pub(super) fn notify(
        &mut self,
        sh: &Arc<Shared>,
        key: InvKey,
        ty: MessageType,
        id: notification_template::Id,
        result: notification_template::Result,
    ) {
        let notification_key = match &id {
            notification_template::Id::CompletionId(id) => NotificationKey::Completion(*id),
            notification_template::Id::SignalId(id) => NotificationKey::Signal(*id),
            notification_template::Id::SignalName(name) => NotificationKey::Named(name.clone()),
        };
        let frame = Frame::of(
            ty,
            &NotificationTemplate {
                id: Some(id),
                result: Some(result),
            },
        );
        self.store_notification(sh, key, frame, notification_key, true);
    }

    pub(super) fn store_notification(
        &mut self,
        sh: &Arc<Shared>,
        key: InvKey,
        frame: Frame,
        notification_key: NotificationKey,
        push: bool,
    ) {
        let now_ms = self.now_ms;
        let invocation = &mut self.invocations[key.0];
        // A completed invocation takes nothing more, and one that never
        // started takes no signal: restate-server drops both.
        if matches!(
            invocation.status,
            Status::Completed(_) | Status::Inboxed | Status::Scheduled
        ) {
            return;
        }
        let index = invocation.journal.len();
        let encoded = frame.encode();
        if frame.ty == MessageType::RunCompletionNotification
            && let NotificationKey::Completion(completion_id) = &notification_key
        {
            invocation.pending_runs.remove(completion_id);
        }
        invocation.journal.push(Entry {
            frame,
            notification: Some(notification_key.clone()),
        });
        invocation.last_entry_ms = now_ms;
        invocation.retry.failures_since_last_entry = 0;
        let resume = match &mut invocation.status {
            Status::Running(attempt) => {
                // Unpushed (a V7 run result the SDK already holds), the
                // attempt has it only while its input is open for the ack;
                // once the input is closed, it must resume to replay it.
                let delivered = if push {
                    attempt.push(encoded)
                } else {
                    attempt.is_open()
                };
                if !delivered {
                    attempt.unseen_from.get_or_insert(index);
                }
                false
            }
            Status::Suspended(waiting) => waiting.contains(&notification_key),
            _ => false,
        };
        self.touch(key);
        self.delivered(key);
        if resume {
            self.start_attempt(sh, key);
        }
    }

    pub(super) fn fail_notification(
        code: u32,
        message: impl Into<String>,
    ) -> notification_template::Result {
        notification_template::Result::Failure(Failure {
            code,
            message: message.into(),
            metadata: Vec::new(),
        })
    }

    pub(super) fn outcome_result(outcome: &Outcome) -> notification_template::Result {
        match outcome {
            Outcome::Success(bytes) => notification_template::Result::Value(pb::Value {
                content: bytes.clone(),
            }),
            Outcome::Failure(failure) => notification_template::Result::Failure(failure.clone()),
        }
    }

    // ---------------------------------------------------------------------
    // Frames from an attempt
    // ---------------------------------------------------------------------

    /// Apply one frame the SDK wrote on attempt `number` of `key`.
    pub fn on_frame(
        &mut self,
        sh: &Arc<Shared>,
        key: InvKey,
        number: u32,
        frame: Frame,
        received_us: u128,
    ) -> Flow {
        if self.running_attempt(key, number).is_none() {
            return Flow::Stop;
        }
        self.frame_received_us = received_us;
        self.progressed();
        let site = self.crash_site(key, &frame);
        // A random crash's draw is keyed to the frame it would hit, so one
        // seed crashes the same frames however attempts interleave.
        let draw = self
            .ids
            .derive(&[
                b"crash",
                self.invocations[key.0].id.bytes(),
                &number.to_be_bytes(),
                &(site.command_index as u64).to_be_bytes(),
                &site.ty.code().to_be_bytes(),
                &(self.invocations[key.0].journal.len() as u64).to_be_bytes(),
            ])
            .1;
        if self.crash_plan.should_crash(&site, draw) {
            self.crash(sh, key);
            return Flow::Stop;
        }
        match self.apply_frame(sh, key, number, frame) {
            Ok(flow) => flow,
            Err(detail) => {
                self.attempt_failed(
                    sh,
                    key,
                    number,
                    AttemptFailure {
                        code: 571,
                        message: format!("the server could not apply a frame: {detail}"),
                        related_command: None,
                    },
                    None,
                    pb::ErrorBehavior::Retry,
                );
                Flow::Stop
            }
        }
    }

    pub(super) fn crash_site(&self, key: InvKey, frame: &Frame) -> CrashSite {
        let invocation = &self.invocations[key.0];
        let command_index = invocation
            .journal
            .iter()
            .filter(|entry| entry.frame.ty.is_command())
            .count();
        let proposed_run = (frame.ty == MessageType::ProposeRunCompletion)
            .then(|| frame.decode::<ProposeRunCompletionMessage>().ok())
            .flatten()
            .and_then(|proposal| {
                invocation
                    .journal
                    .iter()
                    .filter(|entry| entry.frame.ty.is_command())
                    .enumerate()
                    .find_map(|(index, entry)| {
                        (entry.frame.ty == MessageType::RunCommand)
                            .then(|| entry.frame.decode::<RunCommandMessage>().ok())
                            .flatten()
                            .filter(|run| run.result_completion_id == proposal.result_completion_id)
                            .map(|run| (index, run.name))
                    })
            });
        let run_name = if frame.ty == MessageType::RunCommand {
            frame.decode::<RunCommandMessage>().ok().map(|run| run.name)
        } else {
            proposed_run.as_ref().map(|(_, name)| name.clone())
        };
        let run_index = proposed_run.map(|(index, _)| index);
        CrashSite {
            service: invocation.target.service.clone(),
            handler: invocation.target.handler.clone(),
            ty: frame.ty,
            command_index,
            run_name,
            run_index,
            attempt: invocation.attempts,
        }
    }

    pub(super) fn apply_frame(
        &mut self,
        sh: &Arc<Shared>,
        key: InvKey,
        number: u32,
        frame: Frame,
    ) -> Result<Flow, String> {
        let error = |error: crate::protocol::FrameError| error.to_string();
        match frame.ty {
            MessageType::ProposeRunCompletion => {
                let proposal = frame
                    .decode::<ProposeRunCompletionMessage>()
                    .map_err(error)?;
                let result = match proposal.result {
                    Some(pb::propose_run_completion_message::Result::Value(value)) => {
                        notification_template::Result::Value(pb::Value { content: value })
                    }
                    Some(pb::propose_run_completion_message::Result::Failure(failure)) => {
                        notification_template::Result::Failure(failure)
                    }
                    None => return Err("a run proposal carried no result".into()),
                };
                let completion_id = proposal.result_completion_id;
                let notification = Frame::of(
                    MessageType::RunCompletionNotification,
                    &NotificationTemplate {
                        id: Some(notification_template::Id::CompletionId(completion_id)),
                        result: Some(result),
                    },
                );
                match sh.config.protocol {
                    ProtocolVersion::V6 => self.store_notification(
                        sh,
                        key,
                        notification,
                        NotificationKey::Completion(completion_id),
                        true,
                    ),
                    ProtocolVersion::V7 => {
                        // The SDK holds its own result and waits for the
                        // ack; the stored notification replaces the ack on
                        // replay. The ack itself is not journaled.
                        self.store_notification(
                            sh,
                            key,
                            notification,
                            NotificationKey::Completion(completion_id),
                            false,
                        );
                        if let Some(attempt) = self.running_attempt(key, number) {
                            attempt.push(
                                Frame::of(
                                    MessageType::ProposeRunCompletionAck,
                                    &ProposeRunCompletionAckMessage { completion_id },
                                )
                                .encode(),
                            );
                        }
                        self.delivered(key);
                    }
                }
                Ok(Flow::Continue)
            }
            MessageType::AwaitingOn | MessageType::CommandAck => Ok(Flow::Continue),
            MessageType::Suspension => {
                let waiting = self.decode_suspension(sh.config.protocol, &frame)?;
                self.suspend(sh, key, waiting);
                Ok(Flow::Stop)
            }
            MessageType::End => {
                let outcome = self.invocations[key.0]
                    .output
                    .clone()
                    .unwrap_or(Outcome::Success(Bytes::new()));
                self.complete(sh, key, outcome);
                Ok(Flow::Stop)
            }
            MessageType::Error => {
                let error_message = frame.decode::<ErrorMessage>().map_err(error)?;
                let failure = AttemptFailure {
                    code: error_message.code,
                    message: error_message.message.clone(),
                    related_command: error_message.related_command_name.clone(),
                };
                let behavior = match sh.config.protocol {
                    ProtocolVersion::V7 => pb::ErrorBehavior::try_from(error_message.behavior)
                        .unwrap_or(pb::ErrorBehavior::Retry),
                    ProtocolVersion::V6 => pb::ErrorBehavior::Retry,
                };
                self.attempt_failed(
                    sh,
                    key,
                    number,
                    failure,
                    error_message.next_retry_delay,
                    behavior,
                );
                Ok(Flow::Stop)
            }
            MessageType::Start
            | MessageType::ProposeRunCompletionAck
            | MessageType::InputCommand => Err(format!("the SDK must not send {:?}", frame.ty)),
            ty if ty.is_notification() => Err(format!("the SDK must not send {ty:?}")),
            _ => {
                // restate-server validates a command before storing it; a
                // failed precondition is a transient attempt failure and the
                // command is not journaled.
                if let Err(message) = self.precondition(sh, key, &frame) {
                    self.attempt_failed(
                        sh,
                        key,
                        number,
                        AttemptFailure {
                            code: 500,
                            message,
                            related_command: None,
                        },
                        None,
                        pb::ErrorBehavior::Retry,
                    );
                    return Ok(Flow::Stop);
                }
                self.append_command(key, frame.clone());
                self.apply_command(sh, key, &frame)?;
                Ok(Flow::Continue)
            }
        }
    }

    pub(super) fn decode_suspension(
        &self,
        protocol: ProtocolVersion,
        frame: &Frame,
    ) -> Result<WaitSet, String> {
        let mut waiting = WaitSet::default();
        match protocol {
            ProtocolVersion::V6 => {
                let message = frame
                    .decode::<SuspensionMessageV6>()
                    .map_err(|error| error.to_string())?;
                waiting.keys.extend(
                    message
                        .waiting_completions
                        .into_iter()
                        .map(NotificationKey::Completion),
                );
                waiting.keys.extend(
                    message
                        .waiting_signals
                        .into_iter()
                        .map(NotificationKey::Signal),
                );
                waiting.keys.extend(
                    message
                        .waiting_named_signals
                        .into_iter()
                        .map(NotificationKey::Named),
                );
            }
            ProtocolVersion::V7 => {
                let message = frame
                    .decode::<SuspensionMessage>()
                    .map_err(|error| error.to_string())?;
                fn leaves(future: &pb::Future, waiting: &mut WaitSet) {
                    waiting.keys.extend(
                        future
                            .waiting_completions
                            .iter()
                            .copied()
                            .map(NotificationKey::Completion),
                    );
                    waiting.keys.extend(
                        future
                            .waiting_signals
                            .iter()
                            .copied()
                            .map(NotificationKey::Signal),
                    );
                    waiting.keys.extend(
                        future
                            .waiting_named_signals
                            .iter()
                            .cloned()
                            .map(NotificationKey::Named),
                    );
                    for nested in &future.nested_futures {
                        leaves(nested, waiting);
                    }
                }
                if let Some(future) = message.awaiting_on.as_ref() {
                    leaves(future, &mut waiting);
                }
            }
        }
        Ok(waiting)
    }

    /// The attempt suspended awaiting `waiting`. Resume at once if a
    /// notification it waits for was stored after its input closed.
    pub(super) fn suspend(&mut self, sh: &Arc<Shared>, key: InvKey, waiting: WaitSet) {
        let invocation = &mut self.invocations[key.0];
        let unseen_from = match &invocation.status {
            Status::Running(attempt) => attempt.unseen_from,
            _ => None,
        };
        let already_there = unseen_from.is_some_and(|from| {
            invocation.journal[from..].iter().any(|entry| {
                entry
                    .notification
                    .as_ref()
                    .is_some_and(|notification| waiting.contains(notification))
            })
        });
        invocation.status = Status::Suspended(waiting);
        invocation.suspensions += 1;
        // The invoker starts a fresh retry loop on every invocation start,
        // a resume from suspension included.
        invocation.retry.failures_in_loop = 0;
        self.stats.suspensions += 1;
        self.touch(key);
        if already_there {
            self.start_attempt(sh, key);
        }
        sh.activity.notify_waiters();
    }

    /// Register `waiter` for `key`'s completion, answering at once if it
    /// already completed.
    pub fn add_waiter(&mut self, sh: &Arc<Shared>, key: InvKey, waiter: Waiter) {
        if let Waiter::Ingress {
            ticket: Some(ticket),
            ..
        } = &waiter
        {
            self.ingress_awaits(*ticket, key);
        }
        if let Status::Completed(outcome) = &self.invocations[key.0].status {
            let outcome = outcome.clone();
            self.answer_waiter(sh, waiter, &outcome);
        } else {
            self.invocations[key.0].waiters.push(waiter);
        }
    }

    pub(super) fn answer_waiter(&mut self, sh: &Arc<Shared>, waiter: Waiter, outcome: &Outcome) {
        match waiter {
            Waiter::Call {
                caller,
                completion_id,
            } => self.notify(
                sh,
                caller,
                MessageType::CallCompletionNotification,
                notification_template::Id::CompletionId(completion_id),
                Self::outcome_result(outcome),
            ),
            Waiter::Attach {
                caller,
                completion_id,
            } => self.notify(
                sh,
                caller,
                MessageType::AttachInvocationCompletionNotification,
                notification_template::Id::CompletionId(completion_id),
                Self::outcome_result(outcome),
            ),
            Waiter::Ingress { sender, ticket } => {
                let _ = sender.send(outcome.clone());
                // Answered: the handler that issued it waits on the turn
                // from here on, not on the server.
                if let Some(ticket) = ticket {
                    self.ingress_ended(ticket);
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // Completion, failure and retry
    // ---------------------------------------------------------------------

    /// End `key` with `outcome`: answer every waiter and hand its lock on.
    pub fn complete(&mut self, sh: &Arc<Shared>, key: InvKey, outcome: Outcome) {
        let invocation = &mut self.invocations[key.0];
        if invocation.status.is_completed() {
            return;
        }
        if let Status::Running(attempt) = &mut invocation.status
            && let Some(abort) = attempt.abort.take()
        {
            // The attempt reported its own end; its task stops by itself.
            drop(abort);
        }
        invocation.status = Status::Completed(outcome.clone());
        let waiters = std::mem::take(&mut invocation.waiters);
        self.touch(key);
        for waiter in waiters {
            self.answer_waiter(sh, waiter, &outcome);
        }
        self.remove_timers_of(key);
        self.release_lock(sh, key);
        sh.activity.notify_waiters();
    }

    /// Attempt `number` of `key` failed. Retry it on the handler's policy, or
    /// pause or fail it once the policy is exhausted.
    pub fn attempt_failed(
        &mut self,
        sh: &Arc<Shared>,
        key: InvKey,
        number: u32,
        failure: AttemptFailure,
        next_retry_delay_ms: Option<u64>,
        behavior: pb::ErrorBehavior,
    ) {
        if self.running_attempt(key, number).is_none() {
            return;
        }
        self.close_input(key);
        let policy = sh.config.retry_policy(&self.invocations[key.0].spec);
        let invocation = &mut self.invocations[key.0];
        invocation.retry.failures_since_last_entry += 1;
        // An SDK-chosen delay (a `ctx.run` retry policy) overrides the
        // invoker policy for that one retry without consuming one of its
        // attempts.
        if next_retry_delay_ms.is_none() {
            invocation.retry.failures_in_loop += 1;
        }
        invocation.retry.last_failure = Some(failure.clone());
        let failures = invocation.retry.failures_in_loop;
        match behavior {
            pb::ErrorBehavior::Fail => {
                self.complete(sh, key, Outcome::failure(failure.code, failure.message));
                return;
            }
            pb::ErrorBehavior::Pause => {
                self.pause(sh, key);
                return;
            }
            pb::ErrorBehavior::Retry => {}
        }
        if next_retry_delay_ms.is_none()
            && policy
                .max_attempts
                .is_some_and(|max_attempts| failures >= max_attempts)
        {
            match policy.on_max_attempts {
                OnMaxAttempts::Pause => self.pause(sh, key),
                OnMaxAttempts::Kill => {
                    self.complete(sh, key, Outcome::failure(failure.code, failure.message));
                }
            }
            return;
        }
        let delay_ms = next_retry_delay_ms.unwrap_or_else(|| duration_ms(policy.delay(failures)));
        self.stats.retries += 1;
        self.invocations[key.0].status = Status::BackingOff;
        self.touch(key);
        if delay_ms == 0 {
            self.start_attempt(sh, key);
        } else {
            self.add_timer(
                self.now_ms.saturating_add(delay_ms),
                TimerAction::Retry { invocation: key },
            );
            sh.activity.notify_waiters();
        }
    }

    pub(super) fn pause(&mut self, sh: &Arc<Shared>, key: InvKey) {
        self.invocations[key.0].status = Status::Paused;
        self.touch(key);
        self.remove_retry_timer(key);
        sh.activity.notify_waiters();
    }

    /// The attempt's stream ended without a terminal message: the endpoint
    /// dropped the invocation, which the invoker retries.
    pub fn stream_ended(&mut self, sh: &Arc<Shared>, key: InvKey, number: u32, detail: String) {
        self.attempt_failed(
            sh,
            key,
            number,
            AttemptFailure {
                code: 500,
                message: detail,
                related_command: None,
            },
            None,
            pb::ErrorBehavior::Retry,
        );
    }

    /// Operator resume of a paused invocation: a fresh retry loop.
    pub fn resume(&mut self, sh: &Arc<Shared>, key: InvKey) -> bool {
        if !matches!(self.invocations[key.0].status, Status::Paused) {
            return false;
        }
        self.invocations[key.0].retry.failures_in_loop = 0;
        self.start_attempt(sh, key);
        true
    }

    /// Drop the running attempt mid-step, as a deployment crash does, and
    /// replay it on a new attempt.
    pub fn crash(&mut self, sh: &Arc<Shared>, key: InvKey) -> bool {
        let Status::Running(attempt) = &mut self.invocations[key.0].status else {
            return false;
        };
        if let Some(abort) = attempt.abort.take() {
            abort.abort();
        }
        let invocation = &mut self.invocations[key.0];
        invocation.retry.failures_since_last_entry += 1;
        invocation.retry.last_failure = Some(AttemptFailure {
            code: 500,
            message: "the deployment crashed (simulated)".into(),
            related_command: None,
        });
        self.stats.crashes += 1;
        invocation.status = Status::BackingOff;
        self.start_attempt(sh, key);
        true
    }

    /// Cancel `key` the way Restate (protocol V4+) does: an invocation that
    /// never started completes as `409 canceled` at once; a started one gets
    /// the cancel signal and unwinds through its own handler, whose SDK
    /// cancels the calls it is waiting on. A paused one resumes to do so.
    pub fn cancel(&mut self, sh: &Arc<Shared>, key: InvKey) -> ControlResult {
        match &self.invocations[key.0].status {
            Status::Completed(_) => ControlResult::AlreadyCompleted,
            Status::Scheduled | Status::Inboxed => {
                self.remove_from_inbox(key);
                self.remove_timers_of(key);
                self.complete(sh, key, Outcome::failure(409, "canceled"));
                ControlResult::Done
            }
            Status::Paused => {
                self.store_signal(sh, key, CANCEL_SIGNAL_ID);
                self.resume(sh, key);
                ControlResult::Appended
            }
            _ => {
                self.store_signal(sh, key, CANCEL_SIGNAL_ID);
                ControlResult::Appended
            }
        }
    }

    pub(super) fn store_signal(&mut self, sh: &Arc<Shared>, key: InvKey, signal_id: u32) {
        self.notify(
            sh,
            key,
            MessageType::SignalNotification,
            notification_template::Id::SignalId(signal_id),
            notification_template::Result::Void(pb::Void {}),
        );
    }

    /// Kill `key`: stop it where it stands without consulting the SDK, end
    /// it as `409 killed`, and kill every invocation its journal called or
    /// sent — Restate's V4+ kill cascade.
    pub fn kill(&mut self, sh: &Arc<Shared>, key: InvKey) -> ControlResult {
        if self.invocations[key.0].status.is_completed() {
            return ControlResult::AlreadyCompleted;
        }
        if let Status::Running(attempt) = &mut self.invocations[key.0].status
            && let Some(abort) = attempt.abort.take()
        {
            abort.abort();
        }
        self.remove_from_inbox(key);
        self.remove_timers_of(key);
        let children = self.invocations[key.0].children.clone();
        self.complete(sh, key, Outcome::failure(409, "killed"));
        for child in children {
            self.kill(sh, child);
        }
        ControlResult::Done
    }

    /// Purge a completed `key` as the admin API does: its journal and id are
    /// forgotten, and so are a workflow run's key state and promises, so a
    /// later submission of the same workflow key starts a fresh invocation
    /// with an empty journal, under the same id (a workflow's id derives from
    /// its key). Returns `false` for an invocation that has not completed:
    /// Restate purges only completed invocations.
    pub fn purge(&mut self, key: InvKey) -> bool {
        let invocation = &self.invocations[key.0];
        if !invocation.status.is_completed() {
            return false;
        }
        let id = invocation.id.as_str().to_owned();
        if invocation.spec.kind == HandlerKind::WorkflowRun
            && let Some(service_key) = invocation.target.service_key()
            && let Some(record) = self.keys.get_mut(&service_key)
            && record.workflow_run == Some(key)
        {
            record.workflow_run = None;
            record.state.clear();
            record.promises.clear();
        }
        self.idempotency.retain(|_, named| *named != key);
        if self.by_id.get(&id) == Some(&key) {
            self.by_id.remove(&id);
        }
        self.invocations[key.0].journal.clear();
        self.touch(key);
        true
    }

    /// Whether `key` is still retained: an invocation stays addressable by
    /// its id until it is purged.
    pub fn is_retained(&self, key: InvKey) -> bool {
        self.by_id.get(self.invocations[key.0].id.as_str()) == Some(&key)
    }

    pub(super) fn remove_from_inbox(&mut self, key: InvKey) {
        if let Some(service_key) = self.invocations[key.0].target.service_key()
            && let Some(record) = self.keys.get_mut(&service_key)
        {
            record.inbox.retain(|queued| *queued != key);
        }
    }

    // ---------------------------------------------------------------------
    // Introspection
    // ---------------------------------------------------------------------

    /// Whether nothing can happen without time moving or outside input:
    /// every live attempt is blocked reading its input.
    ///
    /// An attempt counts as blocked only when its SDK waits on its input,
    /// the server has applied everything it wrote (its response body is
    /// idle), and no `ctx.run` closure of it is executing — a handler may
    /// poll its input beside a running closure (`select!`), and that closure
    /// is work in progress.
    pub fn is_quiescent(&self) -> bool {
        self.invocations
            .iter()
            .all(|invocation| match &invocation.status {
                Status::Running(attempt) => {
                    attempt.is_open()
                        && attempt.probe.is_idle()
                        && !attempt.has_held_work()
                        && invocation.pending_runs.is_empty()
                }
                _ => true,
            })
            && !self.serial_pending()
    }

    pub fn timers(&self) -> Vec<TimerView> {
        self.timers
            .iter()
            .map(|(&(fire_at_ms, _, _), action)| {
                let (invocation, kind) = match action {
                    TimerAction::Sleep { invocation, .. } => (*invocation, "sleep"),
                    TimerAction::Start { invocation } => (*invocation, "delayed-start"),
                    TimerAction::Retry { invocation } => (*invocation, "retry"),
                };
                let invocation = &self.invocations[invocation.0];
                TimerView {
                    fire_at_ms,
                    invocation: invocation.id.as_str().to_owned(),
                    target: invocation.target.display(),
                    kind,
                }
            })
            .collect()
    }
}

/// What an attach or get-output command names.
#[derive(Clone, Debug)]
pub enum AttachTarget {
    Invocation(String),
    Workflow {
        name: String,
        key: String,
    },
    Idempotent {
        service: String,
        key: Option<String>,
        handler: String,
        idempotency_key: String,
    },
}

impl From<pb::attach_invocation_command_message::Target> for AttachTarget {
    fn from(target: pb::attach_invocation_command_message::Target) -> Self {
        use pb::attach_invocation_command_message::Target;
        match target {
            Target::InvocationId(id) => Self::Invocation(id),
            Target::WorkflowTarget(workflow) => Self::Workflow {
                name: workflow.workflow_name,
                key: workflow.workflow_key,
            },
            Target::IdempotentRequestTarget(request) => Self::Idempotent {
                service: request.service_name,
                key: request.service_key,
                handler: request.handler_name,
                idempotency_key: request.idempotency_key,
            },
        }
    }
}

impl From<pb::get_invocation_output_command_message::Target> for AttachTarget {
    fn from(target: pb::get_invocation_output_command_message::Target) -> Self {
        use pb::get_invocation_output_command_message::Target;
        match target {
            Target::InvocationId(id) => Self::Invocation(id),
            Target::WorkflowTarget(workflow) => Self::Workflow {
                name: workflow.workflow_name,
                key: workflow.workflow_key,
            },
            Target::IdempotentRequestTarget(request) => Self::Idempotent {
                service: request.service_name,
                key: request.service_key,
                handler: request.handler_name,
                idempotency_key: request.idempotency_key,
            },
        }
    }
}
