//! Compile the production renderer without any runtime crate in scope.

// `not(test)`: this crate's own unit-test build would otherwise compile the
// imported module's `#[cfg(test)]` laws a second time; they already run in
// lash-core-execution's unit target. The library build still proves the
// renderer compiles with no runtime crate in scope.
#[cfg(not(test))]
#[expect(
    clippy::expect_used,
    reason = "shared test renderer: broken fixture assumptions abort the test"
)]
#[path = "../../lash-core-execution/src/testing/behavior_transcript.rs"]
pub mod behavior_transcript;
