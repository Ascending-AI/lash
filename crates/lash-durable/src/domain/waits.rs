//! Wait rows (L5): keyed promises, durable waits and timers
//! (ADR 0132 §6).
//!
//! A wait's deadline is written once, at minting. The first resolution wins.
//! Lock order is the wait row, then the actor row.

use crate::ids::{ActorKey, DurableInstant, Epoch};
use lash_sansio::ProcessId;

use super::keys::ScopeKey;

/// A wait's identity: 128 random bits, minted by the owner in the
/// transaction that pins the wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WaitId(pub [u8; 16]);

/// The version of the completion secret a host-resolvable key was minted
/// under. The row stores it, so a key verifies under its own version for as
/// long as that version is configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyVersion(pub u16);

/// What a wait waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaitKind {
    /// A tool's completion, resolved by the host with its key.
    ToolCompletion,
    /// A host-defined wait, resolved by the host with its key.
    Custom,
    /// A process's terminal.
    ProcessTerminal,
    /// A signal.
    Signal,
    /// A durable sleep.
    Timer,
    /// A child session's end.
    ChildSession,
}

impl WaitKind {
    /// Whether a host may resolve it: only `ToolCompletion` and `Custom`.
    #[must_use]
    pub const fn host_resolvable(self) -> bool {
        matches!(self, Self::ToolCompletion | Self::Custom)
    }
}

/// Where a wait is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaitState {
    /// Not yet resolved.
    Pending,
    /// Resolved: its resolution is stored.
    Resolved,
    /// Its deadline passed first.
    TimedOut,
    /// Its scope ended first.
    Revoked,
}

/// One stored wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitRow {
    /// Its identity.
    pub id: WaitId,
    /// The actor a resolution wakes.
    pub owner: ActorKey,
    /// The scope that revokes it.
    pub scope: ScopeKey,
    /// What it waits for.
    pub kind: WaitKind,
    /// For a process-terminal wait, the process.
    pub target_process: Option<ProcessId>,
    /// Where it is.
    pub state: WaitState,
    /// Its deadline, written once.
    pub deadline: Option<DurableInstant>,
    /// The resolution's digest, once resolved.
    pub resolution_digest: Option<String>,
    /// The resolution, by reference, once resolved.
    pub resolution_ref: Option<String>,
    /// When it was resolved.
    pub resolved_at: Option<DurableInstant>,
    /// For a host-resolvable wait, its key's secret version.
    pub key_version: Option<KeyVersion>,
    /// The epoch of the commit that minted it.
    pub created_epoch: Epoch,
}

/// A wait write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitWrite {
    /// Mint a wait: insert it pending.
    Pin {
        /// Its identity.
        id: WaitId,
        /// The scope that revokes it.
        scope: ScopeKey,
        /// What it waits for.
        kind: WaitKind,
        /// For a process-terminal wait, the process.
        target_process: Option<ProcessId>,
        /// Its deadline.
        deadline: Option<DurableInstant>,
        /// For a host-resolvable wait, its key's secret version.
        key_version: Option<KeyVersion>,
    },
    /// Settle a pending wait whose deadline passed: a timer resolves, any
    /// other kind times out. A resolution committed first wins.
    Due {
        /// The wait.
        id: WaitId,
    },
    /// Revoke every pending wait of `scope`.
    RevokeScope(ScopeKey),
    /// Resolve every pending process-terminal wait on `process` with
    /// `resolution`, and wake each owner.
    ResolveProcessTerminal {
        /// The process that ended.
        process: ProcessId,
        /// Its outcome's digest.
        digest: String,
        /// Its outcome, by reference.
        resolution_ref: String,
    },
}

/// A resolution of one wait from outside its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitResolution {
    /// The wait.
    pub id: WaitId,
    /// Whether the resolver is a host: a host may resolve only host-
    /// resolvable kinds, and is answered [`ResolveAnswer::ReservedKind`]
    /// otherwise.
    pub by_host: bool,
    /// The resolution's digest.
    pub digest: String,
    /// The resolution, by reference.
    pub resolution_ref: String,
}

/// The answer to a [`WaitResolution`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveAnswer {
    /// This resolution won, and the owner was woken.
    Resolved,
    /// An earlier resolution with the same digest won.
    AlreadyResolved,
    /// An earlier resolution with another digest won.
    Conflict,
    /// A host resolved a kind it may not; nothing was written.
    ReservedKind,
    /// No such pending or resolved wait: unknown, revoked or timed out.
    UnknownOrRevoked,
}
