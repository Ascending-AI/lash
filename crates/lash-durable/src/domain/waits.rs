//! Wait rows (L5): keyed promises, durable waits and timers
//! (ADR 0132 §6).
//!
//! A wait's deadline is written once, at minting. The first resolution wins.
//! Lock order is the wait row, then the actor row.

use crate::ids::{ActorKey, DurableInstant, Epoch};
use lash_sansio::ProcessId;

use super::keys::ScopeKey;

/// The mail kind of an awaiter's own cancel request (ADR 0132 §11: a cancel
/// request is a mailbox row that wakes its actor). An unacknowledged mail of
/// this kind in the awaiter's mailbox ends its race `Cancelled`; the lanes
/// that request a cancel (L3's turn cancel, L6's process cancel) append it.
pub const CANCEL_MAIL: &str = "cancel";

/// The format of a wait row: its columns, kinds and states below. A row
/// carries no stamp of its own: its owner's format set carries this version,
/// so only a node that decodes it claims the owner (ADR 0106 §1).
///
/// version_guard(
///     items(
///         path = "crates/lash-durable/src/domain/waits.rs",
///         WaitRow, WaitKind, WaitState, WaitId,
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "WaitRow"
pub const WAIT_ROW_FORMAT_VERSION: u32 = 1;

/// A wait's identity: 128 random bits from the operating system's CSPRNG,
/// minted by the owner in the transaction that pins the wait. A
/// host-resolvable wait's completion key is this id: it is unguessable, so
/// holding it is the capability to resolve the wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WaitId(pub [u8; 16]);

impl WaitId {
    /// The stored spelling: 32 lowercase hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        self.0
            .iter()
            .fold(String::with_capacity(32), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }

    /// The stored spelling read back; `None` for anything but 32 hex digits.
    #[must_use]
    pub fn parse_hex(stored: &str) -> Option<Self> {
        if stored.len() != 32 || !stored.is_ascii() {
            return None;
        }
        let mut id = [0_u8; 16];
        for (index, byte) in id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(stored.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(id))
    }
}

impl std::fmt::Display for WaitId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

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
    /// The end of the turn a `SessionTurn` process runs in its child
    /// session: the wait's target is that process, whose id names the
    /// child's run.
    ChildSession,
}

impl WaitKind {
    /// Whether a host may resolve it: only `ToolCompletion` and `Custom`.
    #[must_use]
    pub const fn host_resolvable(self) -> bool {
        matches!(self, Self::ToolCompletion | Self::Custom)
    }

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolCompletion => "tool_completion",
            Self::Custom => "custom",
            Self::ProcessTerminal => "process_terminal",
            Self::Signal => "signal",
            Self::Timer => "timer",
            Self::ChildSession => "child_session",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        [
            Self::ToolCompletion,
            Self::Custom,
            Self::ProcessTerminal,
            Self::Signal,
            Self::Timer,
            Self::ChildSession,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == stored)
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

impl WaitState {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
            Self::TimedOut => "timed_out",
            Self::Revoked => "revoked",
        }
    }

    /// The stored spelling read back; `None` for anything else.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        [Self::Pending, Self::Resolved, Self::TimedOut, Self::Revoked]
            .into_iter()
            .find(|state| state.as_str() == stored)
    }
}

/// The digest a due timer resolves with: a timer carries no value.
pub const TIMER_DIGEST: &str = "timer";

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
        /// For a process-terminal wait, the process; for a child-session
        /// wait, the process whose child turn it waits for.
        target_process: Option<ProcessId>,
        /// Its deadline.
        deadline: Option<DurableInstant>,
    },
    /// Settle a pending wait of the committing owner whose deadline passed
    /// as of the commit: a timer resolves with [`TIMER_DIGEST`], any other
    /// kind times out. A resolution committed first wins: the write changes
    /// only a row still pending, under the same lock.
    Due {
        /// The wait.
        id: WaitId,
    },
    /// Revoke every pending wait of `scope`, and wake each other owner.
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
    /// Resolve every pending child-session wait on `process` with
    /// `resolution`, and wake each owner: the end of the turn the process
    /// runs in its child session, written by the child's terminal
    /// transaction.
    ResolveChildSession {
        /// The process whose child turn ended.
        process: ProcessId,
        /// The turn's end's digest.
        digest: String,
        /// The turn's end, by reference.
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
    /// No wait has this id: nothing was written.
    Unknown,
    /// The wait was revoked, or timed out, before this resolution; nothing
    /// was written.
    Revoked,
}
