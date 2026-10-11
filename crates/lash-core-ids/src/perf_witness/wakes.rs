//! Opt-in counts of what brought a session's owner to its work (FIG-5932):
//! the claim that took the session, and each wait an owner ended before its
//! next pass, by whether a wake hint or the claim poll ended it. A pass that
//! found work after the poll ended its wait ran on mail no hint reached: the
//! fallback admitted it. Counters are process-wide and only grow; a reader
//! takes the difference of two [`counts`].

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

static CLAIMS_ON_HINT: AtomicU64 = AtomicU64::new(0);
static CLAIMS_ON_POLL: AtomicU64 = AtomicU64::new(0);
static WORK_ON_HINT: AtomicU64 = AtomicU64::new(0);
static WORK_ON_POLL: AtomicU64 = AtomicU64::new(0);
static IDLE_ON_HINT: AtomicU64 = AtomicU64::new(0);
static IDLE_ON_POLL: AtomicU64 = AtomicU64::new(0);

/// The counters at one instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Sessions claimed by a claim a hint rang.
    pub claims_on_hint: u64,
    /// Sessions claimed by a claim the claim poll ran.
    pub claims_on_poll: u64,
    /// Passes that found work after a hint ended the owner's wait.
    pub work_on_hint: u64,
    /// Passes that found work after the claim poll ended the owner's wait.
    pub work_on_poll: u64,
    /// Passes that found nothing after a hint ended the owner's wait.
    pub idle_on_hint: u64,
    /// Passes that found nothing after the claim poll ended the owner's wait.
    pub idle_on_poll: u64,
}

impl Counts {
    /// What was counted between `earlier` and `self`.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            claims_on_hint: self.claims_on_hint.saturating_sub(earlier.claims_on_hint),
            claims_on_poll: self.claims_on_poll.saturating_sub(earlier.claims_on_poll),
            work_on_hint: self.work_on_hint.saturating_sub(earlier.work_on_hint),
            work_on_poll: self.work_on_poll.saturating_sub(earlier.work_on_poll),
            idle_on_hint: self.idle_on_hint.saturating_sub(earlier.idle_on_hint),
            idle_on_poll: self.idle_on_poll.saturating_sub(earlier.idle_on_poll),
        }
    }
}

/// Count one claim of a session, rung by a hint or run by the claim poll.
pub fn record_claim(on_hint: bool) {
    let counter = if on_hint {
        &CLAIMS_ON_HINT
    } else {
        &CLAIMS_ON_POLL
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Count one owner pass after a wait a hint or the claim poll ended, by
/// whether it found work.
pub fn record_pass(on_hint: bool, found_work: bool) {
    let counter = match (on_hint, found_work) {
        (true, true) => &WORK_ON_HINT,
        (false, true) => &WORK_ON_POLL,
        (true, false) => &IDLE_ON_HINT,
        (false, false) => &IDLE_ON_POLL,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The counters now.
pub fn counts() -> Counts {
    Counts {
        claims_on_hint: CLAIMS_ON_HINT.load(Ordering::Relaxed),
        claims_on_poll: CLAIMS_ON_POLL.load(Ordering::Relaxed),
        work_on_hint: WORK_ON_HINT.load(Ordering::Relaxed),
        work_on_poll: WORK_ON_POLL.load(Ordering::Relaxed),
        idle_on_hint: IDLE_ON_HINT.load(Ordering::Relaxed),
        idle_on_poll: IDLE_ON_POLL.load(Ordering::Relaxed),
    }
}
