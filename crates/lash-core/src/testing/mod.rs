//! In-tree test fixtures shared across the lash crate's test modules.
//!
//! Cuts down on per-test-module `MockSessionManager` boilerplate by
//! providing a configurable mock implementation plus a couple of small
//! builders for common policy / turn fixtures.

// Test-support module: these fixtures run inside a test, and a broken setup
// assumption must abort it loudly rather than be reshaped into a runtime error
// the test under way would then report as a runtime defect. Clippy's
// `allow-expect-in-tests` reaches `#[test]` functions only, not the fixtures
// they call.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures: a broken setup assumption aborts the test"
)]

pub use lash_core_execution::testing::*;

// Each submodule documents itself in its own file. Adding an outer doc comment
// here as well would merge two fragments written in different scopes, and a
// reader or editor following the merged doc comment — including the
// submodule's own intra-doc links — would resolve it against *this* module's
// scope, where none of the linked items exist.
pub mod adversarial_text;
pub mod checkpoint_observer;
pub mod conformance_support;
mod live_replay;
pub mod runtime_helpers;
#[cfg(feature = "testing")]
pub mod runtime_internals;

/// Marks resident state stale for downstream reload-race tests.
#[cfg(any(test, feature = "testing"))]
pub fn invalidate_resident_session_state_for_testing(runtime: &mut crate::LashRuntime) {
    runtime.invalidate_resident_session_state();
}

/// Synthesize the response produced when a plugin aborts an in-flight LLM
/// stream after `events` have reached core's stream accumulator.
///
/// This test seam deliberately uses the production accumulator and its
/// empty-response branch: on the abort path, those accumulated parts are the
/// whole response rather than gap-fill input for a provider completion.
pub fn response_synthesized_from_aborted_stream(
    events: &[crate::llm::types::LlmStreamEvent],
) -> crate::llm::types::LlmResponse {
    crate::runtime::response_synthesized_from_aborted_stream(events)
}
