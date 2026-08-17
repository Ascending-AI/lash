//! Journaling a runtime effect's recorded outcome.
//!
//! One responsibility: put exactly one entry in an effect's durable journal
//! slot, and decide - before anything runs - when that entry has to be a
//! give-up because the effect could never be journaled at all.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lash_core::{RuntimeInvocation, facade_support::CanonicalRuntimeEffectEnvelope};
use restate_sdk::serde::Json;

use super::context::RestateControllerContext;
use super::journal_budget::{
    JournaledBudgetVerdict, JournaledEffectRecord, budget_verdict, gave_up_over_budget_entry,
    journalable_recorded_effect, recorded_effect_from_journal, unjournalable_envelope_give_up,
};
use super::{
    RecordedRuntimeEffect, RestateEffectError, RestateRuntimeEffectController, restate_effect_name,
};

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// Give up before an effect runs when its envelope alone cannot be
    /// journaled, or `None` when the effect may proceed.
    ///
    /// A poison substitute still has to carry the envelope replay validation
    /// matches on, so an envelope that cannot be journaled leaves no journalable
    /// record at all - substituting the outcome would propose an over-budget
    /// entry the substrate rejects, reviving the redrive loop silently. The
    /// verdict is a pure function of the reconstructed envelope and the
    /// configured budget, and the budget can change between attempts, so the
    /// give-up still occupies its journal slot with the fixed-size poison entry:
    /// the slot exists whatever budget is in force at replay, and the replayed
    /// entry reproduces this same typed failure. Callers that run their effect
    /// outside the run closure MUST consult this before running it - otherwise a
    /// give-up would discard a completed effect that was never journaled, and
    /// the next redrive would execute it again.
    pub(super) async fn journaled_effect_give_up<'run>(
        &'run self,
        metadata: &RuntimeInvocation,
        envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
    ) -> Option<Result<RecordedRuntimeEffect, RestateEffectError>>
    where
        'ctx: 'run,
    {
        let effect_name = restate_effect_name(metadata);
        let budget = unjournalable_envelope_give_up(
            &effect_name,
            self.options.journaled_effect_byte_budget,
            envelope,
        )?;
        Some(
            self.journal_effect_entry(
                effect_name,
                envelope,
                Box::pin(async move { gave_up_over_budget_entry(budget) }),
            )
            .await,
        )
    }

    /// The pre-flight gate for an effect that runs outside the run closure.
    ///
    /// [`Self::journaled_effect_give_up`] is enough for `record_effect`, whose
    /// effect runs inside the run closure: a replay never invokes that closure,
    /// so a replayed give-up cannot execute anything. An eagerly-executed effect
    /// has no such protection - it would run before its journal slot is ever
    /// consulted - and the give-up verdict depends on the configured budget, so
    /// a budget increase between attempts would let the replay execute the
    /// process command or tool batch and then discard the result for the
    /// replayed poison entry.
    ///
    /// So the verdict is journaled unconditionally, in its own slot ahead of the
    /// effect, and the journaled verdict is what decides. `Some` means this
    /// attempt must not run the effect at all.
    pub(super) async fn journaled_budget_give_up<'run>(
        &'run self,
        metadata: &RuntimeInvocation,
        envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
    ) -> Option<Result<RecordedRuntimeEffect, RestateEffectError>>
    where
        'ctx: 'run,
    {
        // With no budget configured no give-up is reachable, so there is no
        // verdict to pin and the slot is not taken. The journal shape therefore
        // depends only on whether the budget feature is enabled for the
        // deployment - never on the budget's value, which is the flip that
        // re-executed effects - and a journal written before this seam existed
        // keeps replaying entry for entry.
        let payload_budget = self.options.journaled_effect_byte_budget?;
        let effect_name = restate_effect_name(metadata);
        let verdict = budget_verdict(&effect_name, Some(payload_budget), envelope);
        let verdict_name = format!("{effect_name}.journal-budget");
        let run_retry_policy = self.options.run_retry_policy.clone();
        let journaled = self
            .context
            .run_json_send(
                verdict_name.clone(),
                run_retry_policy,
                Box::pin(async move { verdict }),
            )
            .await;
        let Json(journaled) = match journaled {
            Ok(journaled) => journaled,
            Err(source) => {
                return Some(Err(RestateEffectError::Terminal {
                    effect: verdict_name,
                    terminal: source,
                }));
            }
        };
        match journaled {
            JournaledBudgetVerdict::Proceed => None,
            // The verdict entry *is* the fixed-size poison record for this
            // effect, so a give-up still occupies exactly one journal slot -
            // there is no second entry to write, and nothing left that could
            // depend on the budget in force at replay.
            JournaledBudgetVerdict::GaveUpOverBudget { budget } => {
                Some(Ok(recorded_effect_from_journal(
                    envelope,
                    &effect_name,
                    gave_up_over_budget_entry(budget),
                )))
            }
        }
    }

    pub(super) async fn record_effect<'run>(
        &'run self,
        metadata: &RuntimeInvocation,
        envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
        // Keep the full journaled-effect executor behind one allocation. The
        // Restate SDK stores this future in its ctx.run state machine, so
        // accepting it inline here makes every composed turn carry the whole
        // executor frame through the durable adapter.
        future: Pin<Box<dyn Future<Output = RecordedRuntimeEffect> + Send + 'run>>,
    ) -> Result<RecordedRuntimeEffect, RestateEffectError>
    where
        'ctx: 'run,
    {
        if let Some(give_up) = self.journaled_effect_give_up(metadata, envelope).await {
            return give_up;
        }
        let effect_name = restate_effect_name(metadata);
        let payload_budget = self.options.journaled_effect_byte_budget;
        let poisoned_effect_name = effect_name.clone();
        self.journal_effect_entry(
            effect_name,
            envelope,
            Box::pin(async move {
                journalable_recorded_effect(&poisoned_effect_name, payload_budget, future.await)
            }),
        )
        .await
    }

    /// Occupy this effect's journal slot with the entry the future yields, and
    /// reconstruct the recorded effect the journaled entry stands for.
    pub(super) async fn journal_effect_entry<'run>(
        &'run self,
        effect_name: String,
        envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
        future: Pin<Box<dyn Future<Output = JournaledEffectRecord> + Send + 'run>>,
    ) -> Result<RecordedRuntimeEffect, RestateEffectError>
    where
        'ctx: 'run,
    {
        let run_retry_policy = self.options.run_retry_policy.clone();
        let Json(entry) = self
            .context
            .run_json_send(effect_name.clone(), run_retry_policy, future)
            .await
            .map_err(|source| RestateEffectError::Terminal {
                effect: effect_name.clone(),
                terminal: source,
            })?;
        Ok(recorded_effect_from_journal(envelope, &effect_name, entry))
    }
}
