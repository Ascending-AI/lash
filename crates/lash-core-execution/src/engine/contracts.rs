//! Contracts specified now rather than left to an adapter (ADR 0105 §5–§7).
//!
//! **Protocol-driver purity.** The `TurnProtocol` methods and the protocol
//! projectors are synchronous, take `&self` and have no side effects. Any
//! interior mutability in an implementor is a contract violation. A drive
//! replays them over recorded inputs and must reach the same decisions.

use serde::{Deserialize, Serialize};

pub use lash_core_store::build_generation::{BuildGeneration, BuildGenerationParseError};

use super::admission::{DriveFence, DriveRequestId, InheritedAuthority};
use super::context::ReplayKey;
use crate::store::BlobRef;
use crate::{AwaitEventKey, EffectGroupHandle, Resolution, SessionId, TurnId};

/// The acknowledgement of a keyed-promise resolution, returned only once the
/// resolution is durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ack", rename_all = "snake_case")]
pub enum ResolveAck {
    /// This resolution is the promise's terminal.
    First,
    /// The promise already had a terminal; `same` says whether it equals this
    /// one. A differing duplicate never overwrites.
    Duplicate { same: bool },
    /// The promise's session was revoked.
    Revoked,
}

/// The output of an operation that never completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Never {}

/// How a durable format's stored bytes move to a newer build (ADR 0106 §2).
///
/// The type lives in the kernel rather than the facade's format table so an
/// effect engine can declare the policy for the formats it registers
/// (ADR 0104 §2): an engine's durable formats are its own to describe, and
/// the drain generation that depends on the answer is kernel vocabulary too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum UpgradePolicy {
    /// Forward migration: schema DDL or a read upcaster, then writing at the
    /// fleet format.
    Migrate,
    /// A journal replays only under the code that wrote it, so it finishes on
    /// its own build; the drain generation carries these formats.
    Drain,
    /// Both versions live during the roll window: content addresses,
    /// idempotency keys, namespaced object state and negotiated wire versions.
    Coexist,
}

/// One logical drive request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveRequest {
    pub session: SessionId,
    pub request: DriveRequestId,
    pub build_generation: BuildGeneration,
}

/// Everything a fresh execution needs to continue a session, because it does
/// not inherit the previous execution's history.
///
/// A drive hands over only at a turn boundary, or at a checkpoint boundary of
/// an oversized turn, and only after every signal handler has finished.
#[derive(Debug, Serialize, Deserialize)]
pub struct DriveHandover {
    /// The authority continues; the epoch is not re-minted.
    pub fence: DriveFence,
    /// Resolved keys no wait has consumed yet.
    pub pending_resolutions: Vec<PendingResolution>,
    /// A window of drive requests already admitted.
    pub dedupe: Vec<DriveRequestId>,
    /// Open groups with their rank cursors.
    pub open_groups: Vec<EffectGroupHandle>,
    pub unresolved_children: Vec<UnresolvedChild>,
    pub active_root: Option<RootProgress>,
}

/// A keyed promise resolved before any wait consumed it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingResolution {
    pub key: AwaitEventKey,
    pub resolution: Resolution,
}

/// A child still running at the handover, with the authority it inherited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedChild {
    pub key: ReplayKey,
    pub authority: InheritedAuthority,
}

/// Where the active root stands at a handover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum RootProgress {
    /// Between turns of the root.
    Boundary { root: TurnId },
    /// Inside an oversized turn, at a checkpoint boundary.
    Segment(TurnSegmentHandover),
}

/// An oversized turn's handover at its next checkpoint boundary. A turn that
/// cannot hand over parks as journal-budget exhausted (ADR 0025).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSegmentHandover {
    pub root: TurnId,
    /// The turn machine's checkpoint.
    pub checkpoint: BlobRef,
    /// The opener's fold over recorded outcomes.
    pub opener_fold: BlobRef,
}
