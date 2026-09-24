//! The durable journal payload budget for recorded effects.
//!
//! One responsibility: decide whether a recorded effect can reach the durable
//! journal at all, and render the typed give-up when it cannot.

use std::sync::Arc;

use lash_core::{
    RuntimeEffectControllerError, RuntimeErrorCode, facade_support::CanonicalRuntimeEffectEnvelope,
};
use serde::{Deserialize, Serialize};

use std::fmt;

use super::effect_journal::{
    GaveUpEntry, JournaledEffectRecord, RecordedRuntimeEffect, retired_generation_refusal, stamped,
};
use crate::effect_group::EffectGroupOpenRequest;

/// The pre-flight budget verdict for an effect that runs outside the run
/// closure.
///
/// Those effects cannot be executed inside `ctx.run` - their own journal
/// commands have to stay at the handler's journal level - so nothing else stops
/// a replay from running them again. The verdict therefore occupies a journal
/// slot of its own ahead of the effect, unconditionally: whatever budget the
/// replaying attempt is configured with, the journaled verdict is what decides,
/// so a replayed give-up never executes the effect first and a replayed
/// `Proceed` never turns into a give-up that discards finished work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum JournaledBudgetVerdict {
    Proceed,
    GaveUpOverBudget { budget: u64 },
}

/// Journal the result before acting on it.
pub(super) fn budget_verdict(
    effect: &str,
    payload_budget: Option<u64>,
    envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
) -> JournaledBudgetVerdict {
    match unjournalable_envelope_give_up(effect, payload_budget, envelope) {
        Some(budget) => JournaledBudgetVerdict::GaveUpOverBudget { budget },
        None => JournaledBudgetVerdict::Proceed,
    }
}

/// Measure a recorded effect's journal payload without allocating it, and stop
/// serializing as soon as it cannot be journaled.
struct JournalPayloadMeter {
    written: u64,
    budget: Option<u64>,
    exceeded: bool,
}

impl std::io::Write for JournalPayloadMeter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(buf.len() as u64);
        if let Some(budget) = self.budget
            && self.written > budget
        {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "recorded effect exceeded its durable journal payload budget",
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Why a recorded effect cannot reach the durable journal.
///
/// Both reasons render to a message whose length depends only on the effect name
/// and the configured budget, never on an underlying error's text. That keeps a
/// poison record's serialized size a pure function of its envelope, which is
/// what lets [`unjournalable_envelope_give_up`] prove the substitute fits before
/// the effect runs. The unbounded detail goes to the operator log instead.
#[derive(Clone, Copy)]
enum PoisonReason {
    OverBudget { budget: u64 },
    Unserializable,
}

impl fmt::Display for PoisonReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverBudget { budget } => write!(
                f,
                "its payload exceeded the {budget}-byte durable journal budget"
            ),
            Self::Unserializable => f.write_str("its payload cannot be serialized"),
        }
    }
}

fn poisoned_effect_error(effect: &str, reason: PoisonReason) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RestateJournaledEffectPoisoned,
        format!("journaled effect `{effect}` gave up because {reason}"),
    )
}

fn poisoned_effect_record(
    effect: &str,
    envelope: Arc<CanonicalRuntimeEffectEnvelope>,
    reason: PoisonReason,
) -> RecordedRuntimeEffect {
    RecordedRuntimeEffect {
        envelope,
        outcome: Err(poisoned_effect_error(effect, reason)),
    }
}

/// The pre-flight budget verdict for an effect-group open.
///
/// The open's journaled payload is its request: the group's shape, whose
/// membership retains every child's canonical envelope. The dispatch
/// pre-flight call carries the same children unescaped, so the open request is
/// the largest entry the open proposes, and a verdict that clears it clears the
/// whole open.
pub(super) fn group_open_budget_verdict(
    group: &str,
    payload_budget: u64,
    request: &EffectGroupOpenRequest,
) -> JournaledBudgetVerdict {
    match record_exceeds_budget(Some(payload_budget), request) {
        Err((true, _)) => {
            tracing::error!(
                %group,
                budget = %payload_budget,
                "effect group open exceeds the durable journal budget; giving up before the group is opened"
            );
            JournaledBudgetVerdict::GaveUpOverBudget {
                budget: payload_budget,
            }
        }
        _ => JournaledBudgetVerdict::Proceed,
    }
}

/// The typed give-up a journaled over-budget group-open verdict renders: the
/// same failure an over-budget recorded effect reports.
pub(super) fn group_open_gave_up_over_budget(
    group: &str,
    budget: u64,
) -> RuntimeEffectControllerError {
    poisoned_effect_error(group, PoisonReason::OverBudget { budget })
}

/// Measure a journal entry against the journal payload budget.
///
/// Any entry body is accepted by reference so a candidate can be measured before
/// it is committed to a [`JournaledEffectRecord`] variant. A recorded-effect
/// candidate is measured [`stamped`], which is exactly the bytes the wrapped
/// variant writes.
fn record_exceeds_budget<T: Serialize + ?Sized>(
    payload_budget: Option<u64>,
    recorded: &T,
) -> Result<(), (bool, serde_json::Error)> {
    let mut meter = JournalPayloadMeter {
        written: 0,
        budget: payload_budget,
        exceeded: false,
    };
    match serde_json::to_writer(&mut meter, recorded) {
        Ok(()) => Ok(()),
        Err(error) => Err((meter.exceeded, error)),
    }
}

/// Give up before running an effect whose envelope alone cannot be journaled.
///
/// Returns the budget the envelope blew, which the caller journals as the
/// fixed-size [`JournaledEffectRecord::GaveUp`] entry before the effect runs;
/// `None` means a poison substitute is available should the real outcome turn
/// out to be unjournalable.
pub(super) fn unjournalable_envelope_give_up(
    effect: &str,
    payload_budget: Option<u64>,
    envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
) -> Option<u64> {
    let budget = payload_budget?;
    // Measure the exact entry the substitution would propose, in its longest
    // rendering, so a `None` verdict here is a proof rather than an estimate.
    let substitute = poisoned_effect_record(
        effect,
        Arc::clone(envelope),
        PoisonReason::OverBudget { budget },
    );
    if record_exceeds_budget(payload_budget, &stamped(&substitute)).is_ok() {
        return None;
    }
    tracing::error!(
        %effect,
        %budget,
        "journaled effect envelope exceeds the durable journal budget; giving up with a fixed-size poison journal entry"
    );
    Some(budget)
}

/// Reconstruct the recorded effect a journal entry stands for.
///
/// A [`JournaledEffectRecord::GaveUp`] entry deliberately omits the envelope
/// replay validation matches on, so it is restored from the envelope the caller
/// reconstructed for this attempt - the same canonical value every attempt
/// derives from the invocation - while the give-up verdict itself comes from the
/// journal. That keeps the observed failure identical across attempts whose
/// configured budgets differ.
///
/// An entry another effect-journal generation wrote is refused here, typed,
/// before the replay acts on its outcome.
pub(super) fn recorded_effect_from_journal(
    envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
    effect: &str,
    entry: JournaledEffectRecord,
) -> Result<RecordedRuntimeEffect, RuntimeEffectControllerError> {
    match entry {
        JournaledEffectRecord::Recorded(recorded) => Ok(recorded),
        JournaledEffectRecord::GaveUp(GaveUpEntry {
            journaled_effect_gave_up_over_budget: budget,
        }) => Ok(poisoned_effect_record(
            effect,
            Arc::clone(envelope),
            PoisonReason::OverBudget { budget },
        )),
        JournaledEffectRecord::Retired(retired) => {
            Err(retired_generation_refusal(effect, envelope, &retired))
        }
    }
}

/// The fixed-size poison entry a pre-flight give-up puts in the journal slot.
pub(super) fn gave_up_over_budget_entry(budget: u64) -> JournaledEffectRecord {
    JournaledEffectRecord::GaveUp(GaveUpEntry {
        journaled_effect_gave_up_over_budget: budget,
    })
}

/// Give up on an effect outcome the durable journal can never accept.
///
/// The Restate SDK serializes a recorded effect while the journal command is
/// being proposed, so an outcome that cannot be journaled fails the whole
/// attempt with no journal progress - and, because that verdict is a pure
/// function of the recorded value, it fails the same way on every redrive, so
/// the turn never terminates. Substituting a typed poison outcome keeps the
/// give-up inside the effect the host is already waiting on: the substitution
/// is replay-deterministic, its envelope was proven journalable before the
/// effect ran, and the host observes
/// [`RuntimeErrorCode::RestateJournaledEffectPoisoned`] as a terminal effect
/// failure instead of an uncommitted turn.
pub(super) fn journalable_recorded_effect(
    effect: &str,
    payload_budget: Option<u64>,
    recorded: RecordedRuntimeEffect,
) -> JournaledEffectRecord {
    let Err((exceeded, error)) = record_exceeds_budget(payload_budget, &stamped(&recorded)) else {
        return JournaledEffectRecord::Recorded(recorded);
    };
    let reason = if exceeded {
        PoisonReason::OverBudget {
            budget: payload_budget.unwrap_or_default(),
        }
    } else {
        PoisonReason::Unserializable
    };
    tracing::error!(
        %effect,
        %reason,
        %error,
        "journaled effect outcome cannot be recorded; giving up with a terminal poison outcome"
    );
    JournaledEffectRecord::Recorded(poisoned_effect_record(effect, recorded.envelope, reason))
}
