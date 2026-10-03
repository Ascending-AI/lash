//! Crash simulation: drop a handler mid-step, before the server stores the
//! frame it was about to send, and replay the invocation on a new attempt.
//!
//! A crash loses exactly what a deployment crash loses: every frame the
//! server had not yet applied. The interesting point is
//! [`CrashPoint::BeforeRunResult`] — the `ctx.run` closure already ran, its
//! result never became durable, and the replay runs it again.

use std::sync::Arc;

use crate::protocol::MessageType;

use super::CrashListener;

/// Where in an attempt a crash strikes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// Before the server stores the result of a `ctx.run` — the one named
    /// `name`, or any run when `None`.
    BeforeRunResult { name: Option<String> },
    /// Before the server stores the result of the `ctx.run` whose command
    /// has this 0-based journal command index: the point for a run whose name
    /// differs from one execution to the next (it embeds a fresh id).
    BeforeRunResultAt { index: usize },
    /// Before the server stores the result of a `ctx.run` whose name ends
    /// with `suffix`: the point for a run whose name embeds an id the
    /// scenario only learns as it runs (a live server's keys are unique to
    /// each run), such as a tool call's `lash:<call id>:present`.
    BeforeRunResultEnding { suffix: String },
    /// Before the server stores the `ctx.run` command named `name`: the run
    /// never reaches the journal, and the replay issues it anew.
    BeforeRun { name: String },
    /// Before a journal run whose generated name ends with this suffix.
    BeforeRunEnding { suffix: String },
    /// Before the server stores the command with this 0-based journal
    /// command index (the input command is index 0).
    BeforeCommand { index: usize },
    /// Before the server applies a frame of this type, or delivers a V7 run ACK.
    BeforeFrame { ty: MessageType },
}

/// One scripted crash: a point, the handler it applies to, and how many
/// times it may fire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashRule {
    pub point: CrashPoint,
    pub service: Option<String>,
    pub handler: Option<String>,
    /// The object or workflow key the crashing invocation addresses; `None`
    /// matches any key.
    pub key: Option<String>,
    /// A suffix the key must end with: the point for a key that embeds an
    /// id the scenario only learns as it runs, such as a process segment's
    /// `<process id>#<ordinal>`.
    pub key_suffix: Option<String>,
    /// Fire only on attempts numbered at most this (1-based); `None` fires
    /// on any attempt.
    pub max_attempt: Option<u32>,
    pub times: u32,
}

impl CrashRule {
    pub fn new(point: CrashPoint) -> Self {
        Self {
            point,
            service: None,
            handler: None,
            key: None,
            key_suffix: None,
            max_attempt: None,
            times: 1,
        }
    }

    pub fn service(mut self, service: impl Into<String>) -> Self {
        self.service = Some(service.into());
        self
    }

    pub fn handler(mut self, handler: impl Into<String>) -> Self {
        self.handler = Some(handler.into());
        self
    }

    /// Crash only an invocation addressing this object or workflow key.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Crash only an invocation whose key ends with `suffix`.
    pub fn key_ending(mut self, suffix: impl Into<String>) -> Self {
        self.key_suffix = Some(suffix.into());
        self
    }

    pub fn times(mut self, times: u32) -> Self {
        self.times = times;
        self
    }

    /// Crash only while the invocation is on its first `attempts` attempts.
    pub fn within_attempts(mut self, attempts: u32) -> Self {
        self.max_attempt = Some(attempts);
        self
    }

    fn matches(&self, site: &CrashSite) -> bool {
        if self.times == 0 {
            return false;
        }
        if self
            .service
            .as_deref()
            .is_some_and(|service| service != site.service)
            || self
                .handler
                .as_deref()
                .is_some_and(|handler| handler != site.handler)
            || self
                .key
                .as_deref()
                .is_some_and(|key| Some(key) != site.key.as_deref())
            || self
                .key_suffix
                .as_deref()
                .is_some_and(|suffix| !site.key.as_deref().is_some_and(|key| key.ends_with(suffix)))
            || self.max_attempt.is_some_and(|max| site.attempt > max)
        {
            return false;
        }
        match &self.point {
            CrashPoint::BeforeRunResult { name } => {
                site.ty == MessageType::ProposeRunCompletion
                    && name
                        .as_deref()
                        .is_none_or(|name| site.run_name.as_deref() == Some(name))
            }
            CrashPoint::BeforeRunResultAt { index } => {
                site.ty == MessageType::ProposeRunCompletion && site.run_index == Some(*index)
            }
            CrashPoint::BeforeRunResultEnding { suffix } => {
                site.ty == MessageType::ProposeRunCompletion
                    && site
                        .run_name
                        .as_deref()
                        .is_some_and(|name| name.ends_with(suffix.as_str()))
            }
            CrashPoint::BeforeRun { name } => {
                site.ty == MessageType::RunCommand && site.run_name.as_deref() == Some(name)
            }
            CrashPoint::BeforeRunEnding { suffix } => {
                site.ty == MessageType::RunCommand
                    && site
                        .run_name
                        .as_deref()
                        .is_some_and(|name| name.ends_with(suffix))
            }
            CrashPoint::BeforeCommand { index } => {
                site.ty.is_command() && site.command_index == *index
            }
            CrashPoint::BeforeFrame { ty } => site.ty == *ty,
        }
    }
}

/// Seeded random crashes: every frame an attempt sends is a crash candidate
/// with probability `per_mille / 1000`, up to `budget` crashes in total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomCrashes {
    pub per_mille: u16,
    pub budget: u32,
}

/// The frame a crash decision is about.
#[derive(Clone, Debug)]
pub struct CrashSite {
    pub service: String,
    pub handler: String,
    pub key: Option<String>,
    pub ty: MessageType,
    /// The journal index this frame takes if it is a command.
    pub command_index: usize,
    /// For a run command, its name; for a run proposal, the name of the run
    /// it completes.
    pub run_name: Option<String>,
    /// For a run proposal, the journal command index of the run it completes.
    pub run_index: Option<usize>,
    pub attempt: u32,
}

#[derive(Clone, Debug, Default)]
pub struct CrashPlan {
    rules: Vec<CrashRule>,
    random: Option<RandomCrashes>,
    /// Scripted cancellations: a rule's point names the frame before which
    /// the server cancels the invocation instead of dropping its attempt.
    cancels: Vec<CrashRule>,
}

impl CrashPlan {
    pub fn add(&mut self, rule: CrashRule) {
        self.rules.push(rule);
    }

    pub fn add_cancel(&mut self, rule: CrashRule) {
        self.cancels.push(rule);
    }

    /// Whether the server cancels the invocation before it applies `site`'s
    /// frame. The attempt lives on: the frame is applied after the cancel
    /// signal, so the handler meets the cancellation ahead of that frame's
    /// answer.
    pub fn should_cancel(&mut self, site: &CrashSite) -> bool {
        if let Some(rule) = self.cancels.iter_mut().find(|rule| rule.matches(site)) {
            rule.times -= 1;
            return true;
        }
        false
    }

    pub fn set_random(&mut self, random: Option<RandomCrashes>) {
        self.random = random;
    }

    pub fn clear(&mut self) {
        self.rules.clear();
        self.random = None;
        self.cancels.clear();
    }

    /// Whether the attempt dies before the server applies `site`'s frame.
    /// Terminal frames never crash: the SDK has already let go.
    pub fn should_crash(&mut self, site: &CrashSite, draw: u64) -> bool {
        if matches!(
            site.ty,
            MessageType::End | MessageType::Suspension | MessageType::Error
        ) {
            return false;
        }
        if self.should_crash_scripted(site) {
            return true;
        }
        if let Some(random) = &mut self.random
            && random.budget > 0
            && draw % 1000 < u64::from(random.per_mille)
        {
            random.budget -= 1;
            return true;
        }
        false
    }

    /// Explicit rules also cut ACK delivery without adding a random crash site.
    pub(super) fn should_crash_scripted(&mut self, site: &CrashSite) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|rule| rule.matches(site)) {
            rule.times -= 1;
            return true;
        }
        false
    }
}

/// A count of the attempts an `on_crash` listener reports crashed, readable
/// and awaitable from the test task while the listener runs under the
/// server's lock.
///
/// `RestateTestServer::on_crash` — and the live backend's — takes one
/// listener and refuses a second. A law that waits on, counts or reacts to
/// crashes builds its listener here and reads this side back, instead of
/// wiring its own counter, channel or flag for the same event.
#[derive(Clone)]
pub struct CrashCount {
    crashes: tokio::sync::watch::Sender<u64>,
    observed: tokio::sync::watch::Receiver<u64>,
}

impl CrashCount {
    /// A count of zero. It moves once a listener built from it is
    /// registered and a crash drops an attempt.
    pub fn new() -> Self {
        let (crashes, observed) = tokio::sync::watch::channel(0);
        Self { crashes, observed }
    }

    /// The listener that counts each crash — the `on_crash` argument.
    pub fn listener(&self) -> CrashListener {
        self.listener_with(|_| ())
    }

    /// The listener that runs `hook` with the crashed invocation's target
    /// and counts the crash once `hook` returns, so [`wait_until`] observes
    /// a crash only once its hook ran. The hook's bound is the listener's:
    /// it runs under the server's lock, so it may act on the store or a
    /// worker slot but must never call back into the server.
    ///
    /// [`wait_until`]: Self::wait_until
    pub fn listener_with(&self, hook: impl Fn(&str) + Send + Sync + 'static) -> CrashListener {
        let crashes = self.crashes.clone();
        Arc::new(move |target: &str| {
            hook(target);
            crashes.send_modify(|count| *count += 1);
        })
    }

    /// The crashes counted so far.
    pub fn get(&self) -> u64 {
        *self.observed.borrow()
    }

    /// Resolve once at least `at_least` crashes landed, with the count
    /// then. `Err` only when the count can no longer move — every listener
    /// and reader of it is gone.
    pub async fn wait_until(
        &mut self,
        at_least: u64,
    ) -> Result<u64, tokio::sync::watch::error::RecvError> {
        self.observed
            .wait_for(|count| *count >= at_least)
            .await
            .map(|count| *count)
    }
}

impl Default for CrashCount {
    fn default() -> Self {
        Self::new()
    }
}
