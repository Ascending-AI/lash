//! Wire DTOs for driving a lash runtime across a process boundary.
//!
//! Each domain module carries one slice of the protocol vocabulary
//! ([`llm`], [`turn_input`], [`turn_result`], [`processes`], [`triggers`],
//! [`prompt`], [`tools`], [`observations`], [`usage_activity`],
//! [`registry_errors`]); the crate root re-exports all of them, which is the
//! established public API for direct consumers of this crate. The
//! cross-cutting protocol handshake ([`REMOTE_PROTOCOL_VERSION`],
//! [`ensure_protocol_version`]) lives at the root itself.

pub mod llm;
pub mod observations;
pub mod processes;
pub mod prompt;
pub mod registry_errors;
pub mod tools;
pub mod triggers;
pub mod turn_control;
pub mod turn_input;
pub mod turn_result;
pub mod usage_activity;

pub use llm::*;
pub use observations::*;
pub use processes::*;
pub use prompt::*;
pub use registry_errors::*;
pub use tools::*;
pub use triggers::*;
pub use turn_control::*;
pub use turn_input::*;
pub use turn_result::*;
pub use usage_activity::*;

// Bumped to 21: process wake dedupe is always event identity; the removed
// selector and constant variants no longer exist on the wire.
// Bumped to 20: `RemoteProcessExecutionPolicy` carries the session's
// generation options, mirroring `SessionPolicy.generation`. A version 19 peer
// would drop them on the way in and resume a session with uncontrolled
// sampling instead of the caller's.
pub const REMOTE_PROTOCOL_VERSION: u32 = 22;

pub fn ensure_protocol_version(actual: u32) -> Result<(), RemoteProtocolError> {
    if actual == REMOTE_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual,
            expected: REMOTE_PROTOCOL_VERSION,
        })
    }
}

#[cfg(feature = "core-conversions")]
mod core_conversions;

#[cfg(feature = "core-conversions")]
pub use core_conversions::{RemoteTurnActivitySink, replay_collected_activities};

#[cfg(test)]
mod tests;
