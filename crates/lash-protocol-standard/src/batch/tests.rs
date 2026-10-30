use lash_core::plugin::PluginSessionRequest;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::*;
use crate::round::{ResponseCall, RoundPlan, RoundRules, plan_round};
use lash_core::llm::types::ProviderReplayMeta;
use lash_core::sansio::PendingToolCall;
use lash_core::{ToolFailure, ToolFailureClass};

fn max(members: usize) -> NonZeroUsize {
    NonZeroUsize::new(members).expect("a non-zero maximum")
}

/// FIG-7506: successive preambles must not regenerate their batch schema.
#[test]
fn a_batch_definition_builds_its_schema_once() {
    let first = batch_tool_definition(max(37));
    let built = SCHEMA_BUILDS.with(std::cell::Cell::get);
    let second = batch_tool_definition(max(37));
    assert_eq!(SCHEMA_BUILDS.with(std::cell::Cell::get), built);
    assert_eq!(first, second);
    assert_eq!(
        second.contract().input_schema.canonical()["properties"]["tool_calls"]["maxItems"],
        37
    );
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
        read: None,
        control_origin: lash_core::sansio::ToolControlOrigin::OwnCall,
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
        let expansion = expand_plain(vec![call("w", BATCH_TOOL_NAME, args.clone())], max(64));
        assert_eq!(expansion.refused.len(), 1, "{args}");
        assert!(expansion.calls.is_empty(), "{args}");
        assert!(expansion.plan.is_empty(), "{args}");
    }
}

#[test]
fn a_member_without_parameters_runs_with_an_empty_object() {
    let expansion = expand_plain(
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
    let expansion = expand_plain(
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
            .map(|row| (
                row.index,
                row.tool.as_str(),
                matches!(row.outcome, ToolCallOutcome::Success(_))
            ))
            .collect::<Vec<_>>(),
        vec![(0, "read", true), (1, "batch", false), (2, "search", false)]
    );
    let ToolCallOutcome::Success(value) = &rows[0].outcome else {
        panic!("successful read");
    };
    assert_eq!(
        value.to_json_value(),
        serde_json::json!({ "from": "w/batch/0" })
    );
    let schema = lash_kernel_value::JsonSchema::admit(batch_output_schema()).expect("batch schema");
    schema
        .validate(&wrapper.output.value_for_projection())
        .expect("typed outcomes match their schema");

    let [ModelToolReturnPart::Text { text }] = wrapper.model_return.parts.as_slice() else {
        panic!("one text block: {:?}", wrapper.model_return.parts);
    };
    let presented: Value = serde_json::from_str(text).expect("the presentation is JSON");
    assert_eq!(
        presented["results"][0]["body"],
        serde_json::json!({ "from": "w/batch/0" }).to_string(),
        "a member's presentation remains text"
    );
    assert_eq!(
        presented["results"][2]["body"], "bad arguments",
        "an unpresented failure retains its committed explanation"
    );
    assert_eq!(wrapper.provider_call_id.as_deref(), Some("w"));
}

/// Cancellation must remain distinct from failure, and an interrupted member
/// must retain its committed cause even when no presenter ran (FIG-7488).
#[test]
fn the_fold_preserves_committed_stops_without_presenters() {
    let expansion = expand_plain(vec![wrapper("w", &["read", "read"])], max(64));
    let outputs = [
        ToolCallOutput::cancelled(lash_protocol_types::ToolCancellation::runtime("run closed")),
        ToolCallOutput::failure(
            ToolFailure::runtime(
                ToolFailureClass::Execution,
                "tool_interrupted",
                "interrupted",
            )
            .with_cause(lash_protocol_types::ToolFailureCause::Interrupted),
        ),
    ];
    let completed = expansion
        .calls
        .iter()
        .zip(outputs)
        .map(|(slot, output)| {
            let mut completed = answered(slot, output);
            completed.model_return.parts.clear();
            completed
        })
        .collect();
    let folded = fold(&expansion.plan, completed);
    let stored = folded[0].output.value_for_projection();
    lash_kernel_value::JsonSchema::admit(batch_output_schema())
        .expect("batch output schema")
        .validate(&stored)
        .expect("committed stop outcomes match their schema");
    assert_eq!(stored["results"][0]["outcome"]["status"], "cancelled");
    assert_eq!(
        stored["results"][1]["outcome"]["payload"]["cause"]["kind"],
        "interrupted"
    );
    let [ModelToolReturnPart::Text { text }] = folded[0].model_return.parts.as_slice() else {
        panic!("one text block");
    };
    let presented: Value = serde_json::from_str(text).expect("batch presentation");
    lash_kernel_value::JsonSchema::admit(
        schemars::schema_for!(Vec<ModelBatchResultRow>).to_value(),
    )
    .expect("model row schema")
    .validate(&presented["results"])
    .expect("presented rows match their own schema");
    assert_eq!(presented["results"][0]["outcome"]["status"], "cancelled");
    assert_eq!(presented["results"][0]["body"], "run closed");
    assert_eq!(
        presented["results"][1]["outcome"]["payload"]["cause"]["kind"],
        "interrupted"
    );
    assert_eq!(presented["results"][1]["body"], "interrupted");
}

#[test]
fn the_fold_carries_member_attachments_after_the_rows() {
    let expansion = expand_plain(vec![wrapper("w", &["read", "read"])], max(64));
    let attachment = ModelToolReturnPart::Attachment(lash_core::AttachmentRef::new(
        lash_core::AttachmentId::parse(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("digest"),
        lash_core::MediaType::parse("image/png").expect("a media type"),
        3,
        None,
        None,
    ));
    let mut completed = expansion.calls.iter().map(succeeded).collect::<Vec<_>>();
    completed[1].model_return.parts.push(attachment.clone());
    let folded = fold(&expansion.plan, completed);
    assert_eq!(folded[0].model_return.parts.len(), 2);
    assert_eq!(folded[0].model_return.parts[1], attachment);
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
        lash_core::facade_support::PluginHost::new(
            vec![factory],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        )
        .build_session(PluginSessionRequest::creation(
            "root",
            lash_core::plugin::SessionAuthorityContext::fixture(),
        ))
    };
    let refused = build(65)
        .err()
        .expect("a maximum above the ceiling is refused");
    assert!(
        matches!(
            refused,
            lash_execution_contracts::plugin_error::PluginError::InvalidBatchMaximum {
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

/// The round planned for `calls` in a catalog where no tool ends the turn.
fn expand_plain(calls: Vec<PendingToolCall>, max_members: NonZeroUsize) -> RoundPlan {
    plan_round(
        calls
            .into_iter()
            .map(|call| ResponseCall {
                input_json: call.args.to_string(),
                call,
                parse_error: None,
            })
            .collect(),
        &RoundRules {
            batch: crate::BatchSugar::Enabled { max_members },
            listed: None,
            ends_the_turn: &|_| false,
            operation: &str::to_owned,
        },
    )
}
