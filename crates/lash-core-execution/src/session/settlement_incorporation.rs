//! The opener-side incorporation of a recorded tool settlement (ADR 0099 §6,
//! §13; FIG-3411).
//!
//! One operation — [`RuntimeExecutionContext::incorporate_tool_settlement`] —
//! applies every semantic channel a settlement carries exactly once per
//! [`SettlementSource`]: possession is granted, committed checkpoint messages
//! are enqueued, trigger receipts are restored as evidence, and each usage
//! delta is charged into the session token ledger under the `(source, model)`
//! the live path would have used. It never executes a declaration, never
//! emits a delivery, and never re-runs a projector.
//!
//! Idempotence is carried, not hoped for: [`IncorporationLedger`] records the
//! incorporated sources and the [`UsageDeltaIdentity`]s already charged, and
//! travels with the execution context wherever `started_process_ids` does, so
//! a redrive or a segment handover cannot incorporate the same settlement
//! twice or double-charge a delta that already reached the ledger.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use super::execution_context::RuntimeExecutionContext;
use crate::runtime::effect::ToolSettlement;
use crate::runtime::effect::executor::RuntimeEffectControllerError;
use crate::{LlmCallId, ProcessId};

/// Which recorded settlement is being incorporated — its once-only identity
/// (ADR 0099 §6).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SettlementSource {
    /// A scalar or batch tool call the opener admitted live; its invocation
    /// identity is the replay key.
    Invocation { call_id: String, replay_key: String },
    /// Rank `rank` of durable effect group `group_key`, settled by child
    /// `child_replay_key`.
    GroupRank {
        group_key: String,
        rank: u64,
        child_replay_key: String,
    },
}

/// The identity of one usage delta across every carrier it can arrive on
/// (§13): the settlement it rode in on, the attempt ordinal stamped at
/// capture, and the ADR 0032 `(llm_call_id, provider_attempt)` pair the
/// provider's own record is named by. A delta already in
/// [`IncorporationLedger::usage_charged`] is a second attach of a known fact,
/// charged once.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UsageDeltaIdentity {
    pub source: SettlementSource,
    pub attempt: u32,
    pub llm_call_id: LlmCallId,
    pub provider_attempt: u32,
}

/// What the opener has incorporated so far: the once-only set and the usage
/// identities already charged. Travels with the execution context across
/// segment handover beside `started_process_ids`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IncorporationLedger {
    pub incorporated: BTreeSet<SettlementSource>,
    pub usage_charged: BTreeSet<UsageDeltaIdentity>,
}

/// What one incorporation applied, in counts. A source already in the ledger
/// returns every count as zero — the second call is the no-op the ledger
/// exists to make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Incorporated {
    pub source: Option<SettlementSource>,
    pub possession: Vec<ProcessId>,
    pub messages: usize,
    pub triggers: usize,
    pub usage_charged: usize,
    pub usage_deduplicated: usize,
}

/// The charge a settlement's usage deltas land in: the opener's session token
/// ledger, reached through the same capability the live direct-completion
/// path writes (`usage_capability.record_token_usage`). Not a second ledger —
/// the same destination, offered as a narrow sink so the execution context
/// never names the ledger type itself.
pub trait UsageChargeSink: Send + Sync {
    fn charge(
        &self,
        source: &str,
        model: &str,
        usage: &crate::TokenUsage,
    ) -> Result<(), crate::PluginError>;
}

impl<'run> RuntimeExecutionContext<'run> {
    /// ADR 0099 §6/§13: applies a settlement's recorded semantic deltas
    /// exactly once. Never executes a declaration, never emits a delivery,
    /// never re-runs a projector. A `source` already in the ledger returns an
    /// [`Incorporated`] whose counts are all zero.
    pub fn incorporate_tool_settlement(
        &self,
        source: SettlementSource,
        settlement: &ToolSettlement,
    ) -> Result<Incorporated, RuntimeEffectControllerError> {
        settlement.validate()?;
        let mut ledger = self.incorporation_ledger().lock_recover();
        if ledger.incorporated.contains(&source) {
            return Ok(Incorporated {
                source: Some(source),
                ..Incorporated::default()
            });
        }
        // Usage first: it is the only fallible step, and a failure before any
        // buffer mutation leaves the source unincorporated so a retry replays
        // the whole incorporation rather than half of it. A delta already
        // charged is skipped by identity — the additive ledger merge becomes
        // idempotent by `UsageDeltaIdentity`.
        let mut usage_charged = 0usize;
        let mut usage_deduplicated = 0usize;
        if !settlement.usage.is_empty() {
            let sink = self
                .dispatch
                .direct_completions
                .usage_charge_sink()
                .ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                        format!(
                            "settlement {source:?} carries usage but this context has no \
                                 session-ledger charge sink; a known spend is refused rather than \
                                 dropped"
                        ),
                    )
                })?;
            for delta in &settlement.usage {
                let identity = UsageDeltaIdentity {
                    source: source.clone(),
                    attempt: delta.attempt,
                    llm_call_id: delta.llm_call_id.clone(),
                    provider_attempt: delta.provider_attempt,
                };
                if ledger.usage_charged.contains(&identity) {
                    usage_deduplicated += 1;
                    continue;
                }
                sink.charge(&delta.source, &delta.model, &delta.usage)
                    .map_err(|error| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                            format!(
                                "settlement {source:?} usage delta {identity:?} could not be \
                                 charged into the session ledger: {error}"
                            ),
                        )
                    })?;
                ledger.usage_charged.insert(identity);
                usage_charged += 1;
            }
        }
        // Possession is granted from the settlement's own realized
        // `possession` — the identities the child's intent outcomes bound —
        // never re-derived from intent outcomes here (§6).
        self.restore_started_process_ids(&settlement.possession);
        self.dispatch
            .checkpoint_messages
            .enqueue(settlement.checkpoint_messages.clone());
        self.restore_tool_trigger_outcomes(settlement.triggers.clone());
        ledger.incorporated.insert(source.clone());
        Ok(Incorporated {
            source: Some(source),
            possession: settlement.possession.clone(),
            messages: settlement.checkpoint_messages.len(),
            triggers: settlement.triggers.len(),
            usage_charged,
            usage_deduplicated,
        })
    }

    /// The once-only ledger this context incorporates against. Behind a
    /// method so `incorporate_tool_settlement` and the handover carriage both
    /// reach the same `Arc`.
    pub(crate) fn incorporation_ledger(&self) -> &Arc<std::sync::Mutex<IncorporationLedger>> {
        &self.incorporation_ledger
    }
}
