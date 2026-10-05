use serde_json::json;

use super::*;

/// The baton carries only what the next step can be rebuilt from: a context
/// of strings within budget, plain vars that do not shadow a harness binding,
/// and a boolean `final`. The context travels in the seed under `context`.
#[test]
fn next_admits_only_a_baton_the_next_step_can_be_rebuilt_from() {
    let budget = 20;
    let next = RelayNext::from_args(
        &json!({ "context": ["a", "bc"], "vars": { "n": 1 }, "final": true }),
        budget,
    )
    .expect("a valid baton");
    assert_eq!(next.context, ["a", "bc"]);
    assert!(next.final_turn);
    let seed = next.seed_body();
    assert_eq!(seed.globals[CONTEXT_VAR], json!(["a", "bc"]));
    assert_eq!(seed.globals["n"], json!(1));
    assert_eq!(RelayBaton::from_seed(&seed).vars.len(), 1);

    for (args, refusal) in [
        (json!({}), "missing required parameter: context"),
        (json!({ "context": [1] }), "context[0] must be a string"),
        (
            json!({ "context": "a" }),
            "context must be an array of strings",
        ),
        (json!({ "context": [], "vars": 3 }), "vars must be a record"),
        (
            json!({ "context": [], "vars": { "context": [] } }),
            "vars cannot carry `context`",
        ),
        (
            json!({ "context": [], "vars": { "transcript": [] } }),
            "vars cannot carry `transcript`",
        ),
        (
            json!({ "context": [], "final": "yes" }),
            "final must be a boolean",
        ),
        (
            json!({ "context": ["x".repeat(21)] }),
            "over the 20-character budget",
        ),
    ] {
        let error = RelayNext::from_args(&args, budget).expect_err(refusal);
        assert!(error.contains(refusal), "{args}: {error}");
    }
}
