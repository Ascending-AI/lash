//! Boundary witnesses for wire-supplied effect-group shapes.
//!
//! `EffectGroupShape` and `EffectGroupMembership` are public API with public
//! fields that travel side by side, so `replay_keys` and the membership can
//! disagree in anything a caller sends. These are the tests that a
//! disagreement is refused terminally rather than indexed into a panic inside
//! a handler.

use lash_core::{ExecutionScope, GroupWakePolicy, LoserPolicy};

use crate::effect_group::{EffectGroupMembership, EffectGroupShape};

fn shape(replay_keys: &[&str]) -> EffectGroupShape {
    EffectGroupShape {
        wake: GroupWakePolicy::All,
        loser_disposition: LoserPolicy::RunToCompletion,
        replay_keys: replay_keys.iter().map(|key| (*key).to_owned()).collect(),
        wait_scope: ExecutionScope::runtime_operation("group-key"),
        opener: lash_core::AdmittedScope::turn("session", "turn"),
    }
}

#[test]
fn a_wire_membership_may_disagree_with_its_shape_and_is_refused_terminally() {
    // `replay_keys` and the membership are independent public values, so
    // nothing in the types or in serde stops a caller from sending a pair
    // that disagrees -- this test builds exactly that and round-trips it
    // through the wire form to prove deserialization accepts it.
    // `validate_membership` is what refuses it, at the boundary, with a
    // terminal error a retry cannot fix.
    let mismatched = (shape(&["child-0"]), EffectGroupMembership(Vec::new()));
    let encoded = serde_json::to_vec(&mismatched).expect("serialize mismatched pair");
    let (decoded, membership): (EffectGroupShape, EffectGroupMembership) =
        serde_json::from_slice(&encoded).expect("the wire form accepts a mismatched pair");

    let error = decoded
        .validate_membership(&membership)
        .expect_err("a membership that disagrees with its shape must be refused");
    assert!(
        error
            .message()
            .contains("declares 1 children but retains 0 accepted requests"),
        "the refusal must name both counts: {error}"
    );
}

#[test]
fn a_child_position_past_the_replay_keys_is_a_typed_terminal_error() {
    // The close, retirement-cancel, and dispatch-child paths all pair a
    // position with the replay key at that position. Out of range must be a
    // terminal error: a panic inside a handler is retryable, so it would wedge
    // the object key for every later handler until an operator intervened.
    let shape = shape(&["child-0"]);

    assert_eq!(
        shape.member_replay_key(0).expect("the recorded child"),
        "child-0"
    );
    let error = shape
        .member_replay_key(1)
        .expect_err("a position past the replay keys must not panic");
    assert!(
        error.message().contains("no replay key for child 1"),
        "the refusal must name the missing position: {error}"
    );
    let membership = EffectGroupMembership(vec!["{}".to_owned()]);
    let error = membership
        .envelope("group-key", 1)
        .expect_err("a position past the membership must not panic");
    assert!(
        error
            .message()
            .contains("retains no membership for child 1"),
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
    let two = shape(&["child-0", "child-1"]);
    let error = two
        .validate_membership(&EffectGroupMembership(Vec::new()))
        .expect_err("a shape with no membership cannot rebuild its children");
    assert!(
        error
            .message()
            .contains("declares 2 children but retains 0 accepted requests"),
        "the refusal must name both counts: {error}"
    );

    // A short membership is the same defect, and is refused the same way.
    assert!(
        two.validate_membership(&EffectGroupMembership(vec!["{}".to_owned()]))
            .expect_err("a short membership is refused")
            .message()
            .contains("declares 2 children but retains 1 accepted requests")
    );

    // The open request has no default for it either: a payload that omits
    // the field entirely cannot decode into a request at all.
    let request = crate::EffectGroupOpenRequest {
        shape: two,
        membership: EffectGroupMembership(vec!["{}".to_owned(), "{}".to_owned()]),
        dispatch_route: "EffectGroupDispatch".to_owned(),
        content_checked: false,
    };
    let mut value = serde_json::to_value(&request).expect("serialize");
    value
        .as_object_mut()
        .expect("an open request is a JSON object")
        .remove("membership");
    assert!(
        serde_json::from_value::<crate::EffectGroupOpenRequest>(value).is_err(),
        "an omitted membership must not decode to an empty one"
    );
}
