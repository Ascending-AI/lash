//! The §6 incorporation laws, exercised against the applicator itself
//! (FIG-3411 phase 1).
//!
//! What is asserted:
//!
//! * incorporating one settlement twice is a no-op the second time —
//!   possession granted once, messages enqueued once, trigger receipts
//!   restored once;
//! * a cancel-decided child's settlement applies nothing: no possession is
//!   granted and no messages are enqueued. Its spend is its own `ToolAttempt`
//!   run's, delivered by the engine (ADR 0125), never the settlement's;
//! * the incorporation ledger survives the boundary it travels with: a
//!   successor context restored from the handover snapshot refuses to
//!   re-apply a settlement the predecessor already incorporated.

use lash_sansio::core_support::ModelToolReturnCoreSupport as _;

use crate::PluginMessage;
use crate::runtime::effect::{TOOL_SETTLEMENT_VERSION, ToolSettlement};
use crate::session::{IncorporationLedger, SettlementSource};

fn context() -> crate::RuntimeExecutionContext<'static> {
    crate::testing::TestExecutionContextBuilder::over_controller(std::sync::Arc::new(
        crate::testing::UnavailableEffectController,
    )
        as std::sync::Arc<dyn crate::RuntimeEffectController>)
    .plugin_factories(Vec::new())
    .direct_completions(crate::DirectCompletionClient::unavailable(
        "incorporation test context",
    ))
    .build()
    .into_runtime()
}

fn settlement() -> ToolSettlement {
    ToolSettlement {
        version: TOOL_SETTLEMENT_VERSION,
        intent_outcomes: Vec::new(),
        possession: vec![crate::process_id_for_test("child-process")],
        triggers: Vec::new(),
        checkpoint_messages: vec![PluginMessage::text(
            crate::MessageRole::User,
            "committed mid-attempt",
        )],
        stream: crate::runtime::effect::RecordedChildStream::default(),
        model_return: crate::ModelToolReturn::text("tool".to_string(), "ok"),
    }
}

fn source() -> SettlementSource {
    SettlementSource::Invocation {
        call_id: crate::ToolCallId::fixture("call-1"),
    }
}

/// §6: the applicator is once-only per source — the second incorporation of
/// the same settlement is the no-op the ledger exists to make.
#[test]
fn a_settlement_is_incorporated_exactly_once() {
    let context = context();
    let settlement = settlement();

    let first = context
        .incorporate_tool_settlement(source(), &settlement)
        .expect("first incorporation applies");
    assert_eq!(first.messages, 1);
    assert_eq!(first.possession.len(), 1);
    assert!(
        context
            .started_process_ids()
            .contains(&crate::process_id_for_test("child-process"))
    );
    assert_eq!(context.dispatch.checkpoint_messages.drain().len(), 1);

    let second = context
        .incorporate_tool_settlement(source(), &settlement)
        .expect("second incorporation is the no-op");
    assert_eq!(second.messages, 0);
    assert!(second.possession.is_empty());
    assert!(context.dispatch.checkpoint_messages.drain().is_empty());
}

/// A cancel-decided child accepted no value, committed no messages and
/// possesses nothing, so its settlement applies nothing.
#[test]
fn a_cancelled_childs_settlement_applies_nothing() {
    let context = context();
    let mut settlement = settlement();
    settlement.possession = Vec::new();
    settlement.checkpoint_messages = Vec::new();
    settlement.triggers = Vec::new();

    let incorporated = context
        .incorporate_tool_settlement(
            SettlementSource::GroupRank {
                group_key: "group".to_string(),
                rank: 0,
                child_replay_key: "child".to_string(),
            },
            &settlement,
        )
        .expect("a cancelled child's settlement still incorporates");
    assert_eq!(incorporated.messages, 0);
    assert!(incorporated.possession.is_empty());
    assert!(context.started_process_ids().is_empty());
    assert!(context.dispatch.checkpoint_messages.drain().is_empty());
}

/// §6: the ledger travels with the handover — a successor context restored
/// from the predecessor's snapshot does not re-apply the settlement, so a
/// crash between an attempt's commit and the settlement's incorporation
/// cannot double-apply what the journaled carrier already delivered.
#[test]
fn a_committed_attempt_before_settlement_loses_nothing_across_a_crash() {
    let predecessor = context();
    let settlement = settlement();
    predecessor
        .incorporate_tool_settlement(source(), &settlement)
        .expect("the predecessor incorporates before the crash");

    let handover: IncorporationLedger = predecessor.incorporation_ledger_snapshot();
    let successor = context();
    successor.restore_incorporation_ledger(handover);

    let replayed = successor
        .incorporate_tool_settlement(source(), &settlement)
        .expect("the successor's re-incorporation is the no-op");
    assert_eq!(replayed.messages, 0);
    assert!(replayed.possession.is_empty());
    assert!(successor.dispatch.checkpoint_messages.drain().is_empty());
}
