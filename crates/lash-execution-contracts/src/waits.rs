use crate::ProcessOutcome;
use lash_core_effect::Resolution;
use lash_durable::DurableInstant;
use lash_durable::domain::{ScopeKey, WaitPurpose};
use lash_durable::domain::{WaitId, WaitKind};
use std::time::Duration;
/// A host-resolvable wait's completion key: its wait id's 32 lowercase hex
/// digits. The id is 128 random bits, so the key is unguessable, and whoever
/// holds it may resolve the wait: it is a bearer capability, and the host
/// hands it only to callers it has authorized. It carries no scope or kind.
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedKey(String);

impl PinnedKey {
    /// A key as a host stored it.
    #[must_use]
    pub fn new(key: String) -> Self {
        Self(key)
    }

    /// The key of wait `id`.
    #[must_use]
    pub fn of(id: &WaitId) -> Self {
        Self(id.to_hex())
    }

    /// The wait this key names; `None` when it is not a key's spelling.
    #[must_use]
    pub fn wait(&self) -> Option<WaitId> {
        WaitId::parse_hex(&self.0)
    }

    /// The key a host is handed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PinnedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PinnedKey(..)")
    }
}

/// A wait's deadline: written once, at minting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WaitDeadline(DurableInstant);

impl WaitDeadline {
    /// The deadline of a wait of `wait` minted at `now`: the bound its
    /// requester set. Lash holds no wait default or ceiling; a deadline past
    /// the last durable instant is that instant. A nested wait keeps its own
    /// deadline.
    #[must_use]
    pub fn after(wait: Duration, now: DurableInstant) -> Self {
        let millis = i64::try_from(wait.as_millis()).unwrap_or(i64::MAX);
        Self(DurableInstant(now.0.saturating_add(millis)))
    }

    /// The deadline at the stored instant `at`: a deadline read back from
    /// its row, or an absolute one its caller recorded before.
    #[must_use]
    pub fn at_instant(at: DurableInstant) -> Self {
        Self(at)
    }

    /// The deadline.
    #[must_use]
    pub fn at(self) -> DurableInstant {
        self.0
    }
}

/// What a call's admission pinned for its park: the deadline its
/// [`ParkBound`](lash_vocabulary::tool_contract::ParkBound) set, or none for a park that lasts until
/// its scope ends. Computed once, at admission, and recorded once, as the
/// deadline of the completion wait the admission pins: a crash or takeover
/// reads it back from that wait's row, never computes it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParkDeadline {
    /// The park times out at this deadline.
    At(WaitDeadline),
    /// The park has no deadline: the end of its turn or process scope
    /// revokes it.
    UntilScopeEnd,
}

impl ParkDeadline {
    /// What a park bounded by `bound` and admitted at `now` pins.
    #[must_use]
    pub fn admitted(bound: lash_vocabulary::tool_contract::ParkBound, now: DurableInstant) -> Self {
        match bound {
            lash_vocabulary::tool_contract::ParkBound::Within(wait) => {
                Self::At(WaitDeadline::after(wait, now))
            }
            lash_vocabulary::tool_contract::ParkBound::UntilScopeEnd => Self::UntilScopeEnd,
        }
    }

    /// The deadline the park's waits are pinned under, if it has one.
    #[must_use]
    pub fn deadline(self) -> Option<WaitDeadline> {
        match self {
            Self::At(deadline) => Some(deadline),
            Self::UntilScopeEnd => None,
        }
    }
}

/// What to pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitSpec {
    /// The scope that revokes it.
    pub scope: ScopeKey,
    /// What it waits for, with its deadline: written once, with the row.
    pub purpose: WaitPurpose,
}

/// A pinned wait, as its owner holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WaitRef {
    id: WaitId,
    kind: WaitKind,
}

impl WaitRef {
    /// The wait `id` of `kind`.
    #[must_use]
    pub fn new(id: WaitId, kind: WaitKind) -> Self {
        Self { id, kind }
    }

    /// Its identity.
    #[must_use]
    pub fn id(&self) -> WaitId {
        self.id
    }

    /// What it waits for.
    #[must_use]
    pub fn kind(&self) -> WaitKind {
        self.kind
    }
}

/// Which of a race's waits won.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaceWinner {
    /// The wait resolved.
    Resolved {
        /// The wait.
        wait: WaitRef,
        /// Its resolution.
        resolution: Resolution,
    },
    /// The wait's deadline passed first.
    TimedOut(WaitRef),
    /// The awaiter's own cancel mail came first, or the scope that owns the
    /// wait ended and revoked it.
    Cancelled,
}

/// How an [`await_external`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalWaitOutcome {
    /// The host resolved the key.
    Resolved(Resolution),
    /// The deadline passed first.
    TimedOut,
    /// The awaiter was cancelled first.
    Cancelled,
}

/// How an [`await_process`] ended.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessWaitOutcome {
    /// The process ended.
    Resolved(ProcessOutcome),
    /// The deadline passed first.
    TimedOut,
    /// The awaiter was cancelled first.
    Cancelled,
}

/// How a settled wait row ended, decoded: what an activation that reads
/// its waits' rows itself hands to its own logic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitSettled {
    /// Resolved: a timer with `Ok(null)`, a process-terminal wait with the
    /// outcome [`process_outcome`] decodes.
    Resolved(Resolution),
    /// Its deadline passed first, in a committed `Due`.
    TimedOut,
    /// Its scope ended first.
    Revoked,
}
