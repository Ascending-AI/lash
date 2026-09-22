use crate::SessionId;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::ToolDispatchContext;

pub async fn execute_final_tool_intents(
    context: &ToolDispatchContext<'_>,
    tool_call_id: Option<&str>,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    let execution_scope_id = context.effect_controller.scoped().scope_id().to_string();
    if intents.intents.is_empty() && intents.protocol_version == crate::TOOL_INTENT_PROTOCOL_V3 {
        return Ok(Vec::new());
    }
    if let Some(refusal) = admit_batch(&context.session_id, tool_call_id, intents) {
        return Ok(refuse_all(
            context,
            &execution_scope_id,
            tool_call_id,
            intents,
            refusal,
        ));
    }

    // ADR 0099 §4: once the group child whose emission minted this batch is
    // cancel-decided, no new semantic admission may be created beneath it. The
    // fence is not a read this executor performs — the controller lent to this
    // child carries its `GroupChildBinding`, so every intent's journaled sink
    // admits itself under the substrate's own arbitration of the child's own
    // replay row (the minting replay-row claim, the native group mutex, or the
    // serialized index handler), and a cancel that lands between intents
    // surfaces as the typed `RuntimeEffectGroupChildCancelDecided` refusal
    // from the next admission.
    // The decision can land mid-batch, so the refusal latches: once one
    // admission reports it, the remaining intents refuse without reaching
    // their sinks — `Cancelled` is terminal.
    let mut minting_cancelled = false;
    let mut outcomes = Vec::with_capacity(intents.intents.len());
    for (index, intent) in intents.intents.iter().enumerate() {
        let identity = match derive_identity(context, &execution_scope_id, tool_call_id, index) {
            Ok(identity) => identity,
            Err(refusal) => {
                outcomes.push(refused(index, intent.kind(), None, refusal));
                continue;
            }
        };
        if minting_cancelled {
            outcomes.push(refused(
                index,
                intent.kind(),
                Some(identity),
                crate::ToolIntentRefusalReason::MintingGroupChildCancelled,
            ));
            continue;
        }
        let span = tracing::info_span!(
            target: "lash::tool_intent",
            "tool_intent.execute",
            session_id = %identity.session_id,
            execution_scope_id = %identity.execution_scope_id,
            tool_call_id = %identity.tool_call_id,
            intent_index = identity.intent_index,
            intent_kind = intent.kind().as_str(),
            replay_key = %identity.replay_key,
        );
        let _entered = span.enter();
        let result = execute_one(context, intent, &identity, child_trace_hook).await;
        let outcome = match result {
            Ok(result) => {
                record_executed_metric(intent.kind());
                crate::ToolIntentExecutionOutcome::Executed {
                    identity,
                    kind: intent.kind(),
                    result,
                }
            }
            Err(crate::PluginError::RuntimeEffectController(error))
                if error.code.is_replay_mismatch() =>
            {
                return Err(error);
            }
            Err(crate::PluginError::RuntimeEffectController(error))
                if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided =>
            {
                minting_cancelled = true;
                refused(
                    index,
                    intent.kind(),
                    Some(identity),
                    crate::ToolIntentRefusalReason::MintingGroupChildCancelled,
                )
            }
            Err(error) => refused(
                index,
                intent.kind(),
                Some(identity),
                crate::ToolIntentRefusalReason::CommandFailed {
                    code: error_code(&error),
                    message: error_message(&error),
                },
            ),
        };
        tracing::info!(
            target: "lash::tool_intent",
            outcome = match &outcome {
                crate::ToolIntentExecutionOutcome::Executed { .. } => "executed",
                crate::ToolIntentExecutionOutcome::Refused { .. } => "refused",
                crate::ToolIntentExecutionOutcome::ProtocolRefused { .. } => "protocol_refused",
            },
            "tool intent outcome"
        );
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

fn admit_batch(
    session_id: &SessionId,
    tool_call_id: Option<&str>,
    intents: &crate::ToolIntents,
) -> Option<crate::ToolIntentRefusalReason> {
    if intents.protocol_version != crate::TOOL_INTENT_PROTOCOL_V3 {
        return Some(crate::ToolIntentRefusalReason::UnsupportedProtocolVersion {
            recorded: intents.protocol_version,
        });
    }
    if tool_call_id.is_none() {
        return Some(crate::ToolIntentRefusalReason::MissingToolCallId);
    }
    if intents.intents.len() > crate::TOOL_INTENT_MAX_COUNT {
        return Some(crate::ToolIntentRefusalReason::CountBudgetExceeded {
            actual: intents.intents.len(),
            maximum: crate::TOOL_INTENT_MAX_COUNT,
        });
    }
    let canonical_bytes = serde_json::to_vec(intents).map_or(usize::MAX, |bytes| bytes.len());
    if canonical_bytes > crate::TOOL_INTENT_MAX_CANONICAL_BYTES {
        return Some(
            crate::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                actual: canonical_bytes,
                maximum: crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
            },
        );
    }
    let mut by_kind = BTreeMap::<&'static str, (crate::ToolIntentKind, usize)>::new();
    for intent in &intents.intents {
        let entry = by_kind
            .entry(intent.kind().as_str())
            .or_insert((intent.kind(), 0));
        entry.1 += 1;
        if entry.1 > crate::TOOL_INTENT_MAX_PER_KIND {
            return Some(crate::ToolIntentRefusalReason::PerKindBudgetExceeded {
                kind: entry.0,
                actual: entry.1,
                maximum: crate::TOOL_INTENT_MAX_PER_KIND,
            });
        }
        if intent.session_id() != session_id {
            return Some(crate::ToolIntentRefusalReason::SessionMismatch {
                expected: session_id.to_string(),
                recorded: intent.session_id().to_string(),
            });
        }
    }
    None
}

fn refuse_all(
    context: &ToolDispatchContext<'_>,
    execution_scope_id: &str,
    tool_call_id: Option<&str>,
    intents: &crate::ToolIntents,
    refusal: crate::ToolIntentRefusalReason,
) -> Vec<crate::ToolIntentExecutionOutcome> {
    if intents.intents.is_empty() {
        let span = tracing::info_span!(
            target: "lash::tool_intent",
            "tool_intent.execute",
            session_id = %context.session_id,
            execution_scope_id,
            tool_call_id = tool_call_id.unwrap_or("<missing>"),
            intent_index = tracing::field::Empty,
            intent_kind = "<batch>",
            replay_key = "<unavailable>",
        );
        let _entered = span.enter();
        tracing::warn!(
            target: "lash::tool_intent",
            refusal_reason = refusal.code(),
            "empty tool intent batch refused"
        );
        return vec![crate::ToolIntentExecutionOutcome::ProtocolRefused { refusal }];
    }
    intents
        .intents
        .iter()
        .enumerate()
        .map(|(index, intent)| {
            let identity =
                derive_identity(context, execution_scope_id, tool_call_id, index).ok();
            let span = tracing::info_span!(
                target: "lash::tool_intent",
                "tool_intent.execute",
                session_id = %context.session_id,
                execution_scope_id,
                tool_call_id = tool_call_id.unwrap_or("<missing>"),
                intent_index = index,
                intent_kind = intent.kind().as_str(),
                replay_key = identity.as_ref().map_or("<unavailable>", |identity| identity.replay_key.as_str()),
            );
            let _entered = span.enter();
            refused(index, intent.kind(), identity, refusal.clone())
        })
        .collect()
}

fn derive_identity(
    context: &ToolDispatchContext<'_>,
    execution_scope_id: &str,
    tool_call_id: Option<&str>,
    intent_index: usize,
) -> Result<crate::ToolIntentIdentity, crate::ToolIntentRefusalReason> {
    crate::derive_tool_intent_identity_under(
        &context.session_id,
        execution_scope_id,
        tool_call_id,
        intent_index,
        context.parent_invocation.as_ref(),
    )
}

fn refused(
    index: usize,
    kind: crate::ToolIntentKind,
    identity: Option<crate::ToolIntentIdentity>,
    refusal: crate::ToolIntentRefusalReason,
) -> crate::ToolIntentExecutionOutcome {
    record_refused_metric(kind, &refusal);
    tracing::warn!(
        target: "lash::tool_intent",
        intent_kind = kind.as_str(),
        refusal_reason = refusal.code(),
        intent_index = index,
        "tool intent refused"
    );
    crate::ToolIntentExecutionOutcome::Refused {
        identity,
        intent_index: u32::try_from(index).unwrap_or(u32::MAX),
        kind,
        refusal,
    }
}

#[cfg(feature = "otel-trace")]
fn tool_intent_metrics() -> &'static lash_trace::otel::ToolIntentMetrics {
    static METRICS: std::sync::LazyLock<lash_trace::otel::ToolIntentMetrics> =
        std::sync::LazyLock::new(lash_trace::otel::ToolIntentMetrics::from_global_provider);
    &METRICS
}

#[cfg(feature = "otel-trace")]
fn record_executed_metric(kind: crate::ToolIntentKind) {
    tool_intent_metrics().record_executed(kind.as_str());
}

#[cfg(not(feature = "otel-trace"))]
fn record_executed_metric(_kind: crate::ToolIntentKind) {}

#[cfg(feature = "otel-trace")]
fn record_refused_metric(kind: crate::ToolIntentKind, refusal: &crate::ToolIntentRefusalReason) {
    tool_intent_metrics().record_refused(kind.as_str(), refusal.code());
}

#[cfg(not(feature = "otel-trace"))]
fn record_refused_metric(_kind: crate::ToolIntentKind, _refusal: &crate::ToolIntentRefusalReason) {}

#[expect(
    clippy::expect_used,
    reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
)]
async fn execute_one(
    context: &ToolDispatchContext<'_>,
    intent: &crate::ToolIntent,
    identity: &crate::ToolIntentIdentity,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<serde_json::Value, crate::PluginError> {
    let parent = context.parent_invocation.clone().unwrap_or_else(|| {
        crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(
                context.effect_controller.scoped().execution_scope().clone(),
                identity.replay_key.clone(),
            )
            .expect("tool-intent execution carries an admitted effect scope"),
            context.parentless_attribution(),
            format!("tool-intent:{}", identity.intent_index),
        )
    });
    let parent = parent.with_replay_attribution(crate::RuntimeReplayAttribution::ToolIntent(
        identity.clone(),
    ));
    let scope = crate::ProcessOpScope::new(context.effect_controller.scoped())
        .with_parent_invocation(Some(parent))
        .with_agent_frame_id(Some(context.agent_frame_id.clone()));

    match intent {
        crate::ToolIntent::StartProcess(intent) => {
            // The declaration carries no id: the sole constructor on this path
            // derives it from this declaration's identity, so a redrive of the
            // same attempt starts the same process id (FIG-2994).
            let request = intent.into_request(identity);
            let summary = context
                .processes
                .start_from_recorded_intent(&intent.session_id, request, scope)
                .await?;
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: summary.process_id.clone(),
                    incarnation: summary.incarnation,
                    attempt: None,
                    child_entry_name: None,
                });
            }
            Ok(serde_json::to_value(summary).unwrap_or(serde_json::Value::Null))
        }
        crate::ToolIntent::SignalProcess(intent) => {
            let event = context
                .processes
                .signal_recorded_intent(
                    &intent.session_id,
                    &intent.process_id,
                    intent.signal_name.clone(),
                    identity.replay_key.clone(),
                    intent.payload.clone(),
                    scope,
                )
                .await?;
            Ok(serde_json::to_value(event).unwrap_or(serde_json::Value::Null))
        }
        crate::ToolIntent::CancelProcess(intent) => {
            let record = context
                .processes
                .cancel_recorded_intent(
                    &intent.session_id,
                    &intent.process_id,
                    identity.clone(),
                    scope,
                )
                .await?;
            Ok(
                serde_json::to_value(crate::ProcessCancelReceipt::from_record(record)?)
                    .unwrap_or(serde_json::Value::Null),
            )
        }
        crate::ToolIntent::EmitProcessEvent(intent) => {
            let event = context
                .processes
                .emit_event_recorded_intent(
                    &intent.session_id,
                    &intent.process_id,
                    intent.event_type.clone(),
                    identity.replay_key.clone(),
                    intent.payload.clone(),
                    scope,
                )
                .await?;
            Ok(serde_json::to_value(event).unwrap_or(serde_json::Value::Null))
        }
        crate::ToolIntent::EmitTrigger(intent) => {
            // The router owns the whole emission, but the durable declaration
            // owns its occurrence identity. Stamp the request with that
            // declaration's replay key so two declarations cannot collapse
            // merely because their callers reused a key. A redrive retains the
            // same replay key, so it still re-ingests the same occurrence and
            // replays the same deterministic delivery starts.
            // `emit_recorded` settles the report those two dedupe points make
            // replay-varying, so the recorded `Executed` result is byte-stable.
            let router = context.trigger_router.as_ref().ok_or_else(|| {
                crate::PluginError::Session(
                    "trigger store is unavailable in this runtime".to_string(),
                )
            })?;
            // Boxed because the drain future is already near the coordinator's
            // large-future budget and emission adds a delivery-start frame.
            let mut request = intent.request.clone();
            request.idempotency_key = identity.replay_key.clone();
            let report =
                Box::pin(router.emit_recorded(request, &context.effect_controller.scoped()))
                    .await?;
            Ok(serde_json::to_value(report).unwrap_or(serde_json::Value::Null))
        }
        crate::ToolIntent::RegisterProcessDefinition(intent) => {
            let registry = context
                .process_definitions
                .clone()
                .ok_or_else(|| process_definition_registry_unavailable(&intent.engine_kind))?;
            Ok(serde_json::to_value(
                realize_register_process_definition(context, intent, identity, registry).await?,
            )
            .unwrap_or(serde_json::Value::Null))
        }
        crate::ToolIntent::RegisterTrigger(intent) => {
            let router = context.trigger_router.as_ref().ok_or_else(|| {
                crate::PluginError::Session(
                    "trigger store is unavailable in this runtime".to_string(),
                )
            })?;
            let outcome = Box::pin(register_recorded_trigger(
                context,
                router,
                identity,
                intent.draft.clone(),
            ))
            .await?;
            Ok(serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null))
        }
    }
}

/// The definition registry is unavailable in this runtime: the declaration is
/// admitted, identified and journaled like any other intent, so realization
/// refuses in the shared typed vocabulary rather than reporting a
/// registration that never happened.
fn process_definition_registry_unavailable(engine_kind: &str) -> crate::PluginError {
    crate::PluginError::Session(format!(
        "process definition registry is unavailable in this runtime: \
         cannot register a `{engine_kind}` definition"
    ))
}

/// Realization of one [`RegisterProcessDefinitionIntent`](crate::tool_intent::RegisterProcessDefinitionIntent)
/// against the registry (FIG-2995).
///
/// Resolve-once discipline: the engine registry resolves the definition
/// reference first, and the durable row pins the engine's authoritative
/// signature and its derived fingerprint. The name is tool input only and
/// never reaches a durable consumer record. The write carries the caller's
/// compare-and-swap expectation, so a stale expected revision, or a
/// take-over of a name without the caller's endorsement, refuses with the
/// registry's typed conflict instead of rewriting silently.
#[expect(
    clippy::expect_used,
    reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
)]
async fn realize_register_process_definition(
    context: &ToolDispatchContext<'_>,
    intent: &crate::RegisterProcessDefinitionIntent,
    identity: &crate::ToolIntentIdentity,
    registry: Arc<dyn crate::ProcessDefinitionRegistry>,
) -> Result<crate::ProcessDefinitionRegistration, crate::PluginError> {
    let name = intent
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            crate::PluginError::Session(
                "process definition registration requires a registered name".to_string(),
            )
        })?;
    crate::process_registry::validate_process_definition_name(name)?;
    // A name resolves once, at intent execution. An existing slot under this
    // name resolves its pinned record; a fresh registration resolves through
    // the engine directly.
    let existing = crate::process_registry::resolve_named_definition(
        registry.as_ref(),
        &intent.session_id,
        name,
    )
    .await?;
    let pinned = match existing.as_ref() {
        Some(existing) => {
            if existing.definition.engine_kind.as_str() != intent.engine_kind {
                return Err(crate::PluginError::Session(format!(
                    "process definition name `{name}` is registered under engine kind \
                     `{}`, not `{}`",
                    existing.definition.engine_kind, intent.engine_kind
                )));
            }
            existing.definition.clone()
        }
        None => crate::ProcessDefinitionRef::unclaimed(
            intent.engine_kind.clone(),
            intent.definition.clone(),
        ),
    };
    let resolution = context
        .process_engines
        .resolve(&pinned)
        .await
        .map_err(crate::PluginError::from)?;
    let pinned = pinned.with_resolved_signature(resolution.signature);
    let expectation = match existing
        .as_ref()
        .map(|existing| (existing.revision, existing.fingerprint.clone()))
    {
        Some((existing_revision, existing_fingerprint)) => {
            // A re-registration must carry the caller's observed revision;
            // without it the write would silently take the name over, which
            // the compare-and-swap fence forbids.
            let Some(observed_revision) = intent.expected_revision else {
                return Err(crate::PluginError::Session(format!(
                    "process definition name `{name}` is registered at revision \
                     {existing_revision}; re-registration requires the caller's \
                     revision compare-and-swap"
                )));
            };
            Some(crate::ProcessDefinitionExpectation::observed(
                observed_revision,
                existing_fingerprint,
            ))
        }
        None => None,
    };
    // The CAS write crosses the runtime-effect seam exactly like
    // `register_recorded_trigger` (FIG-3470): the journaled admission — not
    // this call — owns the durable write, so a redrive replays the recorded
    // registration and a group child admits it under its own binding.
    let scoped = context.effect_controller.scoped();
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            scoped.execution_scope().clone(),
            identity.replay_key.clone(),
        )
        .expect("tool-intent execution carries an admitted effect scope"),
        context.parentless_attribution(),
        identity.replay_key.clone(),
    )
    .with_replay_attribution(crate::RuntimeReplayAttribution::ToolIntent(
        identity.clone(),
    ));
    let outcome = scoped
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::process(crate::ProcessCommand::RegisterDefinition {
                    owner_scope: crate::TriggerOwnerScope::session(intent.session_id.clone()),
                    name: name.to_string(),
                    pinned,
                    expectation,
                }),
            ),
            crate::RuntimeEffectLocalExecutor::process_definitions(registry),
        )
        .await
        .map_err(crate::PluginError::RuntimeEffectController)?
        .into_process()
        .map_err(crate::PluginError::RuntimeEffectController)?;
    match outcome {
        crate::ProcessEffectOutcome::RegisterDefinition { registration } => Ok(*registration),
        other => Err(crate::PluginError::Session(format!(
            "process definition registration returned a non-registration outcome: {other:?}"
        ))),
    }
}

/// Install one recorded subscription draft through the trigger effect the
/// foreground registration path uses, keyed by the declaration's replay key so
/// a redrive re-installs the same subscription instead of a second one.
#[expect(
    clippy::expect_used,
    reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
)]
async fn register_recorded_trigger(
    context: &ToolDispatchContext<'_>,
    router: &crate::TriggerRouter,
    identity: &crate::ToolIntentIdentity,
    draft: crate::TriggerSubscriptionDraft,
) -> Result<crate::TriggerMutationReceipt, crate::PluginError> {
    let scoped = context.effect_controller.scoped();
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            scoped.execution_scope().clone(),
            identity.replay_key.clone(),
        )
        .expect("tool-intent execution carries an admitted effect scope"),
        context.parentless_attribution(),
        identity.replay_key.clone(),
    )
    .with_replay_attribution(crate::RuntimeReplayAttribution::ToolIntent(
        identity.clone(),
    ));
    let session_scope = crate::SessionScope::new(context.session_id.clone());
    let outcome = scoped
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::Trigger {
                    command: Box::new(crate::TriggerCommand::Register {
                        owner_scope: crate::TriggerOwnerScope::session(context.session_id.clone()),
                        actor: crate::ProcessOriginator::session(session_scope),
                        draft,
                    }),
                },
            ),
            crate::RuntimeEffectLocalExecutor::triggers(router.store()),
        )
        .await
        .map_err(crate::PluginError::RuntimeEffectController)?
        .into_trigger()
        .map_err(crate::PluginError::RuntimeEffectController)?
        .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    match outcome {
        crate::TriggerCommandOutcome::Mutation { receipt } => Ok(*receipt),
        other => Err(crate::PluginError::Session(format!(
            "trigger registration returned a non-mutation outcome: {other:?}"
        ))),
    }
}

fn error_code(error: &crate::PluginError) -> String {
    match error {
        crate::PluginError::RuntimeEffectController(error) => error.code.as_str().to_string(),
        crate::PluginError::ProcessNotVisible { .. } => "process_not_visible".to_string(),
        crate::PluginError::ProcessAlreadyTerminal { .. } => "process_already_terminal".to_string(),
        crate::PluginError::ParentEnded { .. } => "process_parent_ended".to_string(),
        crate::PluginError::ProcessCancelConflict { .. } => "process_cancel_conflict".to_string(),
        crate::PluginError::ProcessNoLongerRetained { .. } => {
            "process_no_longer_retained".to_string()
        }
        _ => "plugin".to_string(),
    }
}

fn error_message(error: &crate::PluginError) -> String {
    match error {
        crate::PluginError::RuntimeEffectController(error) => error.message.clone(),
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessId;

    fn signal(session_id: &SessionId, payload: serde_json::Value) -> crate::ToolIntent {
        crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
            session_id: SessionId::from(session_id.to_string()),
            process_id: ProcessId::from("process-1"),
            signal_name: "continue".to_string(),
            payload,
        })
    }

    #[test]
    fn protocol_dispatch_refuses_predecessor_and_unknown_records() {
        for recorded in [0, 1, 2, 4] {
            let intents = crate::ToolIntents {
                protocol_version: recorded,
                intents: vec![signal(
                    &SessionId::from("session"),
                    serde_json::json!({"value": 1}),
                )],
            };
            assert_eq!(
                admit_batch(&SessionId::from("session"), Some("call"), &intents),
                Some(crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded })
            );
        }
    }

    #[test]
    fn admission_is_all_or_nothing_for_total_count_overflow() {
        let intents = crate::ToolIntents::v3(
            (0..=crate::TOOL_INTENT_MAX_COUNT)
                .map(|index| {
                    signal(
                        &SessionId::from("session"),
                        serde_json::json!({"index": index}),
                    )
                })
                .collect(),
        );
        assert_eq!(
            admit_batch(&SessionId::from("session"), Some("call"), &intents),
            Some(crate::ToolIntentRefusalReason::CountBudgetExceeded {
                actual: 33,
                maximum: 32,
            })
        );
    }

    #[test]
    fn admission_is_all_or_nothing_for_per_kind_overflow() {
        let intents = crate::ToolIntents::v3(
            (0..=crate::TOOL_INTENT_MAX_PER_KIND)
                .map(|index| {
                    signal(
                        &SessionId::from("session"),
                        serde_json::json!({"index": index}),
                    )
                })
                .collect(),
        );
        assert_eq!(
            admit_batch(&SessionId::from("session"), Some("call"), &intents),
            Some(crate::ToolIntentRefusalReason::PerKindBudgetExceeded {
                kind: crate::ToolIntentKind::SignalProcess,
                actual: 17,
                maximum: 16,
            })
        );
    }

    #[test]
    fn admission_is_all_or_nothing_for_canonical_byte_overflow() {
        let intents = crate::ToolIntents::v3(vec![signal(
            &SessionId::from("session"),
            serde_json::json!({"payload": "x".repeat(crate::TOOL_INTENT_MAX_CANONICAL_BYTES)}),
        )]);
        assert!(matches!(
            admit_batch(&SessionId::from("session"), Some("call"), &intents),
            Some(
                crate::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                    maximum: crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
                    ..
                }
            )
        ));
    }

    #[test]
    fn admission_refuses_the_entire_batch_on_session_mismatch() {
        let intents = crate::ToolIntents::v3(vec![
            signal(&SessionId::from("session"), serde_json::json!({"index": 0})),
            signal(
                &SessionId::from("other-session"),
                serde_json::json!({"index": 1}),
            ),
        ]);
        assert_eq!(
            admit_batch(&SessionId::from("session"), Some("call"), &intents),
            Some(crate::ToolIntentRefusalReason::SessionMismatch {
                expected: "session".to_string(),
                recorded: "other-session".to_string(),
            })
        );
    }

    #[test]
    fn outcome_model_addenda_have_literal_stable_text() {
        let executed = crate::ToolIntentExecutionOutcome::Executed {
            identity: crate::ToolIntentIdentity {
                session_id: SessionId::from("session"),
                execution_scope_id: "turn".to_string(),
                tool_call_id: "call".to_string(),
                intent_index: 4,
                replay_key: "tool-intent-v1-literal".to_string(),
                minting_emission_replay_key: None,
            },
            kind: crate::ToolIntentKind::CancelProcess,
            result: serde_json::json!({"cancelled": true}),
        };
        assert_eq!(
            executed.model_addendum(),
            "[tool intent cancel_process #4 executed: {\"cancelled\":true}]"
        );

        let refused = crate::ToolIntentExecutionOutcome::Refused {
            identity: None,
            intent_index: 0,
            kind: crate::ToolIntentKind::StartProcess,
            refusal: crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded: 1 },
        };
        assert_eq!(
            refused.model_addendum(),
            "[tool intent start_process #0 refused: unsupported_protocol_version]"
        );
    }
}
