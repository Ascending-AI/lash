//! Laws of the one ingress (FIG-3600 S5b, D1 §1): what a [`SendHandle`]
//! answers, read from what was recorded, whichever engine drove the input.
//!
//! Every law runs on lash-restate's engine over the Restate server double.
//!
//! [`SendHandle`]: crate::SendHandle

mod exact_host_runs {}

#[test]
fn a_journaled_send_outcome_requires_the_data_owned_by_its_variant() {
    let invalid = serde_json::json!({
        "status": "Answered", "run": null, "output": null, "gaps": []
    });
    assert!(
        serde_json::from_value::<crate::SendOutcome>(invalid).is_err(),
        "a journaled answered send cannot exist without its run and output"
    );
}
