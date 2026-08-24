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

// Bumped to 40: `RemoteRuntimeEffectKind` gains `AssistantResponseHooks`, the
// second phase of the staged LLM-call effect boundary (FIG-1276). A version 39
// peer has no such variant and fails to decode any effect projection carrying
// it, so the handshake must reject it rather than let it choke mid-stream.
// Bumped to 39: `RemoteToolIntentKind` gains `EmitTrigger`, the fifth durable
// tool-intent declaration. A version 38 peer has no name for that kind, so it
// would refuse or misread a recorded declaration instead of emitting the
// occurrence the committed attempt owes.
// Bumped to 38: `RemoteRuntimeEffectKind` gains `LanguageRuntimeValue`, the
// journaled sample a language runtime draws for a nondeterministic builtin. A
// version 37 peer has no name for that kind and would reject or misread the
// effect rather than replay it.
// Bumped to 32: session process originators retain their optional agent-frame
// elevation. Exact version negotiation prevents an older peer from silently
// dropping that authority before a process wake is enqueued.
// Bumped to 36: tool-intent outcomes and replay attribution are typed on the
// wire alongside sealed LLM attempt ledgers; activity events are the canonical
// remote evidence surface for both additions.
// Bumped to 35: sealed LLM attempt ledgers and provider execution evidence are
// carried by remote turn results and semantic activity streams.
// Bumped to 34: generation disposition renamed the protocol-owned stop value
// without a compatibility decoder, and execution evidence can explicitly
// report collection interrupted by a protocol abort.
// Bumped to 33: `RemoteTurnOutcome::AgentFrameSwitch` carries `frame_key`
// instead of `frame_id`; version 32 carries the wake-coalesce wire change.
// Bumped to 31: process execution policies require an explicit `turn_budget`,
// and their content-addressed environment refs use the v3 family prefix
// (FIG-1087). 29 was provisionally assigned to this change but 30 (the
// stop-sequence disposition) released first; the check is exact-equality, so
// the gap is harmless.
// Bumped to 30: generation disposition reports when a protocol replaces a
// caller's stop-sequence list with protocol-owned response boundaries. Version
// 29 belongs to the concurrent registry-union wire change (#316), so this
// branch takes the next value to avoid a version collision.
// Bumped to 28: `RemoteToolFailureClass` gained the `Io` variant (FIG-1098);
// older peers reject the unknown variant mid-payload, so the addition needs a
// clean version rejection.
// Bumped to 27: `RemoteGenerationReceipt` gained an always-serialized
// `cache` field (FIG-1101); older peers reject the unknown field on every
// disposition, so the shape change requires a clean version rejection.
// Bumped to 25: residual process/trigger/effect identities use the FIG-915
// structural and shared-framing families; older peers cannot safely replay
// their durable names.
// Bumped to 24: process env refs carry the v2 family prefix, session-turn
// inputs carry their caller-owned definition key, and trigger records expose
// definition fingerprints rather than unversioned hashes.
// Bumped to 23: process originators carry raw session ids, process list filters
// use originator ids, and contradictory status/outcome pairs are rejected.
// Bumped to 22: process DTOs use explicit identity, observer, wake-session,
// lifecycle-status, and outcome fields from the observer-schema cutover.
// Bumped to 21: process wake dedupe is always event identity; the removed
// selector and constant variants no longer exist on the wire.
// Bumped to 20: `RemoteProcessExecutionPolicy` carries the session's
// generation options, mirroring `SessionPolicy.generation`. A version 19 peer
// would drop them on the way in and resume a session with uncontrolled
// sampling instead of the caller's.
// Bumped to 42: session-observation events distinguish revision-stable
// `resident_changed` replacements from durable `committed` replacements. A
// version 41 peer has no resident signal and must never decode one as committed.
// Bumped to 43: remote turn reports reject status/outcome contradictions;
// peers must derive status from the terminal outcome rather than trusting both
// fields independently (FIG-1756).
// Bumped to 44: `RemoteTurnStatus` is a projection derived from the turn
// outcome and no longer has an `in_progress` variant -- a turn report exists
// only once the turn has a terminal outcome, so a version 43 peer emitting
// that status must be refused rather than mapped onto a terminal one.
// Bumped to 45: remote runtime effects carry an `accept_turn_input` kind for
// the journaled turn-input acceptance. A version 44 peer rejects the new
// variant outright, so the two sides must agree before it can appear on the
// wire (FIG-1671).
// Bumped to 46: turn-cancel requests carry the typed undelivered-input
// disposition. Older peers would silently apply the legacy defer policy.
pub const REMOTE_PROTOCOL_VERSION: u32 = 46;

pub(crate) fn decode_versioned_json<T>(
    bytes: &[u8],
    expected_version: u32,
) -> Result<T, RemoteProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    #[derive(serde::Deserialize)]
    struct VersionProbe {
        protocol_version: u32,
    }

    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(RemoteProtocolError::MessageDecode)?;
    if probe.protocol_version != expected_version {
        return Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: probe.protocol_version,
            expected: expected_version,
        });
    }
    serde_json::from_slice(bytes).map_err(RemoteProtocolError::MessageDecode)
}

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

#[cfg(any(feature = "core-conversions", test))]
mod core_conversions;

#[cfg(any(feature = "core-conversions", test))]
pub use core_conversions::{RemoteTurnActivitySink, replay_collected_activities};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod versioned_decode_tests;
