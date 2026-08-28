//! Boundary witnesses for wire-supplied effect-group shapes.
//!
//! `EffectGroupShape` is public API with public fields, so its two halves --
//! `children` and `replay_keys` -- can disagree in anything a caller sends.
//! These are the tests that a disagreement is refused terminally rather than
//! indexed into a panic inside a handler.

use lash_core::{ExecutionScope, GroupWakePolicy, LoserPolicy};

use crate::effect_group::EffectGroupShape;

#[test]
fn a_wire_shape_may_disagree_with_itself_and_is_refused_terminally() {
    // `children` and `replay_keys` are independent public fields, so nothing in
    // the type or in serde stops a caller from sending a shape whose halves
    // disagree -- this test builds exactly that and round-trips it through the
    // wire form to prove deserialization accepts it. `validate_wire` is what
    // refuses it, at the boundary, with a terminal error a retry cannot fix.
    let mismatched = EffectGroupShape {
        children: 2,
        wake: GroupWakePolicy::First,
        loser_disposition: LoserPolicy::Cancel,
        replay_keys: vec!["child-0".to_owned()],
        wait_scope: ExecutionScope::runtime_operation("group-key"),
    };
    let encoded = serde_json::to_vec(&mismatched).expect("serialize mismatched shape");
    let decoded: EffectGroupShape =
        serde_json::from_slice(&encoded).expect("the wire form accepts a mismatched shape");

    let error = decoded
        .validate_wire()
        .expect_err("a shape whose halves disagree must be refused");
    assert!(
        error
            .message()
            .contains("declares 2 children but carries 1 replay keys"),
        "the refusal must name both counts: {error}"
    );
}

#[test]
fn a_child_position_past_the_replay_keys_is_a_typed_terminal_error() {
    // The close, retirement-cancel, and dispatch-child paths all pair a
    // position with the replay key at that position. Out of range must be a
    // terminal error: a panic inside a handler is retryable, so it would wedge
    // the object key for every later handler until an operator intervened.
    let shape = EffectGroupShape {
        children: 1,
        wake: GroupWakePolicy::All,
        loser_disposition: LoserPolicy::RunToCompletion,
        replay_keys: vec!["child-0".to_owned()],
        wait_scope: ExecutionScope::runtime_operation("group-key"),
    };

    assert_eq!(shape.replay_key(0).expect("the recorded child"), "child-0");
    let error = shape
        .replay_key(1)
        .expect_err("a position past the replay keys must not panic");
    assert!(
        error.message().contains("no replay key for child 1"),
        "the refusal must name the missing position: {error}"
    );
}
