//! Contracts specified now rather than left to an adapter (ADR 0105 §5–§7).
//!
//! **Protocol-driver purity.** The `TurnProtocol` methods and the protocol
//! projectors are synchronous, take `&self` and have no side effects. Any
//! interior mutability in an implementor is a contract violation. A shift
//! replays them over recorded inputs and must reach the same decisions.

use serde::{Deserialize, Serialize};

use super::admission::ShiftRequestId;
use crate::SessionId;

/// How a durable format's stored bytes move to a newer build (ADR 0106 §2).
///
/// The type lives in the kernel rather than the facade's format table so an
/// effect engine can declare the policy for the formats it registers
/// (ADR 0104 §2): an engine's durable formats are its own to describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum UpgradePolicy {
    /// Forward migration: schema DDL or a read upcaster, then writing at the
    /// fleet format.
    Migrate,
    /// Stored bytes only the build that wrote them decodes, so the work
    /// finishes on its own build before a newer one takes it.
    Drain,
    /// Both versions live during the roll window: content addresses,
    /// idempotency keys, namespaced object state and negotiated wire versions.
    Coexist,
}

/// One logical shift request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShiftRequest {
    pub session: SessionId,
    pub request: ShiftRequestId,
}
