use super::*;

/// A node id is never empty, typed or decoded.
#[test]
fn a_node_id_is_never_empty() {
    assert_eq!(WorkflowNodeId::new(""), Err(EmptyWorkflowNodeId));
    assert!(serde_json::from_value::<WorkflowNodeId>(serde_json::json!("")).is_err());
    let id = WorkflowNodeId::new("node:1").expect("a nonempty id");
    assert_eq!(serde_json::to_value(&id).expect("encode"), "node:1");
    assert_eq!(
        serde_json::from_value::<WorkflowNodeId>(serde_json::json!("node:1")).expect("decode"),
        id
    );
}

/// Occurrences count from 1: a stored occurrence 0 does not decode.
#[test]
fn an_occurrence_counts_from_one() {
    let first = serde_json::json!({ "site": { "node_id": "node:1" }, "occurrence": 1 });
    let decoded: WorkflowOccurrence = serde_json::from_value(first.clone()).expect("decode");
    assert_eq!(decoded, WorkflowOccurrence::fixture("node:1", 1));
    assert_eq!(serde_json::to_value(&decoded).expect("encode"), first);
    let mut zeroth = first;
    zeroth["occurrence"] = serde_json::json!(0);
    assert!(serde_json::from_value::<WorkflowOccurrence>(zeroth).is_err());
}

/// A site path is the slots to its expression and at most one trailing
/// role: a role between slots, or a second role, has no spelling.
#[test]
fn a_site_path_holds_slots_and_at_most_one_role() {
    let path = WorkflowSitePath::at([ExprSlot::Value, ExprSlot::Arg(0)])
        .with_role(WorkflowSiteRole::LabeledStep);
    let wire = serde_json::json!({ "slots": ["value", { "arg": 0 }], "role": "labeled_step" });
    assert_eq!(serde_json::to_value(&path).expect("encode"), wire);
    assert_eq!(path.to_string(), "/value/arg[0]#labeled_step");
    assert_eq!(
        serde_json::to_value(WorkflowSitePath::default()).expect("encode"),
        serde_json::json!({})
    );
    for refused in [
        serde_json::json!(["value", { "role": "labeled_step" }, { "arg": 0 }]),
        serde_json::json!({ "slots": ["value", { "role": "labeled_step" }] }),
        serde_json::json!({ "slots": ["value"], "role": ["labeled_step", "labeled_step"] }),
    ] {
        assert!(
            serde_json::from_value::<WorkflowSitePath>(refused.clone()).is_err(),
            "{refused}"
        );
    }
}
