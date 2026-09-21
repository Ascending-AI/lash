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
        membership: Vec::new(),
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
        membership: vec!["{}".to_owned()],
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

#[test]
fn a_shape_that_cannot_rebuild_its_children_is_refused_terminally() {
    // An empty membership beside a nonzero arity is not a tolerated legacy
    // shape. ADR 0099 records that no production caller of
    // `open_effect_group` exists, so no deployment can be holding a group
    // whose state predates the membership -- and accepting one would be a
    // pre-cutover acceptance arm for a population that cannot exist, which is
    // the class this arc is deleting.
    let empty = EffectGroupShape {
        children: 2,
        wake: GroupWakePolicy::All,
        loser_disposition: LoserPolicy::RunToCompletion,
        replay_keys: vec!["child-0".to_owned(), "child-1".to_owned()],
        wait_scope: ExecutionScope::runtime_operation("group-key"),
        membership: Vec::new(),
    };
    let error = empty
        .validate_wire()
        .expect_err("a shape with no membership cannot rebuild its children");
    assert!(
        error
            .message()
            .contains("declares 2 children but retains 0 accepted requests"),
        "the refusal must name both counts: {error}"
    );

    // A short membership is the same defect, and is refused the same way.
    let short = EffectGroupShape {
        children: 2,
        wake: GroupWakePolicy::All,
        loser_disposition: LoserPolicy::RunToCompletion,
        replay_keys: vec!["child-0".to_owned(), "child-1".to_owned()],
        wait_scope: ExecutionScope::runtime_operation("group-key"),
        membership: vec!["{}".to_owned()],
    };
    assert!(
        short
            .validate_wire()
            .expect_err("a short membership is refused")
            .message()
            .contains("declares 2 children but retains 1 accepted requests")
    );

    // The wire form has no default for it either: a payload that omits the
    // field entirely cannot decode into a shape at all.
    let mut value = serde_json::to_value(&short).expect("serialize");
    value
        .as_object_mut()
        .expect("a shape is a JSON object")
        .remove("membership");
    assert!(
        serde_json::from_value::<EffectGroupShape>(value).is_err(),
        "an omitted membership must not decode to an empty one"
    );
}
