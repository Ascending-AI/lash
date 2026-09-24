//! The effect journal's generation gate.
//!
//! One responsibility: stamp every entry a recorded effect puts in its
//! `lash:{replay_key}` journal slot with the effect-journal generation that
//! wrote it, and refuse, typed, an entry that another generation wrote before
//! the replay acts on it.
//!
//! The entry's bytes are the closure of [`RecordedRuntimeEffect`]: the
//! canonical runtime-effect envelope, the effect's outcome and the error it may
//! carry, plus the fixed-size give-up entry. A build that changes any of those
//! bytes, or the position of a recorded effect in the journal, bumps
//! [`EFFECT_JOURNAL_VERSION`]. A journal written under another generation is
//! then refused before its first recorded effect replays; it is never migrated
//! (ADR 0105 §12).

use std::sync::Arc;

use lash_core::{
    RuntimeEffectControllerError, RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport,
    RuntimeErrorCode, facade_support::CanonicalRuntimeEffectEnvelope,
};
use serde::{Deserialize, Serialize};

/// The effect-journal generation this build writes and replays.
///
/// 1: the first stamped generation (FIG-3672). Every entry written before the
/// stamp existed is refused.
pub const EFFECT_JOURNAL_VERSION: u32 = 1;

/// The entry field the generation is stamped under.
const EFFECT_JOURNAL_VERSION_FIELD: &str = "effect_journal_version";

/// A recorded effect: the envelope replay validation matches on, and the
/// outcome the effect produced.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RecordedRuntimeEffect {
    pub(crate) envelope: Arc<CanonicalRuntimeEffectEnvelope>,
    pub(crate) outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

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
/// Both written variants are stamped with [`EFFECT_JOURNAL_VERSION`] beside
/// their own fields. The two stay mutually exclusive because [`GaveUpEntry`]
/// denies unknown fields and carries a field name no recorded effect has, while
/// a recorded effect requires `envelope` and `outcome`.
///
/// [`Self::Retired`] is never written: it is what an entry stamped with another
/// generation, or with none, decodes to. Decoding it succeeds on purpose. The
/// SDK turns a journal entry that fails to decode into a retryable attempt
/// failure, which Restate retries without end; decoding the entry and refusing
/// it in the controller instead gives the typed refusal a turn parks on.
#[derive(Clone, Debug)]
pub(crate) enum JournaledEffectRecord {
    Recorded(RecordedRuntimeEffect),
    GaveUp(GaveUpEntry),
    Retired(RetiredEntry),
}

/// The fixed-size poison entry: one budget, no envelope, no error text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GaveUpEntry {
    pub(super) journaled_effect_gave_up_over_budget: u64,
}

/// An entry another effect-journal generation wrote, kept as its exact
/// journaled value.
#[derive(Clone, Debug)]
pub(crate) struct RetiredEntry {
    entry: serde_json::Value,
}

impl RetiredEntry {
    /// The generation the entry is stamped with, if it carries a stamp at all.
    fn found(&self) -> Option<&serde_json::Value> {
        self.entry.get(EFFECT_JOURNAL_VERSION_FIELD)
    }
}

/// An entry body with the generation stamped beside its fields.
#[derive(Serialize)]
pub(super) struct Stamped<'a, T: Serialize> {
    effect_journal_version: u32,
    #[serde(flatten)]
    entry: &'a T,
}

/// The exact bytes `entry` occupies in its journal slot.
pub(super) fn stamped<T: Serialize>(entry: &T) -> Stamped<'_, T> {
    Stamped {
        effect_journal_version: EFFECT_JOURNAL_VERSION,
        entry,
    }
}

impl Serialize for JournaledEffectRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Recorded(recorded) => stamped(recorded).serialize(serializer),
            Self::GaveUp(gave_up) => stamped(gave_up).serialize(serializer),
            Self::Retired(retired) => retired.entry.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JournaledEffectRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut entry = serde_json::Value::deserialize(deserializer)?;
        let current = entry
            .get(EFFECT_JOURNAL_VERSION_FIELD)
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(EFFECT_JOURNAL_VERSION));
        // The generation is read from the raw entry before the body decodes,
        // so an entry whose shape has since moved is refused by generation,
        // never by an accident of decoding.
        if !current {
            return Ok(Self::Retired(RetiredEntry { entry }));
        }
        if let Some(object) = entry.as_object_mut() {
            object.remove(EFFECT_JOURNAL_VERSION_FIELD);
        }
        if entry.get("envelope").is_some() && entry.get("outcome").is_some() {
            return serde_json::from_value(entry)
                .map(Self::Recorded)
                .map_err(serde::de::Error::custom);
        }
        serde_json::from_value(entry)
            .map(Self::GaveUp)
            .map_err(serde::de::Error::custom)
    }
}

/// The typed refusal of an entry another effect-journal generation wrote.
///
/// It is the engine-neutral replay divergence, so the turn parks rather than
/// fails: only a build of the generation that wrote the journal replays it, and
/// the invocation keeps its journal for one. The replay acts on nothing first.
pub(super) fn retired_generation_refusal(
    effect: &str,
    reconstructed: &CanonicalRuntimeEffectEnvelope,
    retired: &RetiredEntry,
) -> RuntimeEffectControllerError {
    let found = retired.found().map_or_else(
        || "no effect-journal generation (it predates the stamp)".to_string(),
        |found| format!("effect-journal generation {found}"),
    );
    let mut error = RuntimeEffectControllerError::new(
        RuntimeErrorCode::EffectReplayDivergence,
        format!(
            "journaled effect `{effect}` carries {found}; this build journals effect-journal \
             generation {EFFECT_JOURNAL_VERSION} and refuses it before any effect. Only a build \
             of the generation that wrote the journal replays it."
        ),
    );
    error.summary = Some(Box::new(RuntimeEffectReplayMismatchReport {
        divergent_path_count: 1,
        first_divergent_paths: vec![EFFECT_JOURNAL_VERSION_FIELD.to_string()],
        effect_kind: serde_json::from_str::<serde_json::Value>(reconstructed.json())
            .ok()
            .and_then(|envelope| {
                envelope
                    .pointer("/command/type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
    }));
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gave_up_value() -> serde_json::Value {
        serde_json::to_value(JournaledEffectRecord::GaveUp(GaveUpEntry {
            journaled_effect_gave_up_over_budget: 64,
        }))
        .expect("encode a give-up entry")
    }

    #[test]
    fn every_written_entry_carries_the_current_generation() {
        let value = gave_up_value();
        assert_eq!(
            value,
            serde_json::json!({
                "effect_journal_version": EFFECT_JOURNAL_VERSION,
                "journaled_effect_gave_up_over_budget": 64,
            })
        );
        let decoded: JournaledEffectRecord =
            serde_json::from_value(value).expect("the current generation decodes");
        assert!(matches!(
            decoded,
            JournaledEffectRecord::GaveUp(GaveUpEntry {
                journaled_effect_gave_up_over_budget: 64
            })
        ));
    }

    #[test]
    fn an_unstamped_or_foreign_entry_decodes_as_retired_and_keeps_its_bytes() {
        let current = gave_up_value();
        let mut unstamped = current.clone();
        unstamped
            .as_object_mut()
            .expect("an entry is an object")
            .remove(EFFECT_JOURNAL_VERSION_FIELD);
        let mut predecessor = current.clone();
        predecessor[EFFECT_JOURNAL_VERSION_FIELD] = serde_json::json!(EFFECT_JOURNAL_VERSION - 1);
        let mut successor = current;
        successor[EFFECT_JOURNAL_VERSION_FIELD] = serde_json::json!(EFFECT_JOURNAL_VERSION + 1);
        for entry in [unstamped, predecessor, successor, serde_json::json!(41)] {
            let decoded: JournaledEffectRecord = serde_json::from_value(entry.clone())
                .expect("a retired entry decodes rather than failing the SDK's run decode");
            let JournaledEffectRecord::Retired(retired) = &decoded else {
                panic!("{entry} must decode as retired: {decoded:?}");
            };
            assert_eq!(
                serde_json::to_value(&decoded).expect("re-encode"),
                entry,
                "a retired entry round-trips its exact journaled value"
            );
            let refusal = retired_generation_refusal(
                "lash:retired",
                &lash_core::RuntimeEffectEnvelope::new(
                    lash_core::RuntimeEffectInvocation::new(
                        lash_core::EffectAddress::new(
                            lash_core::ExecutionScope::turn("session", "turn"),
                            "sleep:retired",
                        )
                        .expect("valid address"),
                        lash_core::RuntimeAttribution::for_turn("session", "turn", 0, 0),
                        "sleep:retired",
                    ),
                    lash_core::RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For { duration_ms: 1 },
                    },
                )
                .canonical_form()
                .expect("canonical envelope"),
                retired,
            );
            assert_eq!(refusal.code, RuntimeErrorCode::EffectReplayDivergence);
            assert_eq!(
                refusal.turn_failure_cause(),
                lash_core::TurnFailureCause::Parked,
                "a retired generation parks its turn"
            );
            assert_eq!(
                refusal
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.effect_kind.as_deref()),
                Some("sleep")
            );
        }
    }
}
