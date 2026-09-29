use std::num::NonZeroUsize;
use std::sync::Arc;

use super::*;
use lash_core::llm::types::ProviderReplayMeta;

fn max(members: usize) -> NonZeroUsize {
    NonZeroUsize::new(members).expect("a non-zero maximum")
}

fn call(call_id: &str, tool_name: &str, args: Value) -> PendingToolCall {
    PendingToolCall {
        call_id: lash_core::ToolCallId::fixture(call_id),
        provider_call_id: Some(call_id.to_string()),
        tool_name: tool_name.to_string(),
        args,
        replay: None,
    }
}

/// The fixture label of `id`: a model call's own label, or a batch member's
/// `{wrapper}/batch/{index}`, the member id being the wrapper id's child.
fn label(id: &lash_core::ToolCallId) -> &'static str {
    for base in ["native-a", "native-b", "n", "w", "w1", "w2", "over", "at"] {
        let wrapper = lash_core::ToolCallId::fixture(base);
        if *id == wrapper {
            return base;
        }
        for index in 0..=64 {
            if *id == wrapper.child(index) {
                return Box::leak(format!("{base}/batch/{index}").into_boxed_str());
            }
        }
    }
    panic!("an unlabelled call id: {id}")
}

fn wrapper(call_id: &str, members: &[&str]) -> PendingToolCall {
    PendingToolCall {
        replay: Some(ProviderReplayMeta {
            item_id: Some(format!("provider-{call_id}")),
            ..ProviderReplayMeta::default()
        }),
        ..call(
            call_id,
            BATCH_TOOL_NAME,
            serde_json::json!({
                "tool_calls": members
                    .iter()
                    .enumerate()
                    .map(|(index, tool)| serde_json::json!({
                        "tool": tool,
                        "parameters": { "member": index },
                    }))
                    .collect::<Vec<_>>()
            }),
        )
    }
}

fn answered(slot: &PendingToolCall, output: ToolCallOutput) -> CompletedToolCall {
    CompletedToolCall {
        call_id: slot.call_id.clone(),
        provider_call_id: slot.provider_call_id.clone(),
        tool_name: slot.tool_name.clone(),
        args: slot.args.clone(),
        model_return: ModelToolReturn::from_output(slot.tool_name.clone(), &output),
        output,
        intent_outcomes: Vec::new(),
        replay: slot.replay.clone(),
    }
}

fn succeeded(slot: &PendingToolCall) -> CompletedToolCall {
    answered(
        slot,
        ToolCallOutput::success(serde_json::json!({ "from": label(&slot.call_id) })),
    )
}

#[test]
fn the_definition_renders_the_configured_maximum() {
    for members in [1, 8, 64] {
        let definition = batch_tool_definition(max(members));
        let schema = &definition.contract().input_schema.canonical;
        assert_eq!(
            schema["properties"]["tool_calls"]["maxItems"],
            serde_json::json!(members)
        );
        assert!(
            definition.description().contains(&format!("1-{members} ")),
            "{}",
            definition.description()
        );
    }
}

#[test]
fn the_definition_documents_the_results_array() {
    let definition = batch_tool_definition(max(64));
    assert_eq!(
        definition.contract().output_schema.canonical["required"],
        serde_json::json!(["results"])
    );
}

#[test]
fn members_take_consecutive_slots_at_their_wrappers_position() {
    let expansion = expand(
        vec![
            call("native-a", "list", serde_json::json!({})),
            wrapper("w1", &["read", "search"]),
            call("native-b", "list", serde_json::json!({})),
            wrapper("w2", &["read"]),
        ],
        max(64),
    );
    assert!(expansion.refused.is_empty());
    let slots = expansion
        .calls
        .iter()
        .map(|slot| (label(&slot.call_id), slot.tool_name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        slots,
        vec![
            ("native-a", "list"),
            ("w1/batch/0", "read"),
            ("w1/batch/1", "search"),
            ("native-b", "list"),
            ("w2/batch/0", "read"),
        ]
    );
    assert_eq!(
        expansion.calls[1].args,
        serde_json::json!({ "member": 0 }),
        "a member runs with its own parameters"
    );
    assert!(
        expansion.calls[1].replay.is_none(),
        "provider replay stays on the wrapper"
    );
    let positions = expansion
        .plan
        .wrappers
        .iter()
        .map(|wrapper| wrapper.source_position)
        .collect::<Vec<_>>();
    assert_eq!(positions, vec![1, 3]);
    assert_eq!(
        expansion.plan.wrappers[0].rows,
        vec![
            ExpandedRow::Slot {
                member_index: 0,
                tool: "read".to_string(),
                slot: 1,
            },
            ExpandedRow::Slot {
                member_index: 1,
                tool: "search".to_string(),
                slot: 2,
            },
        ]
    );
    assert_eq!(
        expansion.plan.wrappers[0].replay,
        Some(ProviderReplayMeta {
            item_id: Some("provider-w1".to_string()),
            ..ProviderReplayMeta::default()
        })
    );
}

#[test]
fn a_nested_batch_is_a_refused_row_and_takes_no_slot() {
    let expansion = expand(vec![wrapper("w", &["read", "batch", "search"])], max(64));
    assert_eq!(expansion.calls.len(), 2);
    assert_eq!(
        expansion.plan.wrappers[0].rows[1],
        ExpandedRow::Refused {
            member_index: 1,
            tool: "batch".to_string(),
            error: serde_json::json!("`batch` cannot run inside `batch`"),
        }
    );
    assert_eq!(
        expansion.plan.wrappers[0].rows[2],
        ExpandedRow::Slot {
            member_index: 2,
            tool: "search".to_string(),
            slot: 1,
        }
    );
}

#[test]
fn a_wrapper_over_its_maximum_is_refused_whole() {
    let members = vec!["read"; 65];
    let expansion = expand(
        vec![wrapper("over", &members), wrapper("at", &vec!["read"; 64])],
        max(64),
    );
    assert_eq!(expansion.refused.len(), 1);
    assert_eq!(label(&expansion.refused[0].0.call_id), "over");
    assert!(!expansion.refused[0].1.is_success());
    assert_eq!(
        expansion.calls.len(),
        64,
        "no member of the refused wrapper starts"
    );
    assert_eq!(expansion.plan.wrappers.len(), 1);
    assert_eq!(
        expansion.plan.wrappers[0].source_position, 0,
        "a refused wrapper takes no position in the step"
    );

    let expansion = expand(vec![wrapper("over", &["read"; 3])], max(2));
    assert_eq!(expansion.refused.len(), 1, "the configured maximum binds");
}

#[test]
fn a_structurally_malformed_wrapper_is_refused_whole() {
    for args in [
        serde_json::json!({}),
        serde_json::json!({ "tool_calls": "read" }),
        serde_json::json!({ "tool_calls": [] }),
        serde_json::json!({ "tool_calls": [ "read" ] }),
        serde_json::json!({ "tool_calls": [ { "parameters": {} } ] }),
        serde_json::json!({ "tool_calls": [ { "tool": "  ", "parameters": {} } ] }),
        serde_json::json!({ "tool_calls": [ { "tool": "read", "parameters": {} }, 7 ] }),
    ] {
        let expansion = expand(vec![call("w", BATCH_TOOL_NAME, args.clone())], max(64));
        assert_eq!(expansion.refused.len(), 1, "{args}");
        assert!(expansion.calls.is_empty(), "{args}");
        assert!(expansion.plan.is_empty(), "{args}");
    }
}

#[test]
fn a_member_without_parameters_runs_with_an_empty_object() {
    let expansion = expand(
        vec![call(
            "w",
            BATCH_TOOL_NAME,
            serde_json::json!({ "tool_calls": [ { "tool": "read" } ] }),
        )],
        max(64),
    );
    assert_eq!(expansion.calls[0].args, serde_json::json!({}));
}

#[test]
fn the_fold_answers_one_call_per_response_call_in_response_order() {
    let expansion = expand(
        vec![
            call("native-a", "list", serde_json::json!({})),
            wrapper("w", &["read", "batch", "search"]),
            call("native-b", "list", serde_json::json!({})),
        ],
        max(64),
    );
    let completed = expansion
        .calls
        .iter()
        .map(|slot| {
            if label(&slot.call_id) == "w/batch/2" {
                answered(
                    slot,
                    ToolCallOutput::failure(ToolFailure::runtime(
                        ToolFailureClass::InvalidRequest,
                        "schema",
                        "bad arguments",
                    )),
                )
            } else {
                succeeded(slot)
            }
        })
        .collect();
    let folded = fold(&expansion.plan, completed);
    let ids = folded
        .iter()
        .map(|call| label(&call.call_id))
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["native-a", "w", "native-b"]);

    let wrapper = &folded[1];
    assert_eq!(wrapper.tool_name, BATCH_TOOL_NAME);
    assert_eq!(
        wrapper.replay,
        Some(ProviderReplayMeta {
            item_id: Some("provider-w".to_string()),
            ..ProviderReplayMeta::default()
        }),
        "the wrapper keeps the provider's replay metadata"
    );
    assert!(
        wrapper.output.is_success(),
        "a wrapper succeeds even when rows fail"
    );
    assert!(
        wrapper.output.control.is_none(),
        "the fold promotes no member control"
    );
    let rows: Vec<BatchResultRow> =
        serde_json::from_value(wrapper.output.value_for_projection()["results"].clone())
            .expect("rows decode");
    assert_eq!(
        rows.iter()
            .map(|row| (row.index, row.tool.as_str(), row.success))
            .collect::<Vec<_>>(),
        vec![(0, "read", true), (1, "batch", false), (2, "search", false)]
    );
    assert_eq!(rows[0].value(), &serde_json::json!({ "from": "w/batch/0" }));

    let [ModelToolReturnPart::Text { text }] = wrapper.model_return.parts.as_slice() else {
        panic!("one text block: {:?}", wrapper.model_return.parts);
    };
    let presented: Value = serde_json::from_str(text).expect("the presentation is JSON");
    assert_eq!(
        presented["results"][0]["result"],
        serde_json::json!({ "from": "w/batch/0" }),
        "a member's JSON presentation is embedded as JSON"
    );
    assert_eq!(wrapper.provider_call_id.as_deref(), Some("w"));
}

#[test]
fn a_wrapper_whose_members_were_all_refused_folds_with_no_slots() {
    let expansion = expand(vec![wrapper("w", &["batch", "batch"])], max(64));
    assert!(expansion.calls.is_empty());
    let folded = fold(&expansion.plan, Vec::new());
    assert_eq!(folded.len(), 1);
    let rows = folded[0].output.value_for_projection()["results"].clone();
    assert_eq!(rows.as_array().map(Vec::len), Some(2));
    assert_eq!(rows[1]["success"], serde_json::json!(false));
}

#[test]
fn the_fold_carries_member_attachments_after_the_rows() {
    let expansion = expand(vec![wrapper("w", &["read", "read"])], max(64));
    let attachment = ModelToolReturnPart::Attachment(lash_core::AttachmentSource::ExternalUrl {
        media_type: lash_core::MediaType::parse("image/png").expect("a media type"),
        url: "https://example.test/a.png".to_string(),
    });
    let mut completed = expansion.calls.iter().map(succeeded).collect::<Vec<_>>();
    completed[1].model_return.parts.push(attachment.clone());
    let folded = fold(&expansion.plan, completed);
    assert_eq!(folded[0].model_return.parts.len(), 2);
    assert_eq!(folded[0].model_return.parts[1], attachment);
}

#[test]
fn the_fold_is_a_pure_function_of_plan_and_slots() {
    let expansion = expand(
        vec![
            wrapper("w", &["read", "search"]),
            call("n", "list", serde_json::json!({})),
        ],
        max(64),
    );
    let completed = expansion.calls.iter().map(succeeded).collect::<Vec<_>>();
    let first = fold(&expansion.plan, completed.clone());
    let second = fold(&expansion.plan, completed);
    assert_eq!(
        serde_json::to_value(&first).expect("folded calls serialize"),
        serde_json::to_value(&second).expect("folded calls serialize")
    );
}

#[test]
fn batch_config_ceiling_is_refused_at_build() {
    use lash_core::plugin::PluginFactory;

    let build = |members: usize| {
        let factory: Arc<dyn PluginFactory> =
            Arc::new(crate::StandardProtocolPluginFactory::with_config(
                crate::StandardProtocolConfig::default().batch(crate::BatchSugar::Enabled {
                    max_members: max(members),
                }),
            ));
        lash_core::facade_support::PluginHost::new(vec![factory]).build_session("root")
    };
    let refused = build(65)
        .err()
        .expect("a maximum above the ceiling is refused");
    assert!(
        matches!(
            refused,
            lash_core::plugin::PluginError::InvalidBatchMaximum {
                requested: 65,
                ceiling: 64
            }
        ),
        "{refused}"
    );
    for members in [1, 32, 64] {
        build(members).expect("a maximum within the ceiling builds");
    }
    assert_eq!(
        crate::BatchSugar::default(),
        crate::BatchSugar::Enabled {
            max_members: max(crate::BATCH_MEMBER_CEILING)
        },
        "batch is on by default at the ceiling"
    );
}
