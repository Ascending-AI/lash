//! The durable journal payload budget for recorded effects.
//!
//! One responsibility: decide whether a recorded effect can reach the durable
//! journal at all, and render the typed give-up when it cannot.

use std::sync::Arc;

use lash_core::{
    RuntimeEffectControllerError, RuntimeErrorCode, facade_support::CanonicalRuntimeEffectEnvelope,
};

use std::fmt;

use super::RecordedRuntimeEffect;

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

/// Measure a record against the journal payload budget.
fn record_exceeds_budget(
    payload_budget: Option<u64>,
    recorded: &RecordedRuntimeEffect,
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
/// Returns the un-journaled poison record when not even the substitute fits;
/// `None` means a poison substitute is available should the real outcome turn
/// out to be unjournalable.
pub(super) fn unjournalable_envelope_give_up(
    effect: &str,
    payload_budget: Option<u64>,
    envelope: &Arc<CanonicalRuntimeEffectEnvelope>,
) -> Option<RecordedRuntimeEffect> {
    let budget = payload_budget?;
    // Measure the exact record the substitution would propose, in its longest
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
        "journaled effect envelope exceeds the durable journal budget; giving up without journaling"
    );
    Some(substitute)
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
) -> RecordedRuntimeEffect {
    let Err((exceeded, error)) = record_exceeds_budget(payload_budget, &recorded) else {
        return recorded;
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
    poisoned_effect_record(effect, recorded.envelope, reason)
}
