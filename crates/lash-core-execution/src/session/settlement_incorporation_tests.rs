//! The §6/§13 incorporation laws, exercised against the applicator itself
//! (FIG-3411 phase 1).
//!
//! What is asserted:
//!
//! * incorporating one settlement twice is a no-op the second time —
//!   possession granted once, messages enqueued once, trigger receipts
//!   restored once, usage charged once;
//! * a usage delta is charged once per [`UsageDeltaIdentity`]: the same
//!   `(attempt, llm_call_id, provider_attempt)` arriving twice on the same
//!   settlement's carriers is one charge, while a different provider attempt
//!   of the same call — the billed failure the retry replaced — is its own
//!   fact and is charged separately;
//! * a cancel-decided child's settlement carries its known usage and nothing
//!   else: the spend is charged, no possession is granted and no messages are
//!   enqueued;
//! * the incorporation ledger survives the boundary it travels with: a
//!   successor context restored from the handover snapshot refuses to
//!   re-apply a settlement the predecessor already incorporated.

use std::sync::{Arc, Mutex};

use lash_sansio::core_support::ModelToolReturnCoreSupport as _;
use lash_sansio::sync::MutexExt as _;

use crate::runtime::effect::{TOOL_SETTLEMENT_VERSION, ToolSettlement, ToolUsageDelta};
use crate::session::{IncorporationLedger, SettlementSource, UsageChargeSink, UsageDeltaIdentity};
use crate::{LlmCallId, PluginMessage, TokenUsage};

/// The session-ledger stand-in: records every charge the applicator makes as
/// `(source, model, usage)` so the tests can count them by identity.
#[derive(Default)]
struct RecordingCharge {
    charges: Mutex<Vec<(String, String, TokenUsage)>>,
}

impl UsageChargeSink for RecordingCharge {
    fn charge(
        &self,
        source: &str,
        model: &str,
        usage: &crate::TokenUsage,
    ) -> Result<(), crate::PluginError> {
        self.charges
            .lock_recover()
            .push((source.to_string(), model.to_string(), usage.clone()));
        Ok(())
    }
}

fn context_with_charge(charge: Arc<RecordingCharge>) -> crate::RuntimeExecutionContext<'static> {
    crate::testing::TestExecutionContextBuilder::over_controller(std::sync::Arc::new(
        crate::testing::UnavailableEffectController,
    )
        as std::sync::Arc<dyn crate::RuntimeEffectController>)
    .plugin_factories(Vec::new())
    .direct_completions(
        crate::DirectCompletionClient::unavailable("incorporation test context")
            .with_usage_charge_sink(charge),
    )
    .build()
    .into_runtime()
}

fn delta(attempt: u32, call_id: &str, provider_attempt: u32, input_tokens: i64) -> ToolUsageDelta {
    ToolUsageDelta {
        attempt,
        llm_call_id: LlmCallId(call_id.to_string()),
        provider_attempt,
        source: "managed".to_string(),
        model: "test-model".to_string(),
        usage: TokenUsage {
            input_tokens,
            ..TokenUsage::default()
        },
    }
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
        usage: vec![delta(0, "call-a", 1, 41)],
        stream: crate::runtime::effect::RecordedChildStream::default(),
        model_return: crate::ModelToolReturn::text("call".to_string(), "tool".to_string(), "ok"),
    }
}

fn source() -> SettlementSource {
    SettlementSource::Invocation {
        call_id: "call-1".to_string(),
        replay_key: "replay-1".to_string(),
    }
}

/// §6: the applicator is once-only per source — the second incorporation of
/// the same settlement is the no-op the ledger exists to make.
#[test]
fn a_settlement_is_incorporated_exactly_once() {
    let charge = Arc::new(RecordingCharge::default());
    let context = context_with_charge(Arc::clone(&charge));
    let settlement = settlement();

    let first = context
        .incorporate_tool_settlement(source(), &settlement)
        .expect("first incorporation applies");
    assert_eq!(first.messages, 1);
    assert_eq!(first.possession.len(), 1);
    assert_eq!(first.usage_charged, 1);
    assert!(
        context
            .started_process_ids()
            .contains(&crate::process_id_for_test("child-process"))
    );
    assert_eq!(context.dispatch.checkpoint_messages.drain().len(), 1);
    assert_eq!(charge.charges.lock_recover().len(), 1);

    let second = context
        .incorporate_tool_settlement(source(), &settlement)
        .expect("second incorporation is the no-op");
    assert_eq!(second.messages, 0);
    assert!(second.possession.is_empty());
    assert_eq!(second.usage_charged, 0);
    assert!(context.dispatch.checkpoint_messages.drain().is_empty());
    assert_eq!(
        charge.charges.lock_recover().len(),
        1,
        "the second incorporation charges nothing"
    );
}

/// §13: a spend is identified by `(attempt, llm_call_id, provider_attempt)`
/// on the settlement it rode in on. A duplicated attach of the same identity
/// is deduplicated; a different provider attempt of the same call — the
/// billed failure — is a separate fact and is charged separately.
#[test]
fn usage_is_charged_once_per_delta_identity_across_carriers() {
    let charge = Arc::new(RecordingCharge::default());
    let context = context_with_charge(Arc::clone(&charge));
    let mut settlement = settlement();
    // The same fact arriving twice — the attempt capture and the settlement
    // both naming it — is one charge; the billed second provider attempt of
    // the same call is its own charge.
    settlement.usage = vec![
        delta(0, "call-a", 1, 41),
        delta(0, "call-a", 1, 41),
        delta(0, "call-a", 2, 11),
    ];

    let incorporated = context
        .incorporate_tool_settlement(source(), &settlement)
        .expect("incorporation applies");
    assert_eq!(incorporated.usage_charged, 2);
    assert_eq!(incorporated.usage_deduplicated, 1);
    let charges = charge.charges.lock_recover();
    assert_eq!(charges.len(), 2);
    assert_eq!(charges[0].2.input_tokens, 41);
    assert_eq!(charges[1].2.input_tokens, 11);
    assert!(
        charges
            .iter()
            .all(|(source, model, _)| source == "managed" && model == "test-model"),
        "each delta charges under the (source, model) the live path would have used"
    );
}

/// §13: a cancel-decided child's known usage is still charged — the spend a
/// provider reported before the decision is a fact the settlement keeps even
/// when no value was accepted, no messages committed and nothing is possessed.
#[test]
fn a_cancelled_childs_known_usage_is_still_charged() {
    let charge = Arc::new(RecordingCharge::default());
    let context = context_with_charge(Arc::clone(&charge));
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
    assert_eq!(incorporated.usage_charged, 1);
    assert_eq!(incorporated.messages, 0);
    assert!(incorporated.possession.is_empty());
    assert_eq!(charge.charges.lock_recover().len(), 1);
    assert!(context.started_process_ids().is_empty());
    assert!(context.dispatch.checkpoint_messages.drain().is_empty());
}

/// §6: the ledger travels with the handover — a successor context restored
/// from the predecessor's snapshot does not re-apply the settlement, so a
/// crash between an attempt's commit and the settlement's incorporation
/// cannot double-apply what the journaled carrier already delivered.
#[test]
fn a_committed_attempt_before_settlement_loses_nothing_across_a_crash() {
    let charge = Arc::new(RecordingCharge::default());
    let predecessor = context_with_charge(Arc::clone(&charge));
    let settlement = settlement();
    predecessor
        .incorporate_tool_settlement(source(), &settlement)
        .expect("the predecessor incorporates before the crash");

    let handover: IncorporationLedger = predecessor.incorporation_ledger_snapshot();
    let successor = context_with_charge(Arc::clone(&charge));
    successor.restore_incorporation_ledger(handover);
    // A raw usage identity is carried too: a delta the predecessor charged is
    // refused a second charge even before its settlement source is replayed.
    let already_charged = UsageDeltaIdentity {
        source: source(),
        attempt: 0,
        llm_call_id: LlmCallId("call-a".to_string()),
        provider_attempt: 1,
    };
    assert!(
        successor
            .incorporation_ledger()
            .lock_recover()
            .usage_charged
            .contains(&already_charged),
        "the charged-delta identity survives the handover"
    );

    let replayed = successor
        .incorporate_tool_settlement(source(), &settlement)
        .expect("the successor's re-incorporation is the no-op");
    assert_eq!(replayed.usage_charged, 0);
    assert_eq!(replayed.messages, 0);
    assert!(replayed.possession.is_empty());
    assert_eq!(
        charge.charges.lock_recover().len(),
        1,
        "across the handover the settlement was incorporated exactly once"
    );
}
