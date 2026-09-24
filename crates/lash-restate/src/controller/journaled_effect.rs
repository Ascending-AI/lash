//! Journaling a runtime effect's recorded outcome.
//!
//! One responsibility: put exactly one entry in an effect's durable journal
//! slot, and decide - before anything runs - when that entry has to be a
//! give-up because the effect could never be journaled at all.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lash_core::{
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectOutcome, facade_support::CanonicalRuntimeEffectEnvelope,
};
use restate_sdk::serde::Json;

use super::context::RestateControllerContext;
use super::effect_journal::JournaledEffectRecord;
use super::journal_budget::{
    JournaledBudgetVerdict, budget_verdict, gave_up_over_budget_entry, group_open_budget_verdict,
    group_open_gave_up_over_budget, journalable_recorded_effect, recorded_effect_from_journal,
    unjournalable_envelope_give_up,
};
use super::{
    RecordedRuntimeEffect, RestateEffectError, RestateRuntimeEffectController, restate_effect_name,
    validate_recorded_effect_envelope,
};
use crate::effect_group::EffectGroupOpenRequest;

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
        metadata: &RuntimeEffectInvocation,
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
    /// process command and then discard the result for the
    /// replayed poison entry.
    ///
    /// So the verdict is journaled unconditionally, in its own slot ahead of the
    /// effect, and the journaled verdict is what decides. `Some` means this
    /// attempt must not run the effect at all.
    pub(super) async fn journaled_budget_give_up<'run>(
        &'run self,
        metadata: &RuntimeEffectInvocation,
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
        let journaled = match self.journal_budget_verdict(&effect_name, verdict).await {
            Ok(journaled) => journaled,
            Err(error) => return Some(Err(error)),
        };
        match journaled {
            JournaledBudgetVerdict::Proceed => None,
            // The verdict entry *is* the fixed-size poison record for this
            // effect, so a give-up still occupies exactly one journal slot -
            // there is no second entry to write, and nothing left that could
            // depend on the budget in force at replay.
            JournaledBudgetVerdict::GaveUpOverBudget { budget } => Some(
                recorded_effect_from_journal(
                    envelope,
                    &effect_name,
                    gave_up_over_budget_entry(budget),
                )
                .map_err(RestateEffectError::Refused),
            ),
        }
    }

    /// The pre-flight gate for an effect-group open, mirroring
    /// [`Self::journaled_budget_give_up`] for the durable process command.
    ///
    /// Opening a group is not a recorded effect: it is a run of engine calls
    /// (probe, dispatch pre-flight, open) whose requests carry every child's
    /// envelope and each land in the journal. A group whose open cannot be
    /// journaled must give up before the first of them, so the verdict is
    /// journaled in its own slot ahead of the open and the journaled verdict
    /// decides: a replay under a larger budget reproduces the give-up instead
    /// of opening the group, and a replayed `Proceed` never turns into a
    /// give-up that abandons a group it already opened. An `Err` means the
    /// open must not reach the engine at all.
    pub(super) async fn refuse_over_budget_group_open<'run>(
        &'run self,
        group: &RuntimeEffectInvocation,
        request: &EffectGroupOpenRequest,
    ) -> Result<(), RuntimeEffectControllerError>
    where
        'ctx: 'run,
    {
        // As for the process command: no configured budget, no slot.
        let Some(payload_budget) = self.options.journaled_effect_byte_budget else {
            return Ok(());
        };
        let group_name = restate_effect_name(group);
        let verdict = group_open_budget_verdict(&group_name, payload_budget, request);
        match self.journal_budget_verdict(&group_name, verdict).await {
            Ok(JournaledBudgetVerdict::Proceed) => Ok(()),
            Ok(JournaledBudgetVerdict::GaveUpOverBudget { budget }) => {
                Err(group_open_gave_up_over_budget(&group_name, budget))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Journal a budget verdict in the `.journal-budget` slot ahead of its
    /// effect, and return the journaled verdict - which, on replay, is the
    /// recorded one rather than the one this attempt computed.
    async fn journal_budget_verdict<'run>(
        &'run self,
        effect_name: &str,
        verdict: JournaledBudgetVerdict,
    ) -> Result<JournaledBudgetVerdict, RestateEffectError>
    where
        'ctx: 'run,
    {
        let verdict_name = format!("{effect_name}.journal-budget");
        let run_retry_policy = self.options.run_retry_policy.clone();
        let Json(journaled) = self
            .context
            .run_json_send(
                verdict_name.clone(),
                run_retry_policy,
                Box::pin(async move { verdict }),
            )
            .await
            .map_err(|source| RestateEffectError::Terminal {
                effect: verdict_name,
                terminal: source,
            })?;
        Ok(journaled)
    }

    pub(super) async fn record_effect<'run>(
        &'run self,
        metadata: &RuntimeEffectInvocation,
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

    /// Execute an eager effect (a durable process command) through the
    /// five-step journaling protocol: canonicalise envelope -> journaled
    /// budget give-up gate -> run work -> record effect -> validate recorded
    /// envelope against reconstructed.
    pub(super) async fn record_eager_effect<'run>(
        &'run self,
        envelope: &RuntimeEffectEnvelope,
        future: Pin<
            Box<
                dyn Future<Output = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>
                    + Send
                    + 'run,
            >,
        >,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>
    where
        'ctx: 'run,
    {
        let reconstructed_envelope = envelope.canonical_form()?;
        let invocation = envelope.invocation.clone();
        let recorded_envelope = Arc::new(reconstructed_envelope.clone());
        let recorded = match self
            .journaled_budget_give_up(&invocation, &recorded_envelope)
            .await
        {
            Some(gave_up) => gave_up,
            None => {
                let outcome = future.await;
                let journaled_envelope = Arc::clone(&recorded_envelope);
                self.record_effect(
                    &invocation,
                    &recorded_envelope,
                    Box::pin(async move {
                        RecordedRuntimeEffect {
                            envelope: journaled_envelope,
                            outcome,
                        }
                    }),
                )
                .await
            }
        }
        .map_err(RuntimeEffectControllerError::from)?;
        validate_recorded_effect_envelope(recorded, &reconstructed_envelope, None)?
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
        recorded_effect_from_journal(envelope, &effect_name, entry)
            .map_err(RestateEffectError::Refused)
    }
}
