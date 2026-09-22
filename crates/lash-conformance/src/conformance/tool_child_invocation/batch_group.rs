//! One cross-tier differential law: an `All` group of tool children yields
//! the batch replies the pre-group `ToolBatch` effect produced (FIG-3397,
//! ADR 0099 §5).
//!
//! The predecessor path — `call_tool_batch_via_batch_effect`, one recorded
//! `ToolBatch` effect whose local executor ran the leaves — and the group
//! path — one effect group of `ToolInvocation` children consumed to
//! exhaustion — must answer the same replies for the same calls. What is
//! allowed to differ is the dispatch order the group commits in (§5 commit
//! order): `settlement_order` is asserted as a set, not a sequence.

use super::*;

/// A tool id nothing resolves: the call settles during preparation (ADR 0099
/// §10 L5) and leads the settlement order on both paths.
const LEAF_ABSENT: &str = "tool:law_absent";

/// Runs one batch under one fresh session through whichever `call_tool_batch`
/// entry point the caller names.
///
/// The context is built against the tier's own host — `.effect_host` installs
/// the tool-child host and opener registration on it — so the group path
/// exercises the substrate under test, not an in-memory stand-in.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn run_batch<F>(
    fixture: &ToolChildLawFixture,
    host: &Arc<dyn crate::EffectHost>,
    prefix: &str,
    session_suffix: &str,
    calls: Vec<crate::ToolInvocation>,
    run: F,
) -> (crate::SessionId, crate::session::ToolBatchReplies)
where
    F: for<'a> FnOnce(
        &'a crate::RuntimeExecutionContext<'static>,
        Vec<crate::ToolInvocation>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = crate::session::ToolBatchReplies> + Send + 'a>,
    >,
{
    let session_id = crate::SessionId::from(format!("{prefix}-batch-group-{session_suffix}"));
    let scenario = scenario(
        fixture,
        &session_id,
        serde_json::json!({"lane": "batch-group"}),
    )
    .await;
    let scope = crate::ExecutionScope::turn(
        session_id.clone(),
        crate::TurnId::from(format!("{session_id}-turn")),
    );
    let admitted = crate::admit(scope);
    let controller = host
        .scoped_static(admitted)
        .expect("the host lends a scoped controller")
        .expect("this host hands out owned scoped controllers");
    let tool_registry = crate::ToolRegistry::from_tool_provider_with_orchestrating_tools(
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        vec![law_orchestrating_tool()],
    )
    .expect("the law's leaf provider and orchestrating tool register disjoint ids");
    let mut definitions = leaf_definitions();
    definitions.push(crate::ToolDefinition::raw(
        LEAF_FAIL,
        LEAF_FAIL.trim_start_matches("tool:"),
        "conformance failing leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    ));
    definitions.push(crate::ToolDefinition::raw(
        LEAF_ORCHESTRATING,
        LEAF_ORCHESTRATING.trim_start_matches("tool:"),
        "conformance orchestrating leaf",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    ));
    let context = crate::testing::TestExecutionContextBuilder::new()
        .session_id(session_id.clone())
        .provider(Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>)
        .tool_catalog(crate::ToolCatalog::from_tool_definitions(definitions))
        .tool_registry(Arc::new(tool_registry))
        .processes(crate::testing::effect_backed_process_service(
            scenario.registry,
        ))
        .direct_completions(crate::DirectCompletionClient::from_fn(
            |request, _source| {
                Ok(if request.model == "law-billed-model" {
                    law_billed_completion()
                } else {
                    law_direct_completion()
                })
            },
        ))
        .process_env_store(scenario.process_env_store)
        .effect_host(Arc::clone(host))
        .borrowed_effect_controller(controller)
        .build()
        .into_runtime();
    let replies = run(&context, calls).await;
    (session_id, replies)
}

/// Replaces every embedded mention of `session_id` in a projected output —
/// scope ids and derived names carry it — so the two halves of the
/// differential, which run on deliberately distinct sessions, compare like
/// for like. Digits are scrubbed too: scope ids embed registry sequence
/// numbers, which legitimately differ because each half stands up its own
/// scenario.
fn normalize_session(value: &mut serde_json::Value, session_id: &crate::SessionId) {
    let session = session_id.to_string();
    match value {
        serde_json::Value::String(text) => {
            if text.contains(session.as_str()) {
                *text = text.replace(session.as_str(), "<session>");
            }
            if text.chars().any(|ch| ch.is_ascii_digit()) {
                *text = text
                    .chars()
                    .map(|ch| if ch.is_ascii_digit() { '#' } else { ch })
                    .collect();
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_session(item, session_id);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                normalize_session(item, session_id);
            }
        }
        _ => {}
    }
}

/// One batch through both `call_tool_batch` paths yields the same replies.
///
/// The calls cover every admission shape a batch carries — a catalog leaf, a
/// granted leaf, an orchestrating leaf, a leaf whose tool fails, and a leaf
/// whose tool id resolves to nothing — so the differential proves the group
/// consumer preserves preparation-prefix settlement (ADR 0099 §10 L5),
/// per-leaf replies keyed by input index, and the settlement-order contract:
/// a permutation of `0..n` whose preparation-settled positions lead.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_all_group_of_tool_children_yields_the_batch_replies(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;

    let calls = || {
        vec![
            crate::ToolInvocation::new(
                format!("{prefix}-plain"),
                crate::ToolId::from(LEAF_PLAIN),
                serde_json::json!({ "leaf": "plain" }),
            ),
            crate::ToolInvocation::new(
                format!("{prefix}-granted"),
                crate::ToolId::from(LEAF_GRANTED),
                serde_json::json!({ "leaf": "granted" }),
            )
            .with_execution_grant(leaf_grant()),
            crate::ToolInvocation::new(
                format!("{prefix}-orchestrating"),
                crate::ToolId::from(LEAF_ORCHESTRATING),
                serde_json::json!({ "leaf": "orchestrating" }),
            ),
            crate::ToolInvocation::new(
                format!("{prefix}-fail"),
                crate::ToolId::from(LEAF_FAIL),
                serde_json::json!({ "leaf": "fail" }),
            ),
            crate::ToolInvocation::new(
                format!("{prefix}-absent"),
                crate::ToolId::from(LEAF_ABSENT),
                serde_json::json!({ "leaf": "absent" }),
            ),
        ]
    };

    let (predecessor_session, predecessor) = run_batch(
        fixture,
        &host,
        prefix,
        "predecessor",
        calls(),
        |ctx, calls| {
            Box::pin(async move {
                ctx.call_tool_batch_via_batch_effect(
                    calls,
                    crate::session::ToolBatchOccurrence::Opener(1),
                )
                .await
            })
        },
    )
    .await;
    let (grouped_session, grouped) =
        run_batch(fixture, &host, prefix, "grouped", calls(), |ctx, calls| {
            Box::pin(async move {
                ctx.call_tool_batch(calls, crate::session::ToolBatchOccurrence::Opener(1))
                    .await
            })
        })
        .await;

    assert_eq!(
        predecessor.replies.len(),
        grouped.replies.len(),
        "both paths answer every input"
    );
    let projected = |reply: &crate::ToolInvocationReply,
                     session: &crate::SessionId|
     -> (serde_json::Value, serde_json::Value) {
        let mut reply_projection = reply.output.value_for_projection();
        normalize_session(&mut reply_projection, session);
        let mut record_projection = reply
            .record
            .as_ref()
            .map(|record| {
                let mut value = record.output.value_for_projection();
                normalize_session(&mut value, session);
                value
            })
            .unwrap_or_default();
        normalize_session(&mut record_projection, session);
        (reply_projection, record_projection)
    };
    for (index, (before, after)) in predecessor
        .replies
        .iter()
        .zip(grouped.replies.iter())
        .enumerate()
    {
        assert_eq!(
            projected(before, &predecessor_session),
            projected(after, &grouped_session),
            "reply {index} output agrees across paths"
        );
        match (&before.record, &after.record) {
            (Some(before), Some(after)) => {
                assert_eq!(before.call_id, after.call_id, "reply {index} call id");
                assert_eq!(before.tool, after.tool, "reply {index} tool");
                assert_eq!(before.args, after.args, "reply {index} args");
            }
            (None, None) => {}
            _ => panic!("reply {index} record presence differs across paths"),
        }
    }

    // §5: the group's durable final-commit order may differ from the
    // predecessor's source order, so the order is asserted as a set. The
    // preparation-settled unavailable leaf leads in both (§10 L5).
    let n = predecessor.replies.len();
    let mut predecessor_order = predecessor.settlement_order.clone();
    let mut grouped_order = grouped.settlement_order.clone();
    predecessor_order.sort_unstable();
    grouped_order.sort_unstable();
    assert_eq!(
        predecessor_order,
        (0..n).collect::<Vec<_>>(),
        "the predecessor reports every position settled"
    );
    assert_eq!(
        grouped_order,
        (0..n).collect::<Vec<_>>(),
        "the group reports every position settled"
    );
    let absent_index = n - 1;
    assert_eq!(
        predecessor.settlement_order.first(),
        Some(&absent_index),
        "the preparation-settled leaf leads the predecessor's order"
    );
    assert_eq!(
        grouped.settlement_order.first(),
        Some(&absent_index),
        "the preparation-settled leaf leads the group's order"
    );
}
