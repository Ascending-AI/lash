#![cfg(any(test, feature = "testing"))]
// Test-support module: these fixtures run inside a test, and a broken setup
// assumption must abort it loudly rather than be reshaped into a runtime error
// the test under way would then report as a runtime defect. Clippy's
// `allow-expect-in-tests` reaches `#[test]` functions only, not the fixtures
// they call.
#![expect(
    clippy::expect_used,
    reason = "test-support fixtures: a broken setup assumption aborts the test"
)]

mod effect_summary_faults;
#[cfg(test)]
mod identity;
#[path = "testing/parent_end_fault.rs"]
mod parent_end_fault;
mod registration_refusals;
mod registry_faults;
mod support;
pub use effect_summary_faults::EffectSummaryAppendFaults;
pub use parent_end_fault::fail_parent_end_once;
pub use registration_refusals::{
    REFUSAL_FIXTURE_START_KEY as PROCESS_REFUSAL_FIXTURE_START_KEY, accepted_process_registration,
    refused_process_registrations,
};
pub use registry_faults::{
    ProcessRegistryFaults, RegistrationHoldPoint, WorklistPagePause, WorklistPageRead,
};
pub use support::TestProcessRegistryWriteExt;
