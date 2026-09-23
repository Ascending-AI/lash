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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncorporationLedger {
    pub incorporated: BTreeSet<SettlementSource>,
    pub usage_charged: BTreeSet<UsageDeltaIdentity>,
}

impl IncorporationLedger {
    /// Whether nothing has been incorporated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.incorporated.is_empty() && self.usage_charged.is_empty()
    }

    /// Adopt everything `other` incorporated. Incorporation only ever grows a
    /// ledger, so a restored snapshot is merged, never assigned.
    pub fn absorb(&mut self, other: Self) {
        self.incorporated.extend(other.incorporated);
        self.usage_charged.extend(other.usage_charged);
    }
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

    /// ADR 0099 §6/§8: journals the opener's incorporated settlement prefix
    /// of `handle`'s group — ranks `already + 1 ..= handle.consumed()` — and
    /// applies each settled rank's facts through
    /// [`incorporate_tool_settlement`](Self::incorporate_tool_settlement).
    ///
    /// The journaled outcome records exactly which ranks were incorporated,
    /// so a replay re-incorporates the recorded prefix and never a rank that
    /// settled after the record was cut: a late settlement is not early
    /// possession. A call whose `consumed` adds no new rank journals nothing
    /// and returns empty — the ledger already names the prefix.
    ///
    /// Non-tool children carry no settlement facts; their rank is still
    /// recorded in the ledger so the prefix stays contiguous.
    pub async fn incorporate_group_prefix(
        &self,
        handle: &crate::EffectGroupHandle,
    ) -> Result<Vec<crate::runtime::effect::IncorporatedGroupRank>, RuntimeEffectControllerError>
    {
        let group_key = handle.group_key().to_string();
        let through_rank = u64::try_from(handle.consumed()).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                format!("group {group_key} consumed rank does not fit u64: {error}"),
            )
        })?;
        self.incorporate_group_prefix_through(group_key, through_rank)
            .await
    }

    /// ADR 0099 §7 step 2, as
    /// [`OpenerFinalizationSteps`](crate::runtime::effect::OpenerFinalizationSteps)
    /// defines it:
    /// incorporate *every* settled rank of `group_key`, not just the prefix a
    /// consumer cursor reached. Step 1 has already ranked every accepted
    /// child, so the settled ranks are a contiguous `1..=through_rank` whose
    /// end a scan to the first missing rank names.
    ///
    /// The journaled `IncorporateGroupSettlements` record makes the step
    /// idempotent and cursor-resumable: a re-run whose ranks are already in
    /// the ledger journals nothing and re-incorporates only what the record
    /// names.
    pub async fn incorporate_group_outcome(
        &self,
        group_key: &str,
    ) -> Result<Vec<crate::runtime::effect::IncorporatedGroupRank>, RuntimeEffectControllerError>
    {
        let scoped = self.dispatch.effect_controller.scoped();
        let controller = scoped.controller();
        let mut through_rank = 0u64;
        while controller
            .read_group_settlement(group_key, through_rank + 1)
            .await?
            .is_some()
        {
            through_rank += 1;
        }
        self.incorporate_group_prefix_through(group_key.to_string(), through_rank)
            .await
    }

    /// The shared body of [`incorporate_group_prefix`](Self::incorporate_group_prefix)
    /// and [`incorporate_group_outcome`](Self::incorporate_group_outcome):
    /// journal the prefix record for `already + 1 ..= through_rank`, then
    /// apply exactly the ranks the record names.
    async fn incorporate_group_prefix_through(
        &self,
        group_key: String,
        through_rank: u64,
    ) -> Result<Vec<crate::runtime::effect::IncorporatedGroupRank>, RuntimeEffectControllerError>
    {
        // Incorporated ranks of one group are always a contiguous prefix —
        // this method is the only writer and it walks ranks in order — so the
        // count IS the next unincorporated rank minus one.
        let already = {
            let ledger = self.incorporation_ledger().lock_recover();
            u64::try_from(
                ledger
                    .incorporated
                    .iter()
                    .filter(|source| {
                        matches!(
                            source,
                            SettlementSource::GroupRank {
                                group_key: key,
                                ..
                            } if *key == group_key
                        )
                    })
                    .count(),
            )
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!("group {group_key} incorporated count does not fit u64: {error}"),
                )
            })?
        };
        if through_rank <= already {
            return Ok(Vec::new());
        }
        // The effect id names the rank range it covers, so each prefix
        // extension is a distinct journaled record and a replay derives the
        // same id from the same ledger state.
        let effect_id = format!(
            "effect-group-incorporate:{group_key}:{}-{through_rank}",
            already + 1
        );
        let invocation = self.language_runtime_invocation(&effect_id);
        let scoped = self.dispatch.effect_controller.scoped();
        let controller = scoped.controller();
        // Read the prefix's settlements before the record, on the live run
        // and on every replay alike. A settled rank is immutable, so these
        // reads return the same thing each time; issuing them inside the
        // local executor instead would issue them only on the live run, and a
        // host whose journal is positional (Restate) would find the replay
        // taking a different command path from the run it replays.
        let mut prefix = std::collections::BTreeMap::new();
        for rank in (already + 1)..=through_rank {
            let settlement = controller
                .read_group_settlement(&group_key, rank)
                .await?
                .ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                        format!(
                            "effect group {group_key} rank {rank} was inside the consumed \
                             prefix but has no recorded settlement"
                        ),
                    )
                })?;
            prefix.insert(rank, settlement);
        }
        let read_prefix = prefix
            .iter()
            .map(
                |(rank, settlement)| crate::runtime::effect::IncorporatedGroupRank {
                    rank: *rank,
                    child_replay_key: settlement.child_replay_key.clone(),
                },
            )
            .collect::<Vec<_>>();
        let local_executor =
            crate::RuntimeEffectLocalExecutor::language_runtime_value_with(move |envelope| {
                let incorporated = read_prefix.clone();
                async move {
                    crate::runtime::effect::refuse_unhonored_group_membership(
                        envelope.group.as_deref(),
                        "group settlement incorporation",
                    )?;
                    let crate::RuntimeEffectCommand::IncorporateGroupSettlements { .. } =
                        envelope.command
                    else {
                        return Err(RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                            format!(
                                "group incorporation executor cannot execute {} command",
                                envelope.command.kind().as_str()
                            ),
                        ));
                    };
                    Ok(crate::RuntimeEffectOutcome::IncorporateGroupSettlements { incorporated })
                }
            });
        let outcome = scoped
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::IncorporateGroupSettlements {
                        group_key: group_key.clone(),
                        through_rank,
                    },
                ),
                local_executor,
            )
            .await?;
        // Live and replay converge here: the recorded outcome names exactly
        // the ranks to apply, so the live run and every replay incorporate
        // the same prefix — a settlement that landed after the record is not
        // in `incorporated` and is never applied.
        let incorporated = outcome.into_incorporate_group_settlements()?;
        for entry in &incorporated {
            let settlement = prefix.remove(&entry.rank).ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    format!(
                        "effect group {group_key} rank {} was recorded as incorporated \
                         outside the prefix {}..={through_rank} this record covers",
                        entry.rank,
                        already + 1
                    ),
                )
            })?;
            let source = SettlementSource::GroupRank {
                group_key: group_key.clone(),
                rank: entry.rank,
                child_replay_key: entry.child_replay_key.clone(),
            };
            match settlement.outcome {
                Ok(crate::RuntimeEffectOutcome::ToolInvocation { settlement, .. }) => {
                    self.incorporate_tool_settlement(source, &settlement)?;
                }
                // A non-tool child, or a child whose terminal is a recorded
                // error, carries no settlement facts; the rank still joins
                // the incorporated prefix so the next record starts after it.
                _ => {
                    self.incorporation_ledger()
                        .lock_recover()
                        .incorporated
                        .insert(source);
                }
            }
        }
        Ok(incorporated)
    }

    /// The once-only ledger this context incorporates against. Behind a
    /// method so `incorporate_tool_settlement` and the handover carriage both
    /// reach the same `Arc`.
    pub(crate) fn incorporation_ledger(&self) -> &Arc<std::sync::Mutex<IncorporationLedger>> {
        &self.incorporation_ledger
    }
}

/// The §7 finalization steps a real opener hands the closing driver
/// (FIG-3410/FIG-3411): step 2, `commit_outcome_and_accounting`, is
/// [`RuntimeExecutionContext::incorporate_group_outcome`] — every settled rank
/// incorporated through the journaled `IncorporateGroupSettlements` record,
/// which makes the step idempotent and cursor-resumable as
/// [`OpenerFinalizationSteps`](crate::runtime::effect::OpenerFinalizationSteps)
/// requires.
///
/// Step 3 — the parent's end record — is the exit path's own; when it supplies
/// `parent_end` this delegates to it, and without one the step is the no-op a
/// group with no opener record owes.
pub struct ContextFinalizationSteps<'a, 'run> {
    context: &'a RuntimeExecutionContext<'run>,
    parent_end: Option<&'a dyn crate::runtime::effect::OpenerFinalizationSteps>,
}

impl<'a, 'run> ContextFinalizationSteps<'a, 'run> {
    pub fn new(context: &'a RuntimeExecutionContext<'run>) -> Self {
        Self {
            context,
            parent_end: None,
        }
    }

    /// Compose the parent's end-record step: the turn or process exit that
    /// owns the opener's end record (FIG-3397) supplies it here.
    pub fn with_parent_end(
        mut self,
        parent_end: &'a dyn crate::runtime::effect::OpenerFinalizationSteps,
    ) -> Self {
        self.parent_end = Some(parent_end);
        self
    }
}

#[async_trait::async_trait]
impl crate::runtime::effect::OpenerFinalizationSteps for ContextFinalizationSteps<'_, '_> {
    async fn commit_outcome_and_accounting(
        &self,
        group_key: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.context
            .incorporate_group_outcome(group_key)
            .await
            .map(|_| ())
    }

    async fn record_parent_end(&self, group_key: &str) -> Result<(), RuntimeEffectControllerError> {
        match self.parent_end {
            Some(parent_end) => parent_end.record_parent_end(group_key).await,
            None => Ok(()),
        }
    }
}
