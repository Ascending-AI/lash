//! Crash simulation: drop a handler mid-step, before the server stores the
//! frame it was about to send, and replay the invocation on a new attempt.
//!
//! A crash loses exactly what a deployment crash loses: every frame the
//! server had not yet applied. The interesting point is
//! [`CrashPoint::BeforeRunResult`] — the `ctx.run` closure already ran, its
//! result never became durable, and the replay runs it again.

use super::ids::SeededIds;
use crate::protocol::MessageType;

/// Where in an attempt a crash strikes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// Before the server stores the result of a `ctx.run` — the one named
    /// `name`, or any run when `None`.
    BeforeRunResult { name: Option<String> },
    /// Before the server stores the command with this 0-based journal
    /// command index (the input command is index 0).
    BeforeCommand { index: usize },
    /// Before the server applies any frame of this message type.
    BeforeFrame { ty: MessageType },
}

/// One scripted crash: a point, the handler it applies to, and how many
/// times it may fire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashRule {
    pub point: CrashPoint,
    pub service: Option<String>,
    pub handler: Option<String>,
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
    pub ty: MessageType,
    /// The journal index this frame takes if it is a command.
    pub command_index: usize,
    /// For a run proposal, the name of the run it completes.
    pub run_name: Option<String>,
    pub attempt: u32,
}

#[derive(Clone, Debug, Default)]
pub struct CrashPlan {
    rules: Vec<CrashRule>,
    random: Option<RandomCrashes>,
}

impl CrashPlan {
    pub fn add(&mut self, rule: CrashRule) {
        self.rules.push(rule);
    }

    pub fn set_random(&mut self, random: Option<RandomCrashes>) {
        self.random = random;
    }

    pub fn clear(&mut self) {
        self.rules.clear();
        self.random = None;
    }

    /// Whether the attempt dies before the server applies `site`'s frame.
    /// Terminal frames never crash: the SDK has already let go.
    pub fn should_crash(&mut self, site: &CrashSite, ids: &mut SeededIds) -> bool {
        if matches!(
            site.ty,
            MessageType::End | MessageType::Suspension | MessageType::Error
        ) {
            return false;
        }
        if let Some(rule) = self.rules.iter_mut().find(|rule| rule.matches(site)) {
            rule.times -= 1;
            return true;
        }
        if let Some(random) = &mut self.random
            && random.budget > 0
            && ids.below(1000) < u64::from(random.per_mille)
        {
            random.budget -= 1;
            return true;
        }
        false
    }
}
