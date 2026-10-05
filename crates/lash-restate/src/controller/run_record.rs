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

    pub(super) fn start_journal_run_record<'run>(
        &'run self,
        name: String,
        step: lash_core::RunRecordStep<'run>,
    ) -> lash_core::tool_dispatch::RunStepHandle<'run, RunJournalEntry> {
        let build_generation = self.sentinel_stamp();
        let first = build_generation.is_some();
        let (body, key, result) = self
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
            });
        let key_name = name.clone();
        lash_core::tool_dispatch::RunStepHandle {
            body: Box::pin(body),
            result: lash_core::tool_dispatch::RunSelectable {
                key: Box::pin(async move {
                    key.map(lash_core::tool_dispatch::SelectKey::from_engine)
                        .ok_or_else(|| {
                            RuntimeEffectControllerError::new(
                                RuntimeErrorCode::EngineEffectController,
                                format!(
                                    "Run record `{key_name}` registered without its engine key"
                                ),
                            )
                        })
                }),
                value: Box::pin(async move {
                    let Json(mut entry) = result.await.map_err(|error| {
                        crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
                    })?;
                    let generation = entry
                        .as_object_mut()
                        .and_then(|object| object.remove(BUILD_GENERATION_FIELD));
                    if first && let Some(sentinel) = &self.folded_sentinel {
                        sentinel.check(generation.as_ref()).await;
                    }
                    decode_run_journal_entry(&name, entry)
                }),
            },
        }
    }

    pub(super) fn start_journal_run_prepare<'run>(
        &'run self,
        name: String,
        step: lash_core::tool_dispatch::RunStartPrepareStep<'run>,
    ) -> lash_core::tool_dispatch::RunStepHandle<'run, lash_core::tool_dispatch::RunStartPrepared>
    {
        let (body, result) = self
            .context
            .run_json_eager_or_retry_send::<serde_json::Value, _>(name.clone(), async move {
                serde_json::to_value(stamped(&step.await?)).map_err(|error| error.to_string())
            });
        lash_core::tool_dispatch::RunStepHandle {
            body: Box::pin(body),
            result: Box::pin(async move {
                let Json(entry) = result.await.map_err(|error| {
                    crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
                })?;
                decode_entry(&name, entry)
            }),
        }
    }

    pub(super) fn start_journal_run_attempt<'run>(
        &'run self,
        name: String,
        step: lash_core::tool_dispatch::RunAttemptStep<'run>,
    ) -> lash_core::tool_dispatch::RunAttemptHandle<'run> {
        let (body, key, result) = self
            .context
            .run_json_eager_or_retry_send::<serde_json::Value, _>(name.clone(), async move {
                let entry = step.await?;
                serde_json::to_value(stamped(&entry)).map_err(|error| error.to_string())
            });
        let key_name = name.clone();
        lash_core::tool_dispatch::RunAttemptHandle {
            body: Box::pin(body),
            result: lash_core::tool_dispatch::RunSelectable {
                key: Box::pin(async move {
                    key.ok_or_else(|| {
                        RuntimeEffectControllerError::new(
                            RuntimeErrorCode::EngineEffectController,
                            format!("Run attempt `{key_name}` registered without its engine key"),
                        )
                    })
                    .map(lash_core::tool_dispatch::SelectKey::from_engine)
                }),
                value: Box::pin(async move {
                    let Json(entry) = result.await.map_err(|error| {
                        crate::wire::lash_terminal(&error, RuntimeErrorCode::EngineEffectController)
                    })?;
                    decode_entry(&name, entry)
                }),
            },
        }
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

    /// L21: X acknowledgement precedes schedule registration. A predecessor
    /// keeps its drain lane and is refused before its schedule is decoded.
    #[tokio::test]
    async fn l21_x_acknowledgement_precedes_schedule_and_retains_predecessor_lane() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'x', b's', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        let old = generation(crate::JOURNAL_LOGIC_EPOCH - 1);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            "record": "an undecodable predecessor schedule",
        });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashProcessWorkflow/run", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("lash:run:schedule:2", entry.clone())
            })
            .await
            .expect_err("refuse the predecessor before schedule decoding");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor =
            crate::sentinel::FoldedSentinel::new("LashProcessWorkflow/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        for service in [
            crate::LashService::ProcessWorkflow,
            crate::LashService::TurnDriver,
        ] {
            let lane = crate::services::DEFAULT_NAMESPACE
                .generation(service, old.clone())
                .generation_lane_name()
                .unwrap();
            assert!(
                crate::services::lash_service_routes(&crate::services::DEFAULT_NAMESPACE, &old)
                    .iter()
                    .any(|route| route.generation_lane_name().as_ref() == Some(&lane))
            );
        }
    }

    /// L21: terminal unsubscription and physical Run admission change the
    /// handler journal. The predecessor keeps its lane and its undecoded data.
    #[tokio::test]
    async fn l21_source_seal_and_physical_admission_refuse_predecessor_before_decode() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b's', b'p', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        #[cfg(not(feature = "synthetic-next"))]
        const PREDECESSOR: u32 = 30;
        #[cfg(feature = "synthetic-next")]
        const PREDECESSOR: u32 = 31;
        let old = generation(PREDECESSOR);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        assert_ne!(
            old, new,
            "changed commands require a new journal generation"
        );
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            "record": "undecodable predecessor admission",
        });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("physical-admission", entry.clone())
            })
            .await
            .expect_err("refuse the predecessor before decoding");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert!(
            crate::services::lash_service_routes(&crate::services::DEFAULT_NAMESPACE, &old)
                .iter()
                .any(|route| route.generation_lane_name().is_some_and(|name| {
                    crate::services::generation_lane_of(&name) == Some(old.clone())
                }))
        );
    }

    /// L21: the added operation completion peek belongs to a new journal
    /// generation; the predecessor keeps its operation invocation drain lane.
    #[tokio::test]
    async fn l21_operation_completion_refuses_predecessor_before_decode() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'o', b'p', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        #[cfg(not(feature = "synthetic-next"))]
        const PREDECESSOR: u32 = 25;
        #[cfg(feature = "synthetic-next")]
        const PREDECESSOR: u32 = 26;
        let old = generation(PREDECESSOR);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        assert_ne!(old, new, "operation command changes move the generation");
        let entry = serde_json::json!({BUILD_GENERATION_FIELD: old, "record": "undecodable predecessor operation result"});
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/operation", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("plugin-task-completion-cancel-peek", entry.clone())
            })
            .await
            .expect_err("refuse before decoding the predecessor");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/operation", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert!(
            crate::services::lash_service_routes(&crate::services::DEFAULT_NAMESPACE, &old)
                .iter()
                .any(|route| route.generation_lane_name().is_some_and(|name| {
                    crate::services::generation_lane_of(&name) == Some(old.clone())
                }))
        );
    }

    /// L21: native operation tool records belong to a new journal
    /// generation; the predecessor keeps its operation invocation drain lane.
    #[tokio::test]
    async fn l21_native_operation_tools_refuse_predecessor_before_decode() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'o', b'p', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        #[cfg(not(feature = "synthetic-next"))]
        const PREDECESSOR: u32 = 31;
        #[cfg(feature = "synthetic-next")]
        const PREDECESSOR: u32 = 32;
        let old = generation(PREDECESSOR);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        assert_ne!(old, new, "operation command changes move the generation");
        let entry = serde_json::json!({BUILD_GENERATION_FIELD: old, "record": "undecodable predecessor operation result"});
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/operation", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("lash:run:operation-tool:admit", entry.clone())
            })
            .await
            .expect_err("refuse before decoding the predecessor");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/operation", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert!(
            crate::services::lash_service_routes(&crate::services::DEFAULT_NAMESPACE, &old)
                .iter()
                .any(|route| route.generation_lane_name().is_some_and(|name| {
                    crate::services::generation_lane_of(&name) == Some(old.clone())
                }))
        );
    }

    /// L21: immediate proposal changes the journal schedule. The predecessor
    /// keeps its drain lane and the successor refuses before decoding X.
    #[tokio::test]
    async fn l21_borrowed_run_proposal_refuses_predecessor_before_decode() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'x', b'p', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        const PREDECESSOR: u32 = crate::JOURNAL_LOGIC_EPOCH - 1;
        let old = generation(PREDECESSOR);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        assert_ne!(old, new, "proposal scheduling moves the journal generation");
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            "record": "predecessor proposal schedule",
        });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("predecessor", entry.clone())
            })
            .await
            .expect_err("refuse before decoding the predecessor X");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert!(
            crate::services::lash_service_routes(&crate::services::DEFAULT_NAMESPACE, &old)
                .iter()
                .any(|route| route.generation_lane_name().is_some_and(|lane| {
                    crate::services::generation_lane_of(&lane) == Some(old.clone())
                }))
        );
    }

    /// L21: source-backed event observation changes the journal schedule. The predecessor
    /// keeps its drain lane and the successor refuses before decoding X.
    #[tokio::test]
    async fn l21_source_event_completion_refuses_predecessor_before_decode() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'e', b's', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        const PREDECESSOR: u32 = crate::JOURNAL_LOGIC_EPOCH - 1;
        let old = generation(PREDECESSOR);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        assert_ne!(old, new, "source observation moves the journal generation");
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            "record": "predecessor event observation",
        });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("predecessor", entry.clone())
            })
            .await
            .expect_err("refuse before decoding the predecessor X");
        assert!(format!("{refusal:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert!(
            crate::services::lash_service_routes(&crate::services::DEFAULT_NAMESPACE, &old)
                .iter()
                .any(|route| route.generation_lane_name().is_some_and(|lane| {
                    crate::services::generation_lane_of(&lane) == Some(old.clone())
                }))
        );
    }

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
    // FIG-4948 records detailed tool projections in A/D/V and FIG-4936 records
    // cancellation in native inline X before D can publish state. A predecessor
    // generation keeps its drain lane and is refused before decoding.
    #[tokio::test]
    async fn l21_a_predecessor_run_journal_parks_before_decode_and_keeps_its_lane() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b'r', b'u', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        const PREDECESSOR_EPOCH: u32 = crate::JOURNAL_LOGIC_EPOCH - 1;
        let old = generation(PREDECESSOR_EPOCH);
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
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            EFFECT_JOURNAL_VERSION_FIELD: EFFECT_JOURNAL_VERSION,
            "record": lash_core::tool_run::RunRecord {
                segment: lash_core::tool_run::SegmentOrdinal(0),
                first: lash_core::tool_run::RunEventOrdinal(0),
                events: vec![lash_core::tool_run::RunEvent::Decided {
                    call_id: lash_core::ToolCallId::fixture("predecessor-check-cancel"),
                    rank: 1,
                    decision: lash_core::tool_run::CallDecision::Cancelled,
                    after: Some(lash_core::tool_run::CheckRecord::reduce(vec![
                        lash_core::tool_run::AttributedVerdict {
                            callback: lash_core::store::plugin_writers::PluginCallbackIdentity {
                                owner: lash_core::plugin::PluginRevision::new(
                                    "guard", lash_core::plugin::BehaviorRevision::ONE,
                                ),
                                key: "tool_result_check:cancel".into(),
                            },
                            verdict: lash_core::tool_run::AfterCheckVerdict::Cancel {
                                cause: lash_core::tool_run::HookCause {
                                    error_type: "check-cancel".into(),
                                    error_version: std::num::NonZeroU32::MIN,
                                    payload: serde_json::json!({"only_this_call": true}),
                                },
                            },
                        },
                    ])),
                }],
                trace: None,
            },
        });
        let mut body = entry.clone();
        body.as_object_mut().unwrap().remove(BUILD_GENERATION_FIELD);
        decode_run_journal_entry("predecessor", body.clone())
            .expect("the predecessor decision still decodes as retained data");
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new);
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refusal = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                decode_run_journal_entry("predecessor", body)
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
    #[tokio::test]
    async fn l21_turn_continuation_generation_refuses_predecessor_before_decode() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b't', b'u', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        let old = generation(crate::JOURNAL_LOGIC_EPOCH - 1);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        let predecessor_lane = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::TurnDriver, old.clone())
            .generation_lane_name()
            .unwrap();
        let entry = serde_json::json!({BUILD_GENERATION_FIELD: old,
            "continuation": "predecessor turn opener encoding"});
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new.clone());
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refused = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                serde_json::from_value::<lash_core::store::RunContinuation>(
                    entry["continuation"].clone(),
                )
            })
            .await
            .expect_err("generation refusal precedes continuation decoding");
        assert!(format!("{refused:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert_eq!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, old)
                .generation_lane_name()
                .unwrap(),
            predecessor_lane
        );
        assert_ne!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, new)
                .generation_lane_name()
                .unwrap(),
            predecessor_lane
        );
    }

    /// Standard rounds replace the child-completion cursor with a Run
    /// aggregate cursor. Refuse the predecessor before decoding that state;
    /// its own generation continues to name the lane that drains it.
    #[tokio::test]
    async fn l21_standard_round_refuses_a_child_cursor_before_decode_and_keeps_its_lane() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b's', b't', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        const PREDECESSOR_EPOCH: u32 = crate::JOURNAL_LOGIC_EPOCH - 1;
        let old = generation(PREDECESSOR_EPOCH);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        let predecessor_lane = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::TurnDriver, old.clone())
            .generation_lane_name()
            .unwrap();
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            "cursor": {"results": [null], "pending": []},
        });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/run", new.clone());
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refused = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                serde_json::from_value::<lash_core::session::ToolRunAggregateCursor>(
                    entry["cursor"].clone(),
                )
            })
            .await
            .expect_err("generation refusal precedes the child cursor decoder");
        assert!(format!("{refused:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/run", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert_eq!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, old)
                .generation_lane_name()
                .unwrap(),
            predecessor_lane
        );
        assert_ne!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, new)
                .generation_lane_name()
                .unwrap(),
            predecessor_lane
        );
    }

    /// A cell seal uses its stable address rather than live turn metadata.
    /// Fence the predecessor envelope before its payload decodes; the old
    /// deployment retains the generation lane that drains its journal.
    #[tokio::test]
    async fn l21_stable_cell_seal_refuses_predecessor_before_decode_and_keeps_its_lane() {
        use lash_core::engine::BuildGeneration;
        let generation = |epoch: u32| {
            let bytes = epoch.to_be_bytes();
            BuildGeneration::from_digest([b's', b'e', bytes[0], bytes[1], bytes[2], bytes[3]])
        };
        const PREDECESSOR_EPOCH: u32 = crate::JOURNAL_LOGIC_EPOCH - 1;
        const { assert!(crate::JOURNAL_LOGIC_EPOCH > PREDECESSOR_EPOCH) };
        let old = generation(PREDECESSOR_EPOCH);
        let new = generation(crate::JOURNAL_LOGIC_EPOCH);
        let predecessor_lane = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::TurnDriver, old.clone())
            .generation_lane_name()
            .unwrap();
        let entry = serde_json::json!({
            BUILD_GENERATION_FIELD: old,
            "seal": "an undecodable predecessor seal outcome",
        });
        let sentinel = crate::sentinel::FoldedSentinel::new("LashTurn/seal", new.clone());
        let decoded = std::sync::atomic::AtomicBool::new(false);
        let refused = sentinel
            .guard(async {
                sentinel.check(entry.get(BUILD_GENERATION_FIELD)).await;
                decoded.store(true, std::sync::atomic::Ordering::SeqCst);
                serde_json::from_value::<lash_core::RuntimeEffectOutcome>(entry["seal"].clone())
            })
            .await
            .expect_err("generation refusal precedes the seal decoder");
        assert!(format!("{refused:?}").contains("RetiredGeneration"));
        assert!(!decoded.load(std::sync::atomic::Ordering::SeqCst));
        let predecessor = crate::sentinel::FoldedSentinel::new("LashTurn/seal", old.clone());
        predecessor.check(entry.get(BUILD_GENERATION_FIELD)).await;
        assert_eq!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, old)
                .generation_lane_name()
                .unwrap(),
            predecessor_lane
        );
        assert_ne!(
            crate::services::DEFAULT_NAMESPACE
                .generation(crate::LashService::TurnDriver, new)
                .generation_lane_name()
                .unwrap(),
            predecessor_lane
        );
    }
}
