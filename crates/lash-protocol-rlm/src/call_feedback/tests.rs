use super::{CallFeedbackPolicy, CallFeedbackPolicyError, framing, summarize, write_calls_block};

fn call(operation: &str, ok: bool, id: &str, text: Option<&str>) -> lash_core::ExecutedCall {
    lash_core::ExecutedCall {
        operation: operation.to_string(),
        outcome: if ok {
            lash_core::ExecutedCallOutcome::Ok
        } else {
            lash_core::ExecutedCallOutcome::Err
        },
        call_id: Some(lash_core::ToolCallId::fixture(id)),
        result_summary: text.map(|text| lash_core::CallResultSummary {
            text: text.to_string(),
            truncated: false,
        }),
    }
}

fn row(call: &lash_core::ExecutedCall) -> String {
    serde_json::to_string(&super::Row::of(call)).expect("a row encodes")
}

fn block(calls: &[lash_core::ExecutedCall], max_calls: usize, max_block_bytes: usize) -> String {
    let policy = CallFeedbackPolicy::new(0, max_block_bytes).expect("a valid policy");
    let mut out = String::new();
    write_calls_block(&mut out, calls, max_calls, policy);
    out
}

/// FIG-6293: a cell's calls block keeps whole rows, successful ones first,
/// lists them in admission order, and counts exactly what it leaves out at
/// each byte and count edge. Below its fixed framing it omits every call.
#[test]
fn calls_block_budget_edges_render_exact_omission_counts() {
    let calls = [
        call("inbox.send_item", true, "a", Some("{\"item_id\":\"i-1\"}")),
        call(
            "inbox.fail_with_a_long_tool_path",
            false,
            "b",
            Some("{\"error\":\"refused\"}"),
        ),
        call("inbox.read", true, "c", None),
    ];
    let [first, failed, last] = [row(&calls[0]), row(&calls[1]), row(&calls[2])];
    assert!(failed.len() > last.len(), "the failed row is the widest");
    let framed = framing(calls.len()).len();
    assert_eq!(
        framing(calls.len()),
        "{\"completed_calls\":[],\"omitted\":{\"ok\":3,\"err\":3}}"
    );
    let both_ok = framed + first.len() + 1 + last.len();

    // Exactly both successful rows fit: the earlier failure is skipped.
    let rendered = block(&calls, 128, both_ok);
    assert_eq!(
        rendered,
        format!("{{\"completed_calls\":[{first},{last}],\"omitted\":{{\"ok\":0,\"err\":1}}}}")
    );
    assert!(rendered.len() <= both_ok);
    assert!(last.contains("\"result_summary\":null"), "{last}");

    // One byte less: the last successful row no longer fits, nor the wider failure.
    assert_eq!(
        block(&calls, 128, both_ok - 1),
        format!("{{\"completed_calls\":[{first}],\"omitted\":{{\"ok\":1,\"err\":1}}}}")
    );

    // Room for everything restores admission order.
    assert_eq!(
        block(&calls, 128, 16 * 1024),
        format!(
            "{{\"completed_calls\":[{first},{failed},{last}],\"omitted\":{{\"ok\":0,\"err\":0}}}}"
        )
    );

    // The count cut keeps the first successful row alone.
    assert_eq!(
        block(&calls, 1, 16 * 1024),
        format!("{{\"completed_calls\":[{first}],\"omitted\":{{\"ok\":1,\"err\":1}}}}")
    );

    // Below the framing, and at zero, every call is counted as omitted.
    for cut in [framed - 1, 0] {
        assert_eq!(
            block(&calls, 128, cut),
            "{\"completed_calls\":[],\"omitted\":{\"ok\":2,\"err\":1}}"
        );
    }
    assert_eq!(
        block(&calls, 0, 16 * 1024),
        "{\"completed_calls\":[],\"omitted\":{\"ok\":2,\"err\":1}}"
    );
}

/// FIG-6293: a summary is the result's canonical compact JSON, its keys in
/// byte order and its escapes intact, cut at a UTF-8 boundary and marked
/// truncated only when cut.
#[test]
fn result_summaries_are_canonical_and_cut_on_a_character_boundary() {
    let value = serde_json::json!({"b": [1, 2.5, null], "a": "é\"\n"});
    let whole = "{\"a\":\"é\\\"\\n\",\"b\":[1,2.5,null]}";
    assert_eq!(
        summarize(&value, whole.len()),
        lash_core::CallResultSummary {
            text: whole.to_string(),
            truncated: false,
        }
    );
    // `{"a":"é` with é's second byte cut away.
    assert_eq!(
        summarize(&value, 7),
        lash_core::CallResultSummary {
            text: "{\"a\":\"".to_string(),
            truncated: true,
        }
    );
    assert_eq!(summarize(&value, 0).text, "");
}

/// FIG-6293: the recorded cuts are refused outside their bounds, through
/// the constructor and through a recorded spelling alike.
#[test]
fn call_feedback_policy_refuses_cuts_outside_its_bounds() {
    let ceiling = CallFeedbackPolicy::MAX_BLOCK_BYTES;
    assert!(CallFeedbackPolicy::new(ceiling, ceiling).is_ok());
    assert!(CallFeedbackPolicy::new(0, 0).is_ok());
    assert_eq!(
        CallFeedbackPolicy::new(0, ceiling + 1),
        Err(CallFeedbackPolicyError::BlockExceedsCeiling {
            max_block_bytes: ceiling + 1,
            ceiling,
        })
    );
    assert_eq!(
        CallFeedbackPolicy::new(11, 10),
        Err(CallFeedbackPolicyError::ResultExceedsBlock {
            max_result_bytes: 11,
            max_block_bytes: 10,
        })
    );
    assert!(
        serde_json::from_value::<CallFeedbackPolicy>(
            serde_json::json!({"max_result_bytes": 11, "max_block_bytes": 10})
        )
        .is_err()
    );
    assert_eq!(
        serde_json::from_value::<CallFeedbackPolicy>(
            serde_json::to_value(CallFeedbackPolicy::standard()).expect("encodes")
        )
        .expect("decodes"),
        CallFeedbackPolicy::standard()
    );
}
