//! Journaling one record of a logical Run's event log (K3, FIG-4877).
//!
//! One responsibility: put a Run record and the canonical material it owns in
//! one `ctx.run` entry, stamped with the effect-journal generation, and answer
//! the journaled entry. A replay serves the entry without running its step,
//! and an entry another generation wrote is refused, typed, before anything
//! reads it.

use lash_core::tool_run::RunJournalEntry;
use lash_core::{RuntimeEffectControllerError, RuntimeErrorCode};
use restate_sdk::serde::Json;

use super::RestateRuntimeEffectController;
use super::context::RestateControllerContext;
use super::effect_journal::{EFFECT_JOURNAL_VERSION, generation_refusal, stamped};

/// The entry field the build generation is folded under on a handler's first
/// recorded entry (FIG-3980).
const BUILD_GENERATION_FIELD: &str = "build_generation";
const EFFECT_JOURNAL_VERSION_FIELD: &str = "effect_journal_version";

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// Run `step` in the journal slot named `name`, or serve the entry an
    /// earlier attempt journaled there. A step fault is never journaled: the
    /// attempt ends retryably and the replay runs the step again.
    pub(super) async fn journal_run_record<'run>(
        &'run self,
        name: String,
        step: lash_core::RunRecordStep<'run>,
    ) -> Result<RunJournalEntry, RuntimeEffectControllerError>
    where
        'ctx: 'run,
    {
        let build_generation = self.sentinel_stamp();
        let first = build_generation.is_some();
        let Json(mut entry) = self
            .context
            .run_json_or_retry_send::<serde_json::Value, _>(name.clone(), async move {
                let record = step.await?;
                let mut entry =
                    serde_json::to_value(stamped(&record)).map_err(|error| error.to_string())?;
                if let (Some(generation), Some(object)) = (build_generation, entry.as_object_mut())
                {
                    object.insert(BUILD_GENERATION_FIELD.to_owned(), generation);
                }
                Ok(entry)
            })
            .await
            .map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
            })?;
        let generation = entry
            .as_object_mut()
            .and_then(|object| object.remove(BUILD_GENERATION_FIELD));
        if first && let Some(sentinel) = &self.folded_sentinel {
            sentinel.check(generation.as_ref()).await;
        }
        decode_run_journal_entry(&name, entry)
    }
    pub(super) async fn journal_run_schedule(
        &self,
        name: String,
        step: lash_core::RunRecordStep<'static>,
    ) -> Result<RunJournalEntry, RuntimeEffectControllerError> {
        let build_generation = self.sentinel_stamp();
        let first = build_generation.is_some();
        let Json(mut entry) = self
            .context
            .run_json_eager_or_retry_send::<serde_json::Value, _>(name.clone(), async move {
                let record = step.await?;
                let mut entry =
                    serde_json::to_value(stamped(&record)).map_err(|error| error.to_string())?;
                if let (Some(generation), Some(object)) = (build_generation, entry.as_object_mut())
                {
                    object.insert(BUILD_GENERATION_FIELD.to_owned(), generation);
                }
                Ok(entry)
            })
            .await
            .map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
            })?;
        let generation = entry
            .as_object_mut()
            .and_then(|object| object.remove(BUILD_GENERATION_FIELD));
        if first && let Some(sentinel) = &self.folded_sentinel {
            sentinel.check(generation.as_ref()).await;
        }
        decode_run_journal_entry(&name, entry)
    }

    pub(super) fn start_journal_run_attempt(
        &self,
        name: String,
        step: lash_core::tool_dispatch::RunAttemptStep,
    ) -> lash_core::tool_dispatch::RunAttemptHandle {
        let result = self
            .context
            .run_json_eager_or_retry_send::<serde_json::Value, _>(name.clone(), async move {
                let entry = step.await?;
                serde_json::to_value(stamped(&entry)).map_err(|error| error.to_string())
            });
        Box::pin(async move {
            let Json(entry) = result.await.map_err(|error| {
                crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
            })?;
            decode_entry(&name, entry)
        })
    }
}

/// The Run record a journaled `entry` holds. The generation is read from the
/// raw entry before its body decodes, so an entry another generation wrote is
/// refused by generation, never by an accident of decoding.
fn decode_run_journal_entry(
    name: &str,
    entry: serde_json::Value,
) -> Result<RunJournalEntry, RuntimeEffectControllerError> {
    decode_entry(name, entry)
}

fn decode_entry<T: serde::de::DeserializeOwned>(
    name: &str,
    mut entry: serde_json::Value,
) -> Result<T, RuntimeEffectControllerError> {
    if entry
        .get(EFFECT_JOURNAL_VERSION_FIELD)
        .and_then(serde_json::Value::as_u64)
        != Some(u64::from(EFFECT_JOURNAL_VERSION))
    {
        return Err(generation_refusal(
            name,
            entry.get(EFFECT_JOURNAL_VERSION_FIELD),
            Some("run_record".to_owned()),
        ));
    }
    if let Some(object) = entry.as_object_mut() {
        object.remove(EFFECT_JOURNAL_VERSION_FIELD);
    }
    serde_json::from_value(entry).map_err(|error| {
        RuntimeEffectControllerError::new(
            RuntimeErrorCode::EffectReplayDivergence,
            format!("journaled Run record `{name}` does not decode: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_record_of_another_generation_parks_before_it_decodes() {
        for found in [
            serde_json::json!(EFFECT_JOURNAL_VERSION - 1),
            serde_json::Value::Null,
        ] {
            let entry = serde_json::json!({
                EFFECT_JOURNAL_VERSION_FIELD: found,
                "record": "a body this build cannot decode",
            });
            let refusal = decode_run_journal_entry("lash:run:retired", entry)
                .expect_err("another generation's record is refused");
            assert_eq!(refusal.code, RuntimeErrorCode::EffectReplayDivergence);
            assert_eq!(
                refusal.turn_failure_cause(),
                lash_core::TurnFailureCause::Parked
            );
            assert_eq!(
                refusal
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.effect_kind.as_deref()),
                Some("run_record")
            );
        }
    }
    // D06 replaces terminal waiter calls with short subscriptions and publication.
    // Its predecessor retains FIG-4920's deleted-session command transition.
    // The immediate predecessor remains addressable by its drain lane.
    #[tokio::test]
    async fn l21_a_predecessor_run_journal_parks_before_decode_and_keeps_its_lane() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'r', b'u', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        let old = generation(crate::JOURNAL_LOGIC_EPOCH - 1);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        let old_lane = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::TurnDriver, old.clone())
            .generation_lane_name()
            .unwrap();
        let new_lane = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::TurnDriver, new.clone())
            .generation_lane_name()
            .unwrap();
        assert_ne!(old_lane, new_lane);
        let entry = serde_json::json!({ BUILD_GENERATION_FIELD: old, "record": "a predecessor shape this build cannot decode" });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("predecessor", entry.clone())
            })
            .await
            .expect_err("the new handler keeps this journal for its predecessor");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert_eq!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, old.clone())
                .generation_lane_name()
                .unwrap(),
            old_lane
        );
        // Local quiescence is shared by turn and process handlers (K6).
        let predecessor_process = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::ProcessWorkflow, old)
            .generation_lane_name()
            .unwrap();
        let successor_process = crate::services::DEFAULT_NAMESPACE
            .generation(
                crate::LashService::ProcessWorkflow,
                generation(crate::JOURNAL_LOGIC_EPOCH),
            )
            .generation_lane_name()
            .unwrap();
        assert_ne!(predecessor_process, successor_process);
    }
}
