use super::{CallFeedbackPolicy, CallFeedbackPolicyError, summarize, write_calls_block};

fn call(operation: &str, ok: bool, id: &str, text: Option<&str>) -> lash_core::ExecutedCall {
    lash_core::ExecutedCall {
        operation: operation.to_string(),
        outcome: if ok {
            lash_core::ExecutedCallOutcome::Ok {}
        } else {
            lash_core::ExecutedCallOutcome::Failed {
                code: "tool_failed".into(),
                message: "failed".into(),
            }
        },
        call_id: Some(lash_core::ToolCallId::fixture(id)),
        result_summary: text.map(|text| lash_core::CallResultSummary {
            text: text.to_string(),
            truncated: false,
        }),
    }
}

fn block(calls: &[lash_core::ExecutedCall], max_calls: usize, max_block_bytes: usize) -> String {
    let policy = CallFeedbackPolicy::new(0, max_block_bytes).expect("a valid policy");
    let mut out = String::new();
    write_calls_block(&mut out, calls, max_calls, policy);
    out
}

/// FIG-7530: row and byte bounds never silently lose a completed call.
/// Names fit whole or the notice reports the full earliest/latest ids.
#[test]
fn calls_block_budget_edges_render_exact_omission_counts() {
    let calls = (0..129)
        .map(|index| {
            call(
                &format!("inbox.write_{index}"),
                true,
                &format!("write-{index}"),
                None,
            )
        })
        .collect::<Vec<_>>();
    let rendered = block(&calls, 128, CallFeedbackPolicy::MAX_BLOCK_BYTES);
    let (listing, notice) = rendered.split_once('\n').expect("omission notice");
    let listing: serde_json::Value = serde_json::from_str(listing).expect("calls JSON");
    assert_eq!(
        listing["completed_calls"]
            .as_array()
            .expect("completed")
            .len(),
        128
    );
    assert_eq!(listing["omitted"]["ok"], 1);
    assert!(notice.starts_with("Omitted 1 completed calls:"));
    assert!(notice.contains("inbox.write_128"));
    assert!(notice.contains(calls[128].call_id.as_ref().expect("id").as_str()));

    // With no rows, the standard budget fits every whole operation and id.
    let rendered = block(&calls, 0, 16 * 1024);
    let (_, notice) = rendered.split_once('\n').expect("all names notice");
    for call in &calls {
        assert!(notice.contains(&format!("\"{}\"", call.operation)));
        assert!(notice.contains(call.call_id.as_ref().expect("id").as_str()));
    }

    let minimum = CallFeedbackPolicy::MIN_BLOCK_BYTES;
    for budget in [minimum, minimum + super::row(&calls[0]).len() + 1] {
        let rendered = block(&calls, 128, budget);
        assert!(rendered.len() <= budget, "{} > {budget}", rendered.len());
        let (listing, notice) = rendered.split_once('\n').expect("endpoint notice");
        let listing: serde_json::Value = serde_json::from_str(listing).expect("calls JSON");
        let kept = listing["completed_calls"]
            .as_array()
            .expect("completed")
            .len();
        assert_eq!(listing["omitted"]["ok"], calls.len() - kept);
        assert!(notice.contains(&format!("Omitted {} completed calls;", calls.len() - kept)));
        assert!(
            notice.contains(
                calls[kept]
                    .call_id
                    .as_ref()
                    .expect("first omitted")
                    .as_str()
            )
        );
        assert!(notice.contains(calls[128].call_id.as_ref().expect("last omitted").as_str()));
    }

    // Failure stays outside completed_calls even when admission order interleaves it.
    let mixed = [
        calls[0].clone(),
        call("inbox.fail", false, "failed", None),
        calls[1].clone(),
    ];
    let rendered = block(&mixed, 128, 16 * 1024);
    let listing: serde_json::Value = serde_json::from_str(&rendered).expect("no omissions");
    assert_eq!(
        listing["completed_calls"]
            .as_array()
            .expect("completed")
            .len(),
        2
    );
    assert_eq!(listing["failed_calls"][0]["outcome"], "err");
    assert_eq!(listing["omitted"], serde_json::json!({"ok": 0, "err": 0}));
}

/// FIG-7530: structural shortening preserves whole scalar leaves and escape
/// sequences, marks string lengths and container counts, and obeys every
/// result budget, including zero. No prefix can masquerade as an identity.
#[test]
fn result_summaries_elide_structure_without_splitting_scalar_leaves() {
    let value = serde_json::json!({"b": [1, 2.5, null], "a": "é\"\n"});
    let whole = "{\"a\":\"é\\\"\\n\",\"b\":[1,2.5,null]}";
    assert_eq!(
        summarize(&value, whole.len()),
        lash_core::CallResultSummary {
            text: whole.into(),
            truncated: false,
        }
    );
    let path = format!("wiki/{}", "receipt-".repeat(12));
    let body = "é\"\n".repeat(300);
    let receipt = serde_json::json!({"body": body, "path": path});
    let summary = summarize(&receipt, 512);
    assert!(summary.truncated);
    assert!(summary.text.contains(&path));
    assert!(
        summary
            .text
            .contains(&format!("{} characters", body.chars().count()))
    );
    assert!(!summary.text.contains(&body[..6]));

    let items = (0..50)
        .map(|i| serde_json::json!(format!("id-{i:02}")))
        .collect::<Vec<_>>();
    let array = summarize(&serde_json::json!(items), 128);
    assert!(array.text.contains("\"id-00\""));
    assert!(array.text.contains("items elided"));
    let object = serde_json::Value::Object(
        (0..50)
            .map(|i| (format!("key-{i:02}"), serde_json::json!(i)))
            .collect(),
    );
    let preview = summarize(&object, 128);
    assert!(preview.text.contains("\"key-00\":0"));
    assert!(preview.text.contains("members elided"));
    let identifier = serde_json::json!(path);
    let whole_identifier = identifier.to_string();
    for budget in 0..=512 {
        for value in [&receipt, &serde_json::json!(items), &object] {
            assert!(summarize(value, budget).text.len() <= budget);
        }
        let shortened = summarize(&identifier, budget);
        assert!(shortened.text == whole_identifier || !shortened.text.contains("wiki/"));
    }
    let zero = super::with_result_summaries(
        vec![call("inbox.write", true, "zero-result", Some("previous"))],
        &[lash_core::ToolCallRecord {
            call_id: lash_core::ToolCallId::fixture("zero-result"),
            provider_call_id: None,
            tool: "inbox.write".into(),
            args: serde_json::json!({}),
            output: lash_core::ToolCallOutput::success(receipt),
        }],
        0,
    );
    assert!(zero[0].result_summary.is_none());
    assert_eq!(summarize(&value, 0).text, "");
}

/// FIG-6293: the recorded cuts are refused outside their bounds, through
/// the constructor and through a recorded spelling alike.
#[test]
fn call_feedback_policy_refuses_cuts_outside_its_bounds() {
    let ceiling = CallFeedbackPolicy::MAX_BLOCK_BYTES;
    assert!(CallFeedbackPolicy::new(ceiling, ceiling).is_ok());
    let minimum = CallFeedbackPolicy::MIN_BLOCK_BYTES;
    let id = lash_core::ToolCallId::fixture("maximum-width");
    assert_eq!(id.as_str().len(), lash_core::ToolCallId::ENCODED_BYTES);
    let count = usize::MAX;
    let largest_notice = format!(
        "{{\"completed_calls\":[],\"failed_calls\":[],\"omitted\":{{\"ok\":{count},\"err\":{count}}}}}\nOmitted {count} completed calls; earliest call_id={id}; latest call_id={id}."
    );
    assert_eq!(minimum, largest_notice.len());
    assert!(CallFeedbackPolicy::new(0, minimum).is_ok());
    for max_block_bytes in [0, minimum - 1] {
        let expected = CallFeedbackPolicyError::BlockBelowMinimum {
            max_block_bytes,
            minimum,
        };
        assert_eq!(CallFeedbackPolicy::new(0, max_block_bytes), Err(expected));
        assert!(
            serde_json::from_value::<CallFeedbackPolicy>(serde_json::json!({
                "max_result_bytes": 0, "max_block_bytes": max_block_bytes,
            }))
            .is_err()
        );
    }
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
