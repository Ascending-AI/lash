//! The session's recorded `max_tool_calls` (ADR 0099 §9, FIG-4546): a cell's
//! total, and what a process holds at once.

use super::*;
use crate::core_internal::RuntimeExecutionContextRuntimeOps as _;

fn env(max_tool_calls: usize) -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(max_tool_calls),
        ),
    )
}

/// One cell of a turn that records `max_tool_calls`.
fn cell(max_tool_calls: usize) -> RuntimeExecutionContext<'static> {
    crate::testing::TestExecutionContextBuilder::over_controller(std::sync::Arc::new(
        crate::testing::UnavailableEffectController,
    )
        as std::sync::Arc<dyn crate::RuntimeEffectController>)
    .execution_env_spec(env(max_tool_calls))
    .build()
    .into_runtime()
    .with_opener_state(OpenerState::default())
}

/// One segment of a process whose environment records `max_tool_calls`.
fn process(max_tool_calls: usize) -> RuntimeExecutionContext<'static> {
    let registration = crate::ProcessRegistration::new(
        crate::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    );
    cell(max_tool_calls).with_process_execution(
        crate::process_id_for_test("max-tool-calls"),
        &registration,
        None,
    )
}

fn exceeded(error: &RuntimeEffectControllerError) -> crate::ToolCallLimitExceeded {
    assert_eq!(error.code, crate::RuntimeErrorCode::MaxToolCallsExceeded);
    error
        .tool_call_limit_exceeded()
        .expect("the refusal carries its typed cause")
}

/// A cell's limit is its total: the call past it is refused with the typed
/// cause and a message that names the limit, the calls before it stand, and
/// consuming a group gives nothing back.
#[tokio::test]
async fn a_cell_counts_every_call_it_makes_and_refuses_the_one_past_its_limit() {
    let context = cell(4);
    context
        .reserve_tool_calls("g1", 3)
        .await
        .expect("three of four fit");
    context
        .reserve_tool_calls("g1", 3)
        .await
        .expect("a group formed again is the same calls");
    let refused = context
        .reserve_tool_calls("g2", 2)
        .await
        .expect_err("five of four do not fit");
    assert_eq!(
        exceeded(&refused),
        crate::ToolCallLimitExceeded {
            scope: crate::ToolCallLimitScope::Cell,
            limit: crate::MaxToolCalls::new(4),
            counted: 3,
            requested: 2,
        }
    );
    assert!(
        refused.message.contains("max_tool_calls = 4"),
        "the refusal names the limit: {}",
        refused.message
    );
    assert_eq!(context.tool_call_limit_refusal(), Some(exceeded(&refused)));

    // A consumed group releases what a process holds; a cell's total stands.
    context.release_group_work("g1");
    context
        .reserve_tool_calls("g3", 1)
        .await
        .expect("the fourth call fits");
    let again = context
        .reserve_tool_calls("g4", 1)
        .await
        .expect_err("the fifth call is past the total");
    assert_eq!(exceeded(&again).counted, 4);
}

/// A fresh context is a fresh cell: the limit is per cell, not per turn.
#[tokio::test]
async fn each_cell_starts_its_own_count() {
    let opener = OpenerState::default();
    let first = cell(2).with_opener_state(opener.clone());
    first.reserve_tool_calls("g1", 2).await.expect("fits");
    assert!(first.reserve_tool_calls("g2", 1).await.is_err());

    let second = cell(2).with_opener_state(opener);
    second
        .reserve_tool_calls("g2", 2)
        .await
        .expect("the next cell of the same turn has its whole limit");
}

/// A process's limit is what it holds at once: the call past it is refused
/// while the held calls are held, and admitted once they are consumed.
#[tokio::test]
async fn a_process_is_refused_past_what_it_holds_and_admitted_after_it_consumes() {
    let context = process(2);
    context.reserve_tool_calls("g1", 2).await.expect("fits");
    context
        .reserve_tool_calls("g1", 2)
        .await
        .expect("a replayed formation reuses its reservation");
    let refused = context
        .reserve_tool_calls("g2", 1)
        .await
        .expect_err("a third held call is past the limit");
    assert_eq!(
        exceeded(&refused),
        crate::ToolCallLimitExceeded {
            scope: crate::ToolCallLimitScope::Process,
            limit: crate::MaxToolCalls::new(2),
            counted: 2,
            requested: 1,
        }
    );
    context.release_group_work("g1");
    context
        .reserve_tool_calls("g2", 2)
        .await
        .expect("the consumed calls are no longer held");
}

#[tokio::test]
async fn run_boundary_restores_cursor_reservation_and_incorporated_prefix() {
    let opener = OpenerState::default();
    let predecessor = process(4).with_opener_state(opener.clone());
    predecessor
        .reserve_tool_calls("race", 3)
        .await
        .expect("fits");
    predecessor.retain_outstanding_group(
        crate::EffectGroupHandle::restored("race", 4, 1).expect("valid cursor"),
    );
    let prefix = super::super::SettlementSource::GroupRank {
        group_key: "race".to_string(),
        rank: 1,
        child_replay_key: "timer".to_string(),
    };
    opener
        .ledger
        .lock_recover()
        .incorporated
        .insert(prefix.clone());
    let recorded = serde_json::to_vec(&opener.snapshot()).expect("serializable Run state");
    let successor =
        OpenerState::from_snapshot(serde_json::from_slice(&recorded).expect("stored Run state"))
            .expect("reattach");
    assert!(successor.ledger_snapshot().incorporated.contains(&prefix));
    let context = process(4).with_opener_state(successor);
    let groups = context.outstanding_groups_snapshot();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].consumed(), 1);
    context
        .reserve_tool_calls("race", 3)
        .await
        .expect("same reservation");
    context
        .reserve_tool_calls("new", 1)
        .await
        .expect("one call remains");
    assert!(context.reserve_tool_calls("past-limit", 1).await.is_err());
}

#[test]
fn malformed_run_boundary_cursor_refuses_recovery() {
    let snapshot = crate::store::RunOpenerState {
        run: None,
        incorporation: Default::default(),
        groups: vec![crate::store::RunOpenerGroup {
            group_key: "race".to_string(),
            children: 1,
            consumed: 2,
            held_tool_calls: 1,
        }],
    };
    let error = OpenerState::from_snapshot(snapshot).expect_err("an impossible prefix is refused");
    assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
}
