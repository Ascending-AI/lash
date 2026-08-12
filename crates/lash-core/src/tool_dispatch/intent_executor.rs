use std::collections::BTreeMap;

use super::ToolDispatchContext;

pub(crate) async fn execute_final_tool_intents(
    context: &ToolDispatchContext<'_>,
    tool_call_id: Option<&str>,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Vec<crate::ToolIntentExecutionOutcome> {
    let execution_scope_id = context.effect_controller.scoped().scope_id().to_string();
    if let Some(refusal) = admit_batch(&context.session_id, tool_call_id, intents) {
        return refuse_all(context, &execution_scope_id, tool_call_id, intents, refusal);
    }
    if intents.intents.is_empty() {
        return Vec::new();
    }

    let mut outcomes = Vec::with_capacity(intents.intents.len());
    for (index, intent) in intents.intents.iter().enumerate() {
        let identity = match crate::derive_tool_intent_identity(
            &context.session_id,
            &execution_scope_id,
            tool_call_id,
            index,
        ) {
            Ok(identity) => identity,
            Err(refusal) => {
                outcomes.push(refused(index, intent.kind(), None, refusal));
                continue;
            }
        };
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
    outcomes
}

pub(crate) fn record_parent_end_actions(
    context: &ToolDispatchContext<'_>,
    intents: &crate::ToolIntents,
    outcomes: &[crate::ToolIntentExecutionOutcome],
) {
    for (intent, outcome) in intents.intents.iter().zip(outcomes) {
        let crate::ToolIntent::StartProcess(intent) = intent else {
            continue;
        };
        let crate::ToolIntentExecutionOutcome::Executed {
            identity,
            kind: crate::ToolIntentKind::StartProcess,
            ..
        } = outcome
        else {
            continue;
        };
        context
            .parent_end_actions
            .enqueue(super::context::ParentEndAction {
                identity: identity.clone(),
                policy: intent.on_parent_end,
            });
    }
}

pub(crate) async fn execute_parent_end_actions(context: &ToolDispatchContext<'_>) {
    for action in context.parent_end_actions.drain() {
        let reason = match action.policy {
            crate::ProcessParentEndPolicy::Abandon => continue,
            crate::ProcessParentEndPolicy::Cancel => {
                "recorded start intent parent ended with cancel policy"
            }
        };
        let replay_key = format!("{}:parent-end", action.identity.replay_key);
        let parent = crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new(&action.identity.session_id),
            format!("tool-intent-parent-end:{}", action.identity.intent_index),
            crate::RuntimeEffectKind::ToolBatch,
            replay_key.clone(),
        );
        let mut parent = parent;
        parent.replay = Some(crate::RuntimeReplay {
            key: replay_key,
            attribution: Some(crate::RuntimeReplayAttribution::ToolIntent(
                action.identity.clone(),
            )),
        });
        let scope = crate::ProcessOpScope::new(context.effect_controller.scoped())
            .with_parent_invocation(Some(parent))
            .with_agent_frame_id(Some(context.agent_frame_id.clone()));
        let result = context
            .processes
            .cancel_recorded_intent(
                &action.identity.session_id,
                &action.identity.replay_key,
                Some(reason.to_string()),
                scope,
            )
            .await;
        match result {
            Ok(_) => tracing::info!(
                target: "lash::tool_intent",
                intent_index = action.identity.intent_index,
                policy = ?action.policy,
                "tool intent parent-end action executed"
            ),
            Err(error) => tracing::warn!(
                target: "lash::tool_intent",
                intent_index = action.identity.intent_index,
                policy = ?action.policy,
                code = %error_code(&error),
                message = %error_message(&error),
                "tool intent parent-end action refused"
            ),
        }
    }
}

fn admit_batch(
    session_id: &str,
    tool_call_id: Option<&str>,
    intents: &crate::ToolIntents,
) -> Option<crate::ToolIntentRefusalReason> {
    if intents.protocol_version != crate::TOOL_INTENT_PROTOCOL_V1 {
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
            let identity = crate::derive_tool_intent_identity(
                &context.session_id,
                execution_scope_id,
                tool_call_id,
                index,
            )
            .ok();
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

async fn execute_one(
    context: &ToolDispatchContext<'_>,
    intent: &crate::ToolIntent,
    identity: &crate::ToolIntentIdentity,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<serde_json::Value, crate::PluginError> {
    let mut parent = context.parent_invocation.clone().unwrap_or_else(|| {
        crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new(&context.session_id),
            format!("tool-intent:{}", identity.intent_index),
            crate::RuntimeEffectKind::ToolBatch,
            identity.replay_key.clone(),
        )
    });
    parent.replay = Some(crate::RuntimeReplay {
        key: identity.replay_key.clone(),
        attribution: Some(crate::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        )),
    });
    let scope = crate::ProcessOpScope::new(context.effect_controller.scoped())
        .with_parent_invocation(Some(parent))
        .with_agent_frame_id(Some(context.agent_frame_id.clone()));

    match intent {
        crate::ToolIntent::StartProcess(intent) => {
            let mut request = intent.request.clone();
            request.id = identity.replay_key.clone();
            let summary = context
                .processes
                .start_from_recorded_intent(&intent.session_id, request, scope)
                .await?;
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: summary.id.clone(),
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
                    intent.reason.clone(),
                    scope,
                )
                .await?;
            Ok(
                serde_json::to_value(crate::ProcessCancelSummary::from_record(record))
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
    }
}

fn error_code(error: &crate::PluginError) -> String {
    match error {
        crate::PluginError::RuntimeEffectController(error) => error.code.as_str().to_string(),
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

    fn signal(session_id: &str, payload: serde_json::Value) -> crate::ToolIntent {
        crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
            session_id: session_id.to_string(),
            process_id: "process-1".to_string(),
            signal_name: "continue".to_string(),
            payload,
        })
    }

    #[test]
    fn protocol_dispatch_refuses_fabricated_v0_and_v2_records() {
        for recorded in [0, 2] {
            let intents = crate::ToolIntents {
                protocol_version: recorded,
                intents: vec![signal("session", serde_json::json!({"value": 1}))],
            };
            assert_eq!(
                admit_batch("session", Some("call"), &intents),
                Some(crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded })
            );
        }
    }

    #[test]
    fn admission_is_all_or_nothing_for_total_count_overflow() {
        let intents = crate::ToolIntents::v1(
            (0..=crate::TOOL_INTENT_MAX_COUNT)
                .map(|index| signal("session", serde_json::json!({"index": index})))
                .collect(),
        );
        assert_eq!(
            admit_batch("session", Some("call"), &intents),
            Some(crate::ToolIntentRefusalReason::CountBudgetExceeded {
                actual: 33,
                maximum: 32,
            })
        );
    }

    #[test]
    fn admission_is_all_or_nothing_for_per_kind_overflow() {
        let intents = crate::ToolIntents::v1(
            (0..=crate::TOOL_INTENT_MAX_PER_KIND)
                .map(|index| signal("session", serde_json::json!({"index": index})))
                .collect(),
        );
        assert_eq!(
            admit_batch("session", Some("call"), &intents),
            Some(crate::ToolIntentRefusalReason::PerKindBudgetExceeded {
                kind: crate::ToolIntentKind::SignalProcess,
                actual: 17,
                maximum: 16,
            })
        );
    }

    #[test]
    fn admission_is_all_or_nothing_for_canonical_byte_overflow() {
        let intents = crate::ToolIntents::v1(vec![signal(
            "session",
            serde_json::json!({"payload": "x".repeat(crate::TOOL_INTENT_MAX_CANONICAL_BYTES)}),
        )]);
        assert!(matches!(
            admit_batch("session", Some("call"), &intents),
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
        let intents = crate::ToolIntents::v1(vec![
            signal("session", serde_json::json!({"index": 0})),
            signal("other-session", serde_json::json!({"index": 1})),
        ]);
        assert_eq!(
            admit_batch("session", Some("call"), &intents),
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
                session_id: "session".to_string(),
                execution_scope_id: "turn".to_string(),
                tool_call_id: "call".to_string(),
                intent_index: 4,
                replay_key: "tool-intent-v1-literal".to_string(),
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
            refusal: crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded: 2 },
        };
        assert_eq!(
            refused.model_addendum(),
            "[tool intent start_process #0 refused: unsupported_protocol_version]"
        );
    }
}
