//! Contracts specified now rather than left to an adapter (ADR 0105 §5–§7).
//!
//! **Protocol-driver purity.** The `TurnProtocol` methods and the protocol
//! projectors are synchronous, take `&self` and have no side effects. Any
//! interior mutability in an implementor is a contract violation. A drive
//! replays them over recorded inputs and must reach the same decisions.

use serde::{Deserialize, Serialize};

pub use lash_core_store::build_generation::{BuildGeneration, BuildGenerationParseError};

use super::admission::DriveRequestId;
use crate::SessionId;

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
