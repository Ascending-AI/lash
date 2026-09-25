//! The effect journal's generation gate.
//!
//! One responsibility: stamp every entry a recorded effect puts in its
//! `lash:{replay_key}` journal slot with the effect-journal generation that
//! wrote it, and refuse, typed, an entry that another generation wrote before
//! the replay acts on it.
//!
//! The entry's bytes are the closure of [`RecordedRuntimeEffect`]: the
//! canonical runtime-effect envelope, the effect's outcome and the error it may
//! carry, plus the fixed-size give-up entry — and the frontier marker a process
//! start or a timer journals before it acts. A build that changes any of those
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
/// 2: a turn's decision state rides its recorded outcomes (FIG-3672 P7). The
/// model call names its provider, its outcome carries what the provider stream
/// left for later steps (the published reasoning and each plugin's stream-hook
/// end state), and the assistant-response step's command carries those
/// stream-hook states.
/// 3: every execution-environment sync records the tool surface it built,
/// which the drive installs as the turn's catalog (FIG-3672 P7b); a turn
/// machine always opens with its protocol-start sync.
/// 4: a tool child reads its recorded execution environment through its own
/// recorded `load_execution_env` step, the first effect it journals, so a
/// replay never reads the store again (FIG-3683).
/// 5: a live store or session fault inside the environment sync or the
/// assistant-response hooks ends the attempt instead of journaling (FIG-3726);
/// a journal written under an earlier generation may hold such a fault as the
/// step's recorded outcome, which replaying would surface forever, so those
/// journals are refused rather than replayed.
/// 6: a turn's admission records the head it was admitted on and its turn
/// index in its journaled drive outcome, and the acceptance names no turn index
/// read from the live head, so a replay after the turn's own commit rebuilds
/// the turn from its recorded base (FIG-3682).
/// 7: cancellation is an engine event (FIG-3672 P9). A turn-observing
/// effect-group rank wait races the turn's durable cancellation gate (the
/// gate's awakeable and registration now precede its wake); a code cell
/// journals a gate peek at each instruction checkpoint it reaches, and after
/// a cell that stopped on the host; and the post-abort peek follows only an
/// abort a recorded outcome typed as the turn's cancellation.
/// 8: a process await that lost to the turn's cancellation gate records the
/// cancel it owes the process as a step before it asks the process workflow
/// to cancel, so a replay after the process ended issues the same cancel call
/// instead of re-asking the store (FIG-3752).
/// 9: every process start and every timer journals a frontier marker at
/// `lash:{replay_key}:frontier` before it acts, recording the start's process
/// id, so a drifted binding's recorded start or sleep is served and only one
/// at the live frontier refuses (FIG-3779).
/// 10: a direct turn's journaled admission (`ClaimAcceptedTurnInput`) records
/// the executable generation it runs under, and an execution-environment sync
/// no longer journals a cell replay-key grammar (FIG-3571): the turn's one
/// generation is checked at its admission.
pub const EFFECT_JOURNAL_VERSION: u32 = 10;

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
    generation_refusal(
        effect,
        retired.found(),
        serde_json::from_str::<serde_json::Value>(reconstructed.json())
            .ok()
            .and_then(|envelope| {
                envelope
                    .pointer("/command/type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
    )
}

/// The typed refusal of a journal entry stamped with `found` — another
/// effect-journal generation, or none — for an effect of `effect_kind`.
fn generation_refusal(
    effect: &str,
    found: Option<&serde_json::Value>,
    effect_kind: Option<String>,
) -> RuntimeEffectControllerError {
    let found = found.map_or_else(
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
        effect_kind,
    }));
    error
}

/// What a frontier marker records (FIG-3779): the effect whose live frontier
/// its `lash:{replay_key}:frontier` step marks, ahead of an effect that acts
/// outside a `ctx.run` closure.
///
/// A process start records its idempotency key — the process id its
/// registration is idempotent under and its workflow send is keyed by — so a
/// served marker names the row an earlier attempt may have registered. A
/// timer records only that it is a sleep.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frontier", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FrontierMark {
    ProcessStart { process_id: lash_core::ProcessId },
    Sleep,
}

impl FrontierMark {
    fn effect_kind(&self) -> &'static str {
        match self {
            Self::ProcessStart { .. } => "process",
            Self::Sleep => "sleep",
        }
    }
}

/// The exact value a frontier marker journals: `mark`, stamped with this
/// build's effect-journal generation.
pub(super) fn frontier_entry(
    mark: &FrontierMark,
) -> Result<serde_json::Value, RuntimeEffectControllerError> {
    serde_json::to_value(stamped(mark)).map_err(|error| {
        RuntimeEffectControllerError::new(
            RuntimeErrorCode::RecordEncodingFailed,
            format!("failed to encode the frontier mark {mark:?}: {error}"),
        )
    })
}

/// The mark a frontier marker's journaled `entry` records, checked against
/// the `expected` one this attempt would have journaled.
///
/// An entry another generation stamped is refused by generation, before its
/// body decodes, as a recorded effect's is. An entry that records another
/// effect — a start of another process at this key, or another kind — is the
/// replay's divergence, and parks the same way.
pub(super) fn recorded_frontier_mark(
    effect: &str,
    mut entry: serde_json::Value,
    expected: &FrontierMark,
) -> Result<(), RuntimeEffectControllerError> {
    if entry
        .get(EFFECT_JOURNAL_VERSION_FIELD)
        .and_then(serde_json::Value::as_u64)
        != Some(u64::from(EFFECT_JOURNAL_VERSION))
    {
        return Err(generation_refusal(
            effect,
            entry.get(EFFECT_JOURNAL_VERSION_FIELD),
            Some(expected.effect_kind().to_string()),
        ));
    }
    if let Some(object) = entry.as_object_mut() {
        object.remove(EFFECT_JOURNAL_VERSION_FIELD);
    }
    match serde_json::from_value::<FrontierMark>(entry) {
        Ok(recorded) if &recorded == expected => Ok(()),
        recorded => Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EffectReplayDivergence,
            format!(
                "frontier marker `{effect}` records {recorded:?}, where this replay marks \
                 {expected:?}"
            ),
        )),
    }
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
