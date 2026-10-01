use std::collections::BTreeMap;
use std::sync::Arc;

use super::ToolDispatchContext;

pub async fn execute_final_tool_intents(
    context: &ToolDispatchContext<'_>,
    tool_call_id: &lash_sansio::ToolCallId,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<Vec<crate::ToolIntentExecutionOutcome>, crate::RuntimeEffectControllerError> {
    let execution_scope_id = context.effect_controller.scope_id().to_string();
    if intents.intents.is_empty() && intents.protocol_version == crate::TOOL_INTENT_PROTOCOL_V3 {
        return Ok(Vec::new());
    }
    if let Some(refusal) = admit_batch(&context.owner.runtime_owner(), intents) {
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
            owner = %identity.owner,
            execution_scope_id = %identity.execution_scope_id,
            tool_call_id = %identity.tool_call_id,
            intent_index = identity.intent_index,
            intent_kind = intent.kind().as_str(),
            replay_key = %identity.replay_key,
        );
        let _entered = span.enter();
        if let crate::ToolIntent::RegisterTrigger(registration) = intent
            && let Some(refusal) = validate_trigger_registration_authority(context, registration)
        {
            outcomes.push(refused(index, intent.kind(), Some(identity), refusal));
            continue;
        }
        let result = execute_one(context, intent, &identity, child_trace_hook).await;
        let outcome = match result {
            Ok(result) => {
                record_executed_metric(intent.kind());
                crate::ToolIntentExecutionOutcome::Executed {
                    identity,
                    realized: result,
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
                    cause: crate::ToolIntentCommandFailure::from(&error),
                },
            ),
        };
        tracing::info!(
            target: "lash::tool_intent",
            outcome = match &outcome {
                crate::ToolIntentExecutionOutcome::Executed { .. } => "executed",
                crate::ToolIntentExecutionOutcome::Refused { .. } => "refused",
                crate::ToolIntentExecutionOutcome::ProtocolRefused { .. } => "protocol_refused"
            },
            "tool intent outcome"
        );
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// The declared-start launch entry (ADR 0116 §3.2): realizes the one start a
/// pending call declared exactly as a `StartProcess` drain realizes it — the
/// same request under the same derived key, the same journaled admission and
/// the same child-trace hook — and answers the call's launch receipt.
///
/// `scope` carries the call's lineage as its parent. A realized start answers
/// `Executed`; a typed refusal answers `Refused` and settles the call. An
/// `Err` is a fault the call cannot settle (see [`declared_start_fault`]).
pub(crate) async fn realize_declared_start(
    processes: &dyn crate::ProcessService,
    start: &crate::DeclaredStart,
    scope: crate::ProcessOpScope<'_>,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<crate::ToolIntentExecutionOutcome, crate::RuntimeEffectControllerError> {
    let identity = start.identity().clone();
    let parent = scope.parent_invocation.clone().map(|parent| {
        parent.with_replay_attribution(crate::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        ))
    });
    let scope = scope.with_parent_invocation(parent);
    let kind = crate::ToolIntentKind::StartProcess;
    match processes
        .start_from_recorded_intent(&start.start().owner, start.request(), scope)
        .await
    {
        Ok(handle) => {
            // The declared kind names the child's entry, so a trace links the
            // call to a child it can name.
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: handle.process_id.clone(),
                    attempt: None,
                    child_entry_name: start
                        .start()
                        .declaration
                        .identity
                        .as_ref()
                        .map(|identity| identity.kind.as_str().to_string()),
                });
            }
            record_executed_metric(kind);
            Ok(crate::ToolIntentExecutionOutcome::Executed {
                identity,
                realized: crate::ToolIntentRealized::StartProcess(handle),
            })
        }
        Err(error) => match declared_start_fault(&error) {
            Some(fault) => Err(fault),
            None => Ok(refused(
                identity.intent_index as usize,
                kind,
                Some(identity),
                crate::ToolIntentRefusalReason::CommandFailed {
                    cause: crate::ToolIntentCommandFailure::from(&error),
                },
            )),
        },
    }
}

/// A declared start's error the call cannot settle as its result: a replay
/// divergence, a cancel decided before the launch or during it, or a live
/// fault, which the engine's redelivery retries (ADR 0116 §3.2). Every
/// other error is the start's typed refusal — among them a terminal
/// controller error such as a closed scope's `ParentEnded`, which a retry
/// would only meet again.
pub(crate) fn declared_start_fault(
    error: &crate::PluginError,
) -> Option<crate::RuntimeEffectControllerError> {
    match error {
        crate::PluginError::RuntimeEffectController(error)
            if error.code.is_replay_mismatch()
                || error.code == crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided
                || error.code == crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled
                || error.turn_failure_cause() != crate::TurnFailureCause::Outcome =>
        {
            Some(error.clone())
        }
        crate::PluginError::SessionExecutionLeaseLost { .. } => {
            Some(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::SessionExecutionLeaseLost,
                error.to_string(),
            ))
        }
        crate::PluginError::Session(_) | crate::PluginError::ProcessExecutionSuperseded { .. } => {
            Some(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::PluginSessionManager,
                error.to_string(),
            ))
        }
        _ => None,
    }
}

pub(super) fn admit_batch(
    owner: &crate::RuntimeOwner,
    intents: &crate::ToolIntents,
) -> Option<crate::ToolIntentRefusalReason> {
    if intents.protocol_version != crate::TOOL_INTENT_PROTOCOL_V3 {
        return Some(crate::ToolIntentRefusalReason::UnsupportedProtocolVersion {
            recorded: intents.protocol_version,
        });
    }
    if intents.intents.len() > crate::TOOL_INTENT_MAX_COUNT {
        return Some(crate::ToolIntentRefusalReason::CountBudgetExceeded {
            actual: intents.intents.len(),
            maximum: crate::TOOL_INTENT_MAX_COUNT,
        });
    }
    let canonical_bytes = intents.declared_canonical_bytes().unwrap_or(usize::MAX);
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
        if intent.owner() != owner {
            return Some(crate::ToolIntentRefusalReason::OwnerMismatch {
                expected: owner.clone(),
                recorded: intent.owner().clone(),
            });
        }
        if let crate::ToolIntent::StartProcess(start) = intent
            && let Some(refusal) = start.admission_refusal()
        {
            return Some(refusal);
        }
    }
    None
}

fn refuse_all(
    context: &ToolDispatchContext<'_>,
    execution_scope_id: &str,
    tool_call_id: &lash_sansio::ToolCallId,
    intents: &crate::ToolIntents,
    refusal: crate::ToolIntentRefusalReason,
) -> Vec<crate::ToolIntentExecutionOutcome> {
    if intents.intents.is_empty() {
        let span = tracing::info_span!(
            target: "lash::tool_intent",
            "tool_intent.execute",
            owner = %context.owner.runtime_owner(),
            execution_scope_id,
            tool_call_id = %tool_call_id,
            intent_index = tracing::field::Empty,
            intent_kind = "<batch>",
            replay_key = "<unavailable>",
        );
        let _entered = span.enter();
        tracing::warn!(
            target: "lash::tool_intent",
            refusal_reason = %refusal.code(),
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
                owner = %context.owner.runtime_owner(),
                execution_scope_id,
                tool_call_id = %tool_call_id,
                intent_index = index,
                intent_kind = intent.kind().as_str(),
                replay_key = identity.as_ref().map_or("<unavailable>", |identity| identity.replay_key.as_str()),
            );
            let _entered = span.enter();
            refused(index, intent.kind(), identity, refusal.clone())
        })
        .collect()
}

/// The identity a call's attempt derives for its declared start (index 0),
/// minted under `minting_emission`, the attempt's own invocation: the one a
/// declared start is bound to before it launches (ADR 0116 §3.1).
pub(crate) fn declaring_identity(
    context: &ToolDispatchContext<'_>,
    tool_call_id: &lash_sansio::ToolCallId,
    minting_emission: &crate::RuntimeInvocation,
) -> crate::ToolIntentIdentity {
    crate::derive_tool_intent_identity_under(
        &context.owner.runtime_owner(),
        context.effect_controller.scope_id(),
        tool_call_id,
        0,
        Some(minting_emission),
    )
}

fn derive_identity(
    context: &ToolDispatchContext<'_>,
    execution_scope_id: &str,
    tool_call_id: &lash_sansio::ToolCallId,
    intent_index: usize,
) -> Result<crate::ToolIntentIdentity, crate::ToolIntentRefusalReason> {
    let intent_index = u32::try_from(intent_index)
        .map_err(|_| crate::ToolIntentRefusalReason::IntentIndexOverflow)?;
    Ok(crate::derive_tool_intent_identity_under(
        &context.owner.runtime_owner(),
        execution_scope_id,
        tool_call_id,
        intent_index,
        context.parent_invocation.as_ref(),
    ))
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
        refusal_reason = %refusal.code(),
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

pub(super) fn validate_trigger_registration_authority(
    context: &ToolDispatchContext<'_>,
    intent: &crate::RegisterTriggerIntent,
) -> Option<crate::ToolIntentRefusalReason> {
    let expected_owner = match crate::resolve_trigger_owner_scope(
        &context.owner.runtime_owner(),
        context.process_originator.as_ref(),
    ) {
        Ok(owner) => owner,
        Err(error) => {
            return Some(crate::ToolIntentRefusalReason::CommandFailed {
                cause: crate::ToolIntentCommandFailure::from(&error),
            });
        }
    };
    if intent.owner_scope != expected_owner {
        return Some(crate::ToolIntentRefusalReason::ForeignTriggerOwnerScope {
            expected: expected_owner,
            recorded: intent.owner_scope.clone(),
        });
    }
    let expected_actor = match (&context.process_originator, &context.owner) {
        (Some(originator), _) => originator.clone(),
        (
            None,
            crate::ExecutionOwner::SessionFrame {
                session_id,
                agent_frame_id,
            },
        ) => crate::ProcessOriginator::session(crate::SessionScope::for_agent_frame(
            session_id.clone(),
            agent_frame_id.clone(),
        )),
        (None, crate::ExecutionOwner::Process { process_id }) => {
            let error = crate::runtime::not_a_session_runtime("trigger_actor", process_id);
            return Some(crate::ToolIntentRefusalReason::CommandFailed {
                cause: crate::ToolIntentCommandFailure::from(&error),
            });
        }
    };
    (intent.actor != expected_actor).then(|| crate::ToolIntentRefusalReason::ForeignTriggerActor {
        expected: expected_actor,
        recorded: intent.actor.clone(),
    })
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
    tool_intent_metrics().record_refused(kind.as_str(), refusal.code().as_ref());
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
) -> Result<crate::ToolIntentRealized, crate::PluginError> {
    let parent = context.parent_invocation.clone().unwrap_or_else(|| {
        crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(
                context.effect_controller.execution_scope().clone(),
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
    let scope = crate::ProcessOpScope::new(context.effect_controller.clone())
        .with_parent_invocation(Some(parent))
        .with_agent_frame_id(context.owner.agent_frame_id().cloned())
        .with_process_lineage(context.process_lineage.clone());

    match intent {
        crate::ToolIntent::StartProcess(intent) => {
            // The declaration carries no id: the sole constructor on this path
            // derives it from this declaration's identity, so a redrive of the
            // same attempt starts the same process id (FIG-2994).
            let request = intent.into_request(identity);
            let summary = context
                .processes
                .start_from_recorded_intent(&intent.owner, request, scope)
                .await?;
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: summary.process_id.clone(),
                    attempt: None,
                    child_entry_name: None,
                });
            }
            Ok(crate::ToolIntentRealized::StartProcess(summary))
        }
        crate::ToolIntent::SignalProcess(intent) => {
            let event = context
                .processes
                .signal_recorded_intent(
                    &intent.owner,
                    &intent.process_id,
                    intent.signal_name.clone(),
                    identity.replay_key.clone(),
                    intent.payload.clone(),
                    scope,
                )
                .await?;
            Ok(crate::ToolIntentRealized::SignalProcess(Box::new(event)))
        }
        crate::ToolIntent::CancelProcess(intent) => {
            let record = context
                .processes
                .cancel_recorded_intent(&intent.owner, &intent.process_id, identity.clone(), scope)
                .await?;
            Ok(crate::ToolIntentRealized::CancelProcess(
                crate::ProcessCancelReceipt::from_record(record)?,
            ))
        }
        crate::ToolIntent::EmitProcessEvent(intent) => {
            let event = context
                .processes
                .emit_event_recorded_intent(
                    &intent.owner,
                    &intent.process_id,
                    intent.event_type.clone(),
                    identity.replay_key.clone(),
                    intent.payload.clone(),
                    scope,
                )
                .await?;
            Ok(crate::ToolIntentRealized::EmitProcessEvent(Box::new(event)))
        }
        crate::ToolIntent::EmitTrigger(intent) => {
            // The router owns the whole emission, but the durable declaration
            // owns its occurrence identity. Stamp the request with that
            // declaration's replay key so two declarations cannot collapse
            // merely because their callers reused a key. A redrive retains the
            // same replay key, so it ingests the same occurrence, or is served
            // its recorded ingest, and replays the same deterministic delivery
            // starts. A redrive with no journal, after retention reclaimed the
            // occurrence, is refused by the store and writes nothing
            // (FIG-4513).
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
                Box::pin(router.emit_recorded(request, &context.effect_controller)).await?;
            Ok(crate::ToolIntentRealized::EmitTrigger(report))
        }
        crate::ToolIntent::PublishDefinition(intent) => realize_definition(
            context,
            identity,
            crate::ProcessCommand::PublishDefinition {
                draft: intent.draft.clone(),
                module: intent.module.clone(),
            },
        )
        .await
        .map(|definition| crate::ToolIntentRealized::PublishDefinition(Box::new(definition))),
        crate::ToolIntent::GetDefinition(intent) => realize_definition(
            context,
            identity,
            crate::ProcessCommand::GetDefinition {
                definition_id: intent.definition_id.clone(),
            },
        )
        .await
        .map(|definition| crate::ToolIntentRealized::GetDefinition(Box::new(definition))),
        crate::ToolIntent::RegisterTrigger(intent) => {
            let router = context.trigger_router.as_ref().ok_or_else(|| {
                crate::PluginError::Session(
                    "trigger store is unavailable in this runtime".to_string(),
                )
            })?;
            Ok(crate::ToolIntentRealized::RegisterTrigger(
                register_recorded_trigger(context, router, identity, intent).await?,
            ))
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "the controller carries an admitted scope"
)]
async fn realize_definition(
    context: &ToolDispatchContext<'_>,
    identity: &crate::ToolIntentIdentity,
    command: crate::ProcessCommand,
) -> Result<crate::ProcessDefinition, crate::PluginError> {
    let scoped = context.effect_controller.clone();
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            scoped.execution_scope().clone(),
            identity.replay_key.clone(),
        )
        .expect("admitted scope"),
        context.parentless_attribution(),
        identity.replay_key.clone(),
    )
    .with_replay_attribution(crate::RuntimeReplayAttribution::ToolIntent(
        identity.clone(),
    ));
    let claim = crate::session::execution_claim_of(scoped.execution_scope())?;
    let result = scoped
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::process(command),
            ),
            crate::RuntimeEffectLocalExecutor::definition_artifacts(
                context.process_engines.clone(),
                claim,
            ),
        )
        .await?
        .into_process()?;
    match result {
        crate::ProcessEffectOutcome::Definition { definition } => Ok(*definition),
        _ => Err(crate::PluginError::Session(
            "definition effect returned a different outcome".into(),
        )),
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
    intent: &crate::RegisterTriggerIntent,
) -> Result<Box<crate::TriggerMutationReceipt>, crate::PluginError> {
    let scoped = context.effect_controller.clone();
    let draft = intent.draft.clone();
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
    // The registration holds the revision it commits before it commits,
    // under the intent's journal (ADR 0113 §3.4).
    let creator = scoped
        .execution_scope()
        .journal_identity()
        .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let store: Arc<dyn crate::TriggerStore> =
        Arc::new(crate::triggers::RevisionReferrerTriggerStore::new(
            router.store(),
            context.process_engines.clone(),
            creator,
        ));
    let outcome = scoped
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::Trigger {
                    command: Box::new(crate::TriggerCommand::Register {
                        owner_scope: intent.owner_scope.clone(),
                        actor: intent.actor.clone(),
                        draft,
                    }),
                },
            ),
            crate::RuntimeEffectLocalExecutor::triggers(store),
        )
        .await
        .map_err(crate::PluginError::RuntimeEffectController)?
        .into_trigger()
        .map_err(crate::PluginError::RuntimeEffectController)?
        .map_err(|error| crate::PluginError::TriggerOperation(Box::new(error)))?;
    match outcome {
        crate::TriggerCommandOutcome::Mutation { receipt } => Ok(receipt),
        other => Err(crate::PluginError::Session(format!(
            "trigger registration returned a non-mutation outcome: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn store_refusal_intents_report_typed_codes() {
        for (error, expected) in [
            (
                crate::StoreError::WriterFenced {
                    recorded: 2,
                    writable: crate::compat::VersionRange::exactly(1),
                },
                "writer_fenced",
            ),
            (
                crate::StoreError::Incompatible {
                    refusal: crate::compat::CompatRefusal::Unstamped {
                        component: "postgres".into(),
                        writing_release: None,
                    },
                },
                "store_incompatible",
            ),
        ] {
            assert_eq!(
                crate::ToolIntentCommandFailure::from(&crate::PluginError::from(error)).code(),
                expected
            );
        }
    }

    use super::*;

    fn session(id: &str) -> crate::RuntimeOwner {
        crate::RuntimeOwner::Session(crate::SessionId::from(id))
    }

    fn signal(owner: &crate::RuntimeOwner, payload: serde_json::Value) -> crate::ToolIntent {
        crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
            owner: owner.clone(),
            process_id: crate::process_id_for_test("process-1"),
            signal_name: "continue".to_string(),
            payload,
        })
    }

    #[test]
    fn protocol_dispatch_refuses_predecessor_and_unknown_records() {
        for recorded in [0, 1, 2, 4] {
            let intents = crate::ToolIntents {
                protocol_version: recorded,
                intents: vec![signal(&session("session"), serde_json::json!({"value": 1}))],
            };
            assert_eq!(
                admit_batch(&session("session"), &intents),
                Some(crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded })
            );
        }
    }

    #[test]
    fn admission_is_all_or_nothing_for_total_count_overflow() {
        let intents = crate::ToolIntents::v3(
            (0..=crate::TOOL_INTENT_MAX_COUNT)
                .map(|index| signal(&session("session"), serde_json::json!({"index": index})))
                .collect(),
        );
        assert_eq!(
            admit_batch(&session("session"), &intents),
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
                .map(|index| signal(&session("session"), serde_json::json!({"index": index})))
                .collect(),
        );
        assert_eq!(
            admit_batch(&session("session"), &intents),
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
            &session("session"),
            serde_json::json!({"payload": "x".repeat(crate::TOOL_INTENT_MAX_CANONICAL_BYTES)}),
        )]);
        assert!(matches!(
            admit_batch(&session("session"), &intents),
            Some(
                crate::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                    maximum: crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
                    ..
                }
            )
        ));
    }

    /// A start carrying `payload` as its declared input under an execution
    /// env captured from a session whose recorded protocol prompt is `prompt`.
    fn start_under_prompt(payload: serde_json::Value, prompt: String) -> crate::ToolIntent {
        let policy =
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024));
        let mut plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
        plugin_config.insert(
            "protocol",
            serde_json::json!({ "prompt": { "instructions": [prompt] } }),
        );
        crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
            owner: session("session"),
            declaration: crate::ProcessStartDeclaration::new(
                crate::ProcessInput::Engine {
                    kind: "lashlang".to_string(),
                    payload,
                },
                crate::ProcessOriginator::host(),
                crate::Lifetime::Detached,
            )
            .with_env_ref(
                crate::ProcessExecutionEnvSpec::new(
                    crate::AdmittedPluginConfig::new(plugin_config, 0),
                    policy,
                )
                .stable_ref()
                .expect("environment digest"),
            ),
        }))
    }

    #[test]
    fn a_128_kib_environment_keeps_durable_start_rows_under_the_intent_budget() {
        let intent = start_under_prompt(
            serde_json::json!({"key": "child/0"}),
            "x".repeat(128 * 1024),
        );
        let batch = crate::ToolIntents::v3(vec![intent.clone()]);
        let identity = crate::derive_tool_intent_identity(
            &session("session"),
            "scope",
            &crate::ToolCallId::fixture("start"),
            0,
        );
        let submission = crate::ToolIntentSubmissionRecord::new(identity.clone(), intent.clone())
            .expect("submission");
        let crate::ToolIntent::StartProcess(start) = intent else {
            panic!("start");
        };
        let request = start.into_request(&identity);
        let record = crate::ProcessRecord::from_registration(
            request
                .clone()
                .into_registration()
                .stating_input()
                .expect("the start states its input"),
            crate::ProcessId::fixture("large-environment-child"),
        );
        let command = crate::ProcessCommand::Start {
            registration: request.clone().into_registration(),
            observers: Vec::new(),
            execution_context: Box::default(),
        };
        for (name, bytes) in [
            ("intent batch", serde_json::to_vec(&batch).expect("batch")),
            (
                "intent row",
                serde_json::to_vec(&submission).expect("submission"),
            ),
            (
                "journal command",
                serde_json::to_vec(&command).expect("command"),
            ),
            ("process row", serde_json::to_vec(&record).expect("record")),
        ] {
            assert!(
                bytes.len() < crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
                "{name} stores {} bytes for a 128 KiB environment",
                bytes.len()
            );
        }
        assert_eq!(
            batch.declared_canonical_bytes().expect("budget"),
            serde_json::to_vec(&batch).expect("batch").len()
        );
    }

    /// FIG-4256: the whole declaration counts, including its environment
    /// digest, while large captured instructions live in the artifact store.
    #[test]
    fn admission_counts_the_complete_declaration_with_its_environment_digest() {
        let prompt = "x".repeat(2 * crate::TOOL_INTENT_MAX_CANONICAL_BYTES);
        let intents = crate::ToolIntents::v3(vec![start_under_prompt(
            serde_json::json!({"key": "child/0"}),
            prompt,
        )]);
        assert!(
            serde_json::to_vec(&intents).expect("encode").len()
                < crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
            "the batch carries only the environment digest"
        );
        assert_eq!(admit_batch(&session("session"), &intents), None);
    }

    /// The same bytes in what the start declares are the attempt's, and the
    /// budget still refuses them.
    #[test]
    fn admission_still_bounds_what_a_start_declares() {
        let intents = crate::ToolIntents::v3(vec![start_under_prompt(
            serde_json::json!({"key": "x".repeat(crate::TOOL_INTENT_MAX_CANONICAL_BYTES)}),
            String::new(),
        )]);
        assert!(matches!(
            admit_batch(&session("session"), &intents),
            Some(
                crate::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                    maximum: crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
                    ..
                }
            )
        ));
    }

    #[test]
    fn admission_refuses_the_entire_batch_on_owner_mismatch() {
        let intents = crate::ToolIntents::v3(vec![
            signal(&session("session"), serde_json::json!({"index": 0})),
            signal(&session("other-session"), serde_json::json!({"index": 1})),
        ]);
        assert_eq!(
            admit_batch(&session("session"), &intents),
            Some(crate::ToolIntentRefusalReason::OwnerMismatch {
                expected: session("session"),
                recorded: session("other-session"),
            })
        );
    }

    #[test]
    fn outcome_model_addenda_have_literal_stable_text() {
        let executed = crate::ToolIntentExecutionOutcome::Executed {
            identity: crate::ToolIntentIdentity {
                owner: session("session"),
                execution_scope_id: "turn".to_string(),
                tool_call_id: crate::ToolCallId::fixture("call"),
                intent_index: 4,
                replay_key: "tool-intent-v1-literal".to_string(),
                minting_emission_replay_key: None,
            },
            realized: crate::ToolIntentRealized::CancelProcess(crate::ProcessCancelReceipt {
                process_id: crate::ProcessId::fixture("cancelled"),
                status: crate::ProcessStatus::Cancelled,
                origin: crate::CancelOrigin::ModelRequested,
            }),
        };
        assert_eq!(
            executed.model_addendum(),
            format!(
                "[tool intent cancel_process #4 executed: {{\"origin\":\"model_requested\",\"process_id\":\"{}\",\"status\":\"cancelled\"}}]",
                crate::ProcessId::fixture("cancelled")
            )
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
