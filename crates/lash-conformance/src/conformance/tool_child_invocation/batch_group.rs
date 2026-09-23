//! One cross-tier law: an `All` group of tool children answers every admission
//! shape a batch carries with its own reply, keyed by input index (FIG-3397,
//! ADR 0099 §5, §10).
//!
//! A batch is one effect group of `ToolInvocation` children consumed to
//! exhaustion. What the group decides is the dispatch order it commits in (§5
//! commit order), so `settlement_order` is asserted as a permutation whose
//! preparation-settled positions lead (§10 L5), not as a sequence.

use super::*;

/// A tool id nothing resolves: the call settles during preparation (ADR 0099
/// §10 L5) and leads the settlement order.
const LEAF_ABSENT: &str = "tool:law_absent";

/// Runs one batch under one fresh session through `call_tool_batch`.
///
/// The context is built against the tier's own host — `.effect_host` installs
/// the tool-child host and opener registration on it — so the group path
/// exercises the substrate under test, not an in-memory stand-in.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn run_batch(
    fixture: &ToolChildLawFixture,
    host: &Arc<dyn crate::EffectHost>,
    prefix: &str,
    calls: impl FnOnce(&crate::SessionId) -> Vec<crate::ToolInvocation>,
) -> (crate::SessionId, crate::session::ToolBatchReplies) {
    let session_id = crate::SessionId::from(format!("{prefix}-batch-group"));
    // Call ids carry the session id: an orchestrating leaf derives the process
    // it starts from its call id, so a durable registry shared across runs
    // never sees two starts under one name.
    let calls = calls(&session_id);
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
    let replies = context
        .call_tool_batch(calls, crate::session::ToolGroupOccurrence::Opener(1))
        .await;
    (session_id, replies)
}

/// One batch of every admission shape yields one reply per input.
///
/// The calls cover every admission shape a batch carries — a catalog leaf, a
/// granted leaf, an orchestrating leaf, a leaf whose tool fails, and a leaf
/// whose tool id resolves to nothing — so the law proves the group consumer
/// keeps preparation-prefix settlement (ADR 0099 §10 L5), per-leaf replies
/// keyed by input index, and the settlement-order contract: a permutation of
/// `0..n` whose preparation-settled positions lead.
pub async fn an_all_group_of_tool_children_yields_the_batch_replies(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;

    let calls = |session: &crate::SessionId| {
        vec![
            crate::ToolInvocation::new(
                format!("{session}-plain"),
                crate::ToolId::from(LEAF_PLAIN),
                serde_json::json!({ "leaf": "plain" }),
            ),
            crate::ToolInvocation::new(
                format!("{session}-granted"),
                crate::ToolId::from(LEAF_GRANTED),
                serde_json::json!({ "leaf": "granted" }),
            )
            .with_execution_grant(leaf_grant()),
            crate::ToolInvocation::new(
                format!("{session}-orchestrating"),
                crate::ToolId::from(LEAF_ORCHESTRATING),
                serde_json::json!({ "leaf": "orchestrating" }),
            ),
            crate::ToolInvocation::new(
                format!("{session}-fail"),
                crate::ToolId::from(LEAF_FAIL),
                serde_json::json!({ "leaf": "fail" }),
            ),
            crate::ToolInvocation::new(
                format!("{session}-absent"),
                crate::ToolId::from(LEAF_ABSENT),
                serde_json::json!({ "leaf": "absent" }),
            ),
        ]
    };

    let (session, grouped) = run_batch(fixture, &host, prefix, calls).await;

    assert_eq!(grouped.replies.len(), 5, "the group answers every input");
    for (index, (suffix, reply)) in ["plain", "granted", "orchestrating", "fail", "absent"]
        .into_iter()
        .zip(grouped.replies.iter())
        .enumerate()
    {
        let record = reply
            .record
            .as_ref()
            .unwrap_or_else(|| panic!("reply {index} carries its call record"));
        assert_eq!(
            record.call_id.as_deref(),
            Some(format!("{session}-{suffix}").as_str()),
            "reply {index} is the reply to input {index}"
        );
    }
    assert_eq!(
        grouped.replies[0].output.value_for_projection(),
        serde_json::json!({ "leaf": "plain" }),
        "the catalog leaf answers its own output"
    );
    assert_eq!(
        grouped.replies[1].output.value_for_projection(),
        serde_json::json!({ "leaf": "granted" }),
        "the granted leaf answers its own output"
    );
    assert!(
        grouped.replies[2].output.is_success(),
        "the orchestrating leaf settles through its lane: {:?}",
        grouped.replies[2].output
    );
    assert!(
        !grouped.replies[3].output.is_success(),
        "the failing leaf's rejection is its reply, not an infrastructure failure"
    );
    let absent = grouped.replies[4].output.value_for_projection();
    assert!(
        !grouped.replies[4].output.is_success() && absent.to_string().contains("unavailable"),
        "the unresolved leaf settles during preparation as unavailable: {absent}"
    );

    // §5: the group's durable final-commit order decides the sequence; §10 L5:
    // the preparation-settled unavailable leaf leads it.
    let n = grouped.replies.len();
    let mut grouped_order = grouped.settlement_order.clone();
    grouped_order.sort_unstable();
    assert_eq!(
        grouped_order,
        (0..n).collect::<Vec<_>>(),
        "the group reports every position settled exactly once"
    );
    assert_eq!(
        grouped.settlement_order.first(),
        Some(&(n - 1)),
        "the preparation-settled leaf leads the group's order"
    );
}
