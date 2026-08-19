//! What an automatic queued-turn drain returns to a host.
//!
//! A drain that ran no turn is not self-explanatory, so the empty arm
//! carries the reason the runtime already computed instead of a bare
//! `None` the host has to guess at.

use super::QueuedWorkClaimRefusal;

/// Why a queued-turn drain executed no turn.
///
/// A drain that ran nothing is not self-explanatory: "the queue is exhausted"
/// and "this drain never reached the queue" demand opposite responses, and only
/// this reason distinguishes them. Match it exhaustively — a new variant is a
/// new host decision, not a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmptyQueuedDrainReason {
    /// Another execution holds the session execution lane, so this drain never
    /// looked at the queue. Nothing was consumed; the work is retryable.
    ExecutionLaneBusy,
    /// The session has no durable store, so there is no queue to drain.
    NoDurableQueue,
    /// The queue was reachable and the claim was refused for the named reason.
    /// [`QueuedWorkClaimRefusal::Empty`] is the only variant that proves the
    /// durable queue holds no pending work for this lane;
    /// [`QueuedWorkClaimRefusal::NotYetAvailable`] means work exists but is not
    /// yet claimable, so the drain is worth repeating.
    ClaimRefused(QueuedWorkClaimRefusal),
}

impl EmptyQueuedDrainReason {
    /// The stable snake_case spelling, for host logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionLaneBusy => "execution_lane_busy",
            Self::NoDurableQueue => "no_durable_queue",
            Self::ClaimRefused(refusal) => refusal.as_str(),
        }
    }
}

/// One automatic queued-turn drain: the turn it ran, or why it ran none.
#[derive(Clone, Debug)]
pub enum QueuedTurnDrain<T> {
    /// The drain claimed queued work and ran a turn.
    Ran(T),
    /// The drain ran no turn, for the named reason.
    Empty(EmptyQueuedDrainReason),
}

impl<T> QueuedTurnDrain<T> {
    /// The turn this drain ran, discarding why it ran none.
    pub fn ran(self) -> Option<T> {
        match self {
            Self::Ran(turn) => Some(turn),
            Self::Empty(_) => None,
        }
    }

    /// Transforms the turn, preserving the empty reason.
    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> U) -> QueuedTurnDrain<U> {
        match self {
            Self::Ran(turn) => QueuedTurnDrain::Ran(f(turn)),
            Self::Empty(reason) => QueuedTurnDrain::Empty(reason),
        }
    }

    /// Returns the turn this drain ran, or panics with `message`.
    #[track_caller]
    pub fn expect(self, message: &str) -> T {
        match self {
            Self::Ran(turn) => turn,
            Self::Empty(reason) => {
                panic!("{message}: queued drain ran no turn ({})", reason.as_str())
            }
        }
    }
}
