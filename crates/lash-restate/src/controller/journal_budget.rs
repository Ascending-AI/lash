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

use super::RecordedRuntimeEffect;

/// What a journaled effect's `ctx.run` entry carries.
///
/// One journal shape for both give-up paths and the happy path, so the slot a
/// recorded effect occupies never depends on the payload budget in force at the
/// time. [`Self::GaveUp`] is the fixed-size poison entry: it carries the verdict
/// and the budget that produced it, never the envelope, so it fits any journal
/// even when the envelope-carrying record does not. Replaying it reproduces the
/// original give-up under whatever budget the replaying attempt was configured
/// with.
///
/// The encoding is untagged on purpose: a recorded effect keeps the exact
/// payload bytes it had before this entry type existed, so a journal written by
/// an older deployment still replays - a wrapper tag would fail to deserialize on
/// every in-flight journal, which is the redrive-panic loop this whole seam
/// exists to prevent. The two variants stay mutually exclusive because
/// [`GaveUpEntry`] denies unknown fields and carries a field name no recorded
/// effect has, while a recorded effect requires `envelope` and `outcome`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum JournaledEffectRecord {
    Recorded(RecordedRuntimeEffect),
    GaveUp(GaveUpEntry),
}

/// The fixed-size poison entry: one budget, no envelope, no error text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GaveUpEntry {
    journaled_effect_gave_up_over_budget: u64,
}

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

/// Decide, from the envelope and the budget in force right now, whether an
/// eagerly-executed effect may run. Journal the result before acting on it.
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

fn poisoned_effect_record(
    effect: &str,
    envelope: Arc<CanonicalRuntimeEffectEnvelope>,
    reason: PoisonReason,
) -> RecordedRuntimeEffect {
    RecordedRuntimeEffect {
        envelope,
        outcome: Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateJournaledEffectPoisoned,
            format!("journaled effect `{effect}` gave up because {reason}"),
        )),
    }
}

/// Measure a journal entry against the journal payload budget.
///
/// Any entry body is accepted by reference so a candidate can be measured before
/// it is committed to a [`JournaledEffectRecord`] variant. [`JournaledEffectRecord`]
/// is untagged, so measuring a variant's body yields exactly the bytes the
/// wrapped variant would write.
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
    if record_exceeds_budget(payload_budget, &substitute).is_ok() {
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
pub(super) fn recorded_effect_from_journal(
    envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
    effect: &str,
    entry: JournaledEffectRecord,
) -> RecordedRuntimeEffect {
    match entry {
        JournaledEffectRecord::Recorded(recorded) => recorded,
        JournaledEffectRecord::GaveUp(GaveUpEntry {
            journaled_effect_gave_up_over_budget: budget,
        }) => poisoned_effect_record(
            effect,
            Arc::clone(envelope),
            PoisonReason::OverBudget { budget },
        ),
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
    let Err((exceeded, error)) = record_exceeds_budget(payload_budget, &recorded) else {
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
