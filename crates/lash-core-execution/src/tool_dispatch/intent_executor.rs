use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::Instrument as _;

use super::ToolDispatchContext;

/// The dispatch fields intent realization reads, carried without the rest of
/// a [`ToolDispatchContext`]: a final realizes its intents inside the
/// admitted execution that runs its call, and stages what they write to the
/// lash store to commit with the call's outcome (ADR 0132 §5).
pub struct IntentRealizationContext<'run> {
    pub effect_controller: crate::ActorContext,
    pub owner: crate::ExecutionOwner,
    pub processes: Arc<dyn crate::ProcessService>,
    pub process_engines: crate::ProcessEngineRegistry,
    pub parent_invocation: Option<crate::RuntimeInvocation>,
    pub process_lineage: Option<crate::ProcessLineage>,
    pub process_originator: Option<crate::ProcessOriginator>,
    /// The run this context serves; the context itself is `'static`.
    pub run: std::marker::PhantomData<&'run ()>,
}

impl<'run> ToolDispatchContext<'run> {
    /// The admitted owner and services an intent batch executes under.
    pub fn intent_realization_context(&self) -> IntentRealizationContext<'run> {
        let context = self;
        IntentRealizationContext {
            effect_controller: context.effect_controller.clone(),
            owner: context.owner.clone(),
            processes: Arc::clone(&context.processes),
            process_engines: context.process_engines.clone(),
            parent_invocation: context.parent_invocation.clone(),
            process_lineage: context.process_lineage.clone(),
            process_originator: context.process_originator.clone(),
            run: std::marker::PhantomData,
        }
    }
}

impl IntentRealizationContext<'_> {
    /// Attribution available without a causal parent comes only from the
    /// admitted execution scope, exactly as
    /// [`ToolDispatchContext::parentless_attribution`] resolves it.
    pub(crate) fn parentless_attribution(&self) -> crate::RuntimeAttribution {
        self.effect_controller
            .execution_scope()
            .session_id()
            .map(crate::RuntimeAttribution::for_session)
            .unwrap_or_else(crate::RuntimeAttribution::none)
    }
}

/// Realize a final's `intents`: each intent's outcome, and the store-local
/// effects a process start stages, which commit with the call's outcome
/// under its owner's epoch fence (ADR 0132 §5). Nothing here writes a
/// process row.
///
/// # Errors
///
/// A replay divergence, which the call cannot settle.
pub async fn execute_final_tool_intents(
    context: &IntentRealizationContext<'_>,
    tool_call_id: &lash_sansio::ToolCallId,
    intents: &crate::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<super::Realization, crate::RuntimeEffectControllerError> {
    let execution_scope_id = context.effect_controller.scope_id().to_string();
    if intents.intents.is_empty() && intents.protocol_version == crate::TOOL_INTENT_PROTOCOL_V3 {
        return Ok(super::Realization::default());
    }
    if let Some(refusal) = admit_batch(&context.owner.runtime_owner(), intents) {
        return Ok(super::Realization {
            receipt: super::RealizationReceipt {
                outcomes: refuse_all(context, &execution_scope_id, tool_call_id, intents, refusal),
            },
            store_local: Vec::new(),
        });
    }

    // Once a semantic admission is cancelled, remaining intents cannot mint work.
    let mut minting_cancelled = false;
    let mut outcomes = Vec::with_capacity(intents.intents.len());
    let mut store_local = Vec::new();
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
                crate::ToolIntentRefusalReason::MintingRunCancelled,
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
        let result = execute_one(context, intent, &identity, child_trace_hook)
            .instrument(span.clone())
            .await;
        let _entered = span.enter();
        let outcome = match result {
            Ok((realized, effect)) => {
                store_local.extend(effect);
                crate::ToolIntentExecutionOutcome::Executed { identity, realized }
            }
            Err(crate::PluginError::RuntimeEffectController(error))
                if error.code.is_replay_mismatch() =>
            {
                return Err(error);
            }
            Err(crate::PluginError::RuntimeEffectController(error))
                if error.code == crate::RuntimeErrorCode::RuntimeToolRunCancelDecided =>
            {
                minting_cancelled = true;
                refused(
                    index,
                    intent.kind(),
                    Some(identity),
                    crate::ToolIntentRefusalReason::MintingRunCancelled,
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
    Ok(super::Realization {
        receipt: super::RealizationReceipt { outcomes },
        store_local,
    })
}

/// The declared-start launch entry (ADR 0116 §3.2): stages the one start a
/// pending call declared exactly as a `StartProcess` intent stages it — the
/// same request under the same derived key, the same admission and the same
/// child-trace hook — and answers the call's launch receipt with the rows
/// that register the start with the call's park.
///
/// `scope` carries the call's lineage as its parent. A staged start answers
/// `Executed`; a typed refusal answers `Refused` and settles the call. An
/// `Err` is a fault the call cannot settle (see [`declared_start_fault`]).
pub(crate) async fn realize_declared_start(
    processes: &dyn crate::ProcessService,
    start: &crate::DeclaredStart,
    scope: crate::ProcessOpScope<'_>,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<
    (
        crate::ToolIntentExecutionOutcome,
        Option<crate::runtime::actor::round::StoreLocalEffect>,
    ),
    crate::RuntimeEffectControllerError,
> {
    let identity = start.identity().clone();
    let parent = scope.parent_invocation.clone().map(|parent| {
        parent.with_replay_attribution(crate::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        ))
    });
    let scope = scope.with_parent_invocation(parent);
    let kind = crate::ToolIntentKind::StartProcess;
    let request = start
        .request()
        .with_trace_cause(scope.effect_controller.trace_scope().map_or(
            lash_trace::TraceCause::Root,
            lash_trace::DurableTraceScope::parent_cause,
        ));
    match processes
        .stage_recorded_start(&start.start().owner, request, scope)
        .await
    {
        Ok(staged) => {
            let handle = crate::ProcessHandleView::from_record(staged.record);
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
            Ok((
                crate::ToolIntentExecutionOutcome::Executed {
                    identity,
                    realized: crate::ToolIntentRealized::StartProcess(handle),
                },
                staged
                    .rows
                    .map(crate::runtime::actor::round::StoreLocalEffect::ProcessStart),
            ))
        }
        Err(error) => match declared_start_fault(&error) {
            Some(fault) => Err(fault),
            None => Ok((
                refused(
                    identity.intent_index as usize,
                    kind,
                    Some(identity),
                    crate::ToolIntentRefusalReason::CommandFailed {
                        cause: crate::ToolIntentCommandFailure::from(&error),
                    },
                ),
                None,
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
                || error.code == crate::RuntimeErrorCode::RuntimeToolRunCancelDecided
                || error.turn_failure_cause() != crate::TurnFailureCause::Outcome =>
        {
            Some(error.clone())
        }
        crate::PluginError::RuntimeEffectController(_) => None,
        // A store that did not answer, a lost lease, a superseded execution:
        // the attempt's, under the code the controller carries it by.
        error => match error.class() {
            crate::PluginErrorClass::Retryable | crate::PluginErrorClass::Redrivable => {
                Some(crate::RuntimeEffectControllerError::from(error.clone()))
            }
            crate::PluginErrorClass::Terminal => None,
        },
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
    context: &IntentRealizationContext<'_>,
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
            span.in_scope(|| refused(index, intent.kind(), identity, refusal.clone()))
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
    context: &IntentRealizationContext<'_>,
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

#[expect(
    clippy::expect_used,
    reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
)]
/// Realize one intent: its outcome, and the store-local effect a process
/// start stages instead of writing.
async fn execute_one(
    context: &IntentRealizationContext<'_>,
    intent: &crate::ToolIntent,
    identity: &crate::ToolIntentIdentity,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<
    (
        crate::ToolIntentRealized,
        Option<crate::runtime::actor::round::StoreLocalEffect>,
    ),
    crate::PluginError,
> {
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
            let request = intent.into_request(identity).with_trace_cause(
                context.effect_controller.trace_scope().map_or(
                    lash_trace::TraceCause::Root,
                    lash_trace::DurableTraceScope::parent_cause,
                ),
            );
            let staged = context
                .processes
                .stage_recorded_start(&intent.owner, request, scope)
                .await?;
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: staged.record.id.clone(),
                    attempt: None,
                    child_entry_name: None,
                });
            }
            Ok((
                crate::ToolIntentRealized::StartProcess(crate::ProcessHandleView::from_record(
                    staged.record,
                )),
                staged
                    .rows
                    .map(crate::runtime::actor::round::StoreLocalEffect::ProcessStart),
            ))
        }
        crate::ToolIntent::CancelProcess(intent) => {
            let record = context
                .processes
                .cancel_recorded_intent(&intent.owner, &intent.process_id, identity.clone(), scope)
                .await?;
            Ok((
                crate::ToolIntentRealized::CancelProcess(crate::ProcessCancelReceipt::from_record(
                    record,
                )?),
                None,
            ))
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
        .map(|definition| {
            (
                crate::ToolIntentRealized::PublishDefinition(Box::new(definition)),
                None,
            )
        }),
        crate::ToolIntent::GetDefinition(intent) => realize_definition(
            context,
            identity,
            crate::ProcessCommand::GetDefinition {
                definition_id: intent.definition_id.clone(),
            },
        )
        .await
        .map(|definition| {
            (
                crate::ToolIntentRealized::GetDefinition(Box::new(definition)),
                None,
            )
        }),
    }
}

#[expect(
    clippy::expect_used,
    reason = "the controller carries an admitted scope"
)]
async fn realize_definition(
    context: &IntentRealizationContext<'_>,
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
        .process_effect(
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

#[cfg(test)]
mod tests {

    use super::*;

    fn session(id: &str) -> crate::RuntimeOwner {
        crate::RuntimeOwner::Session(crate::SessionId::fixture(id))
    }

    fn cancel(owner: &crate::RuntimeOwner, index: usize) -> crate::ToolIntent {
        crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
            owner: owner.clone(),
            process_id: crate::process_id_for_test(&format!("process-{index}")),
        })
    }

    #[test]
    fn protocol_dispatch_refuses_predecessor_and_unknown_records() {
        for recorded in [0, 1, 2, 4] {
            let intents = crate::ToolIntents {
                protocol_version: recorded,
                intents: vec![cancel(&session("session"), 1)],
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
                .map(|index| cancel(&session("session"), index))
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
                .map(|index| cancel(&session("session"), index))
                .collect(),
        );
        assert_eq!(
            admit_batch(&session("session"), &intents),
            Some(crate::ToolIntentRefusalReason::PerKindBudgetExceeded {
                kind: crate::ToolIntentKind::CancelProcess,
                actual: 17,
                maximum: 16,
            })
        );
    }

    /// A start carrying `payload` as its declared input under an execution
    /// env captured from a session whose recorded protocol prompt is `prompt`.
    fn start_under_prompt(payload: serde_json::Value, prompt: String) -> crate::ToolIntent {
        let policy = crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        );
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
            cancel(&session("session"), 0),
            cancel(&session("other-session"), 1),
        ]);
        assert_eq!(
            admit_batch(&session("session"), &intents),
            Some(crate::ToolIntentRefusalReason::OwnerMismatch {
                expected: session("session"),
                recorded: session("other-session"),
            })
        );
    }
}
