//! Compile the production renderer without any runtime crate in scope.

#[expect(
    clippy::expect_used,
    reason = "shared test renderer: broken fixture assumptions abort the test"
)]
#[path = "../../../crates/lash-core-execution/src/testing/behavior_transcript.rs"]
pub mod behavior_transcript;

#[test]
fn transcript_module_has_no_runtime_dependencies() {
    use behavior_transcript::{Actor, Entry, Transcript, Usage};
    let mut transcript = Transcript::new();
    transcript.record(Entry::commit(Actor::session("root"), 0, 1, Usage::none()));
    assert!(
        transcript
            .render()
            .contains("usage                 entries=0")
    );
}
