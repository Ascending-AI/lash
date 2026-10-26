use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::Instrument as _;

use super::ToolDispatchContext;

/// The dispatch fields intent realization reads, carried without the rest of
/// a [`ToolDispatchContext`]: a final realizes its intents inside the
/// admitted execution that runs its call, and stages what they write to the
/// lash store to commit with the call's outcome (ADR 0132 §5).
pub struct IntentRealizationContext<'run> {
    pub limits: lash_execution_contracts::tool_intent::ToolIntentLimits,
    pub effect_controller: crate::ActorContext,
    pub owner: crate::ExecutionOwner,
    pub processes: Arc<dyn crate::ProcessService>,
    pub process_engines: crate::ProcessEngineRegistry,
    pub parent_invocation: Option<crate::RuntimeInvocation>,
    pub process_lineage: Option<lash_execution_contracts::process::ProcessLineage>,
    pub process_originator: Option<lash_execution_contracts::process::ProcessOriginator>,
    /// The run this context serves; the context itself is `'static`.
    pub run: std::marker::PhantomData<&'run ()>,
}

impl<'run> ToolDispatchContext<'run> {
    /// The admitted owner and services an intent batch executes under.
    pub fn intent_realization_context(&self) -> IntentRealizationContext<'run> {
        let context = self;
        IntentRealizationContext {
            limits: context.state.data_limits().tool_intents,
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
    tool_call_id: &lash_protocol_types::ToolCallId,
    intents: &lash_execution_contracts::tool_intent::ToolIntents,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<super::Realization, crate::RuntimeEffectControllerError> {
    let execution_scope_id = context.effect_controller.scope_id().to_string();
    if intents.intents.is_empty()
        && intents.protocol_version
            == lash_execution_contracts::tool_intent::TOOL_INTENT_PROTOCOL_V3
    {
        return Ok(super::Realization::default());
    }
    if let Some(refusal) = admit_batch(&context.owner.runtime_owner(), intents, context.limits) {
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
                lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::MintingRunCancelled,
            ));
            continue;
        }
        let span = tracing::debug_span!(
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
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed { identity, realized }
            }
            Err(lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(error))
                if error.code.is_replay_mismatch() =>
            {
                return Err(error);
            }
            Err(lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(error))
                if error.code == crate::RuntimeErrorCode::RuntimeToolRunCancelDecided =>
            {
                minting_cancelled = true;
                refused(
                    index,
                    intent.kind(),
                    Some(identity),
                    lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::MintingRunCancelled,
                )
            }
            Err(error) => refused(
                index,
                intent.kind(),
                Some(identity),
                lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::CommandFailed {
                    cause: lash_execution_contracts::plugin_error::ToolIntentCommandFailure::from(&error),
                },
            ),
        };
        tracing::debug!(
            target: "lash::tool_intent",
            outcome = match &outcome {
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed { .. } => "executed",
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Refused { .. } => "refused",
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::ProtocolRefused { .. } => "protocol_refused"
            },
            "tool intent outcome"
        );
        outcomes.push(outcome);
    }
    let executed_count = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed { .. }))
        .count();
    tracing::info!(
        target: "lash::tool_intent",
        %tool_call_id,
        intent_count = outcomes.len(),
        executed_count,
        refused_count = outcomes.len() - executed_count,
        "tool intent batch completed"
    );
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
    start: &lash_execution_contracts::DeclaredStart,
    scope: crate::ProcessOpScope<'_>,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<
    (
        lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome,
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
    let kind = lash_vocabulary::ToolIntentKind::StartProcess;
    let request = lash_execution_contracts::declared_start::declared_start_request(start)
        .with_trace_parent(
            scope
                .effect_controller
                .trace_scope()
                .and_then(lash_trace::DurableTraceScope::as_parent),
        );
    match processes
        .stage_recorded_start(&start.start().owner, request, scope)
        .await
    {
        Ok(staged) => {
            let handle = lash_execution_contracts::process::ProcessHandleView::from_record(staged.record);
            // The declared kind names the child's entry, so a trace links the
            // call to a child it can name.
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: handle.process_id.clone(),
                    child_entry_name: start
                        .start()
                        .declaration
                        .identity
                        .as_ref()
                        .map(|identity| identity.kind.as_str().to_string()),
                });
            }
            Ok((
                lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed {
                    identity,
                    realized: lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::StartProcess(handle),
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
                    lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::CommandFailed {
                        cause: lash_execution_contracts::plugin_error::ToolIntentCommandFailure::from(&error),
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
    error: &lash_execution_contracts::plugin_error::PluginError,
) -> Option<crate::RuntimeEffectControllerError> {
    match error {
        lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(error)
            if error.code.is_replay_mismatch()
                || error.code == crate::RuntimeErrorCode::RuntimeToolRunCancelDecided
                || error.turn_failure_cause() != crate::TurnFailureCause::Outcome =>
        {
            Some(error.clone())
        }
        lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(_) => None,
        // A store that did not answer, a lost lease, a superseded execution:
        // the attempt's, under the code the controller carries it by.
        error => match error.class() {
            lash_execution_contracts::plugin_error::PluginErrorClass::Retryable
            | lash_execution_contracts::plugin_error::PluginErrorClass::Redrivable => {
                Some(crate::RuntimeEffectControllerError::from(error.clone()))
            }
            lash_execution_contracts::plugin_error::PluginErrorClass::Terminal => None,
        },
    }
}

pub(super) fn admit_batch(
    owner: &lash_vocabulary::identity::RuntimeOwner,
    intents: &lash_execution_contracts::tool_intent::ToolIntents,
    limits: lash_execution_contracts::tool_intent::ToolIntentLimits,
) -> Option<lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason> {
    if intents.protocol_version != lash_execution_contracts::tool_intent::TOOL_INTENT_PROTOCOL_V3 {
        return Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::UnsupportedProtocolVersion {
            recorded: intents.protocol_version,
        });
    }
    if intents.intents.len() > limits.max_count() {
        return Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::CountBudgetExceeded {
            actual: intents.intents.len(),
            maximum: limits.max_count(),
        });
    }
    let canonical_bytes = intents.declared_canonical_bytes().unwrap_or(usize::MAX);
    if canonical_bytes > limits.max_canonical_bytes() {
        return Some(
            lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                actual: canonical_bytes,
                maximum: limits.max_canonical_bytes(),
            },
        );
    }
    let mut by_kind = BTreeMap::<&'static str, (lash_vocabulary::ToolIntentKind, usize)>::new();
    for intent in &intents.intents {
        let entry = by_kind
            .entry(intent.kind().as_str())
            .or_insert((intent.kind(), 0));
        entry.1 += 1;
        if entry.1 > limits.max_per_kind() {
            return Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::PerKindBudgetExceeded {
                kind: entry.0,
                actual: entry.1,
                maximum: limits.max_per_kind(),
            });
        }
        if intent.owner() != owner {
            return Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::OwnerMismatch {
                expected: owner.clone(),
                recorded: intent.owner().clone(),
            });
        }
    }
    None
}

fn refuse_all(
    context: &IntentRealizationContext<'_>,
    execution_scope_id: &str,
    tool_call_id: &lash_protocol_types::ToolCallId,
    intents: &lash_execution_contracts::tool_intent::ToolIntents,
    refusal: lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason,
) -> Vec<lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome> {
    if intents.intents.is_empty() {
        let span = tracing::debug_span!(
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
        return vec![lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::ProtocolRefused { refusal }];
    }
    intents
        .intents
        .iter()
        .enumerate()
        .map(|(index, intent)| {
            let identity =
                derive_identity(context, execution_scope_id, tool_call_id, index).ok();
            let span = tracing::debug_span!(
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
    tool_call_id: &lash_protocol_types::ToolCallId,
    minting_emission: &crate::RuntimeInvocation,
) -> crate::ToolIntentIdentity {
    lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
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
    tool_call_id: &lash_protocol_types::ToolCallId,
    intent_index: usize,
) -> Result<
    crate::ToolIntentIdentity,
    lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason,
> {
    let intent_index = u32::try_from(intent_index).map_err(|_| {
        lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::IntentIndexOverflow
    })?;
    Ok(
        lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
            &context.owner.runtime_owner(),
            execution_scope_id,
            tool_call_id,
            intent_index,
            context.parent_invocation.as_ref(),
        ),
    )
}

fn refused(
    index: usize,
    kind: lash_vocabulary::ToolIntentKind,
    identity: Option<crate::ToolIntentIdentity>,
    refusal: lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason,
) -> lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome {
    tracing::warn!(
        target: "lash::tool_intent",
        intent_kind = kind.as_str(),
        refusal_reason = %refusal.code(),
        intent_index = index,
        "tool intent refused"
    );
    lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Refused {
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
    intent: &lash_execution_contracts::tool_intent::ToolIntent,
    identity: &crate::ToolIntentIdentity,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<
    (
        lash_execution_contracts::tool_intent_outcome::ToolIntentRealized,
        Option<crate::runtime::actor::round::StoreLocalEffect>,
    ),
    lash_execution_contracts::plugin_error::PluginError,
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
        lash_execution_contracts::tool_intent::ToolIntent::StartProcess(intent) => {
            // The declaration carries no id: the sole constructor on this path
            // derives it from this declaration's identity, so a redrive of the
            // same attempt starts the same process id (FIG-2994).
            let request = intent.into_request(identity).with_trace_parent(
                context
                    .effect_controller
                    .trace_scope()
                    .and_then(lash_trace::DurableTraceScope::as_parent),
            );
            let staged = context
                .processes
                .stage_recorded_start(&intent.owner, request, scope)
                .await?;
            if let Some(hook) = child_trace_hook {
                hook.child_process_started(crate::tool_provider::ToolChildProcessStarted {
                    process_id: staged.record.id.clone(),
                    child_entry_name: None,
                });
            }
            Ok((
                lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::StartProcess(lash_execution_contracts::process::ProcessHandleView::from_record(
                    staged.record,
                )),
                staged
                    .rows
                    .map(crate::runtime::actor::round::StoreLocalEffect::ProcessStart),
            ))
        }
        lash_execution_contracts::tool_intent::ToolIntent::CancelProcess(intent) => {
            let record = context
                .processes
                .cancel_recorded_intent(&intent.owner, &intent.process_id, identity.clone(), scope)
                .await?;
            Ok((
                lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::CancelProcess(
                    lash_execution_contracts::process::ProcessCancelReceipt::from_observed(record)?,
                ),
                None,
            ))
        }
        lash_execution_contracts::tool_intent::ToolIntent::PublishDefinition(intent) => realize_definition(
            context,
            identity,
            crate::ProcessCommand::PublishDefinition {
                draft: intent.draft.clone(),
            },
        )
        .await
        .map(|definition| {
            (
                lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::PublishDefinition(Box::new(definition)),
                None,
            )
        }),
        lash_execution_contracts::tool_intent::ToolIntent::GetDefinition(intent) => realize_definition(
            context,
            identity,
            crate::ProcessCommand::GetDefinition {
                definition_id: intent.definition_id.clone(),
            },
        )
        .await
        .map(|definition| {
            (
                lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::GetDefinition(Box::new(definition)),
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
) -> Result<
    lash_execution_contracts::process::ProcessDefinition,
    lash_execution_contracts::plugin_error::PluginError,
> {
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
        _ => Err(
            lash_execution_contracts::plugin_error::PluginError::Session(
                "definition effect returned a different outcome".into(),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn session(id: &str) -> lash_vocabulary::identity::RuntimeOwner {
        lash_vocabulary::identity::RuntimeOwner::Session(
            lash_kernel_value::identity::SessionId::fixture(id),
        )
    }

    fn cancel(
        owner: &lash_vocabulary::identity::RuntimeOwner,
        index: usize,
    ) -> lash_execution_contracts::tool_intent::ToolIntent {
        lash_execution_contracts::tool_intent::ToolIntent::CancelProcess(
            lash_execution_contracts::tool_intent::CancelProcessIntent {
                owner: owner.clone(),
                process_id: crate::process_id_for_test(&format!("process-{index}")),
            },
        )
    }

    /// INTENT-ADMISSION-POLICY: the deployment's count, kind and byte budgets decide admission.
    #[test]
    fn a_nondefault_intent_policy_admits_and_refuses_its_own_bounds() {
        let owner = session("configured-intents");
        let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
            cancel(&owner, 0),
            cancel(&owner, 1),
        ]);
        let limits =
            lash_execution_contracts::tool_intent::ToolIntentLimits::new(2, usize::MAX, 2).unwrap();
        assert_eq!(admit_batch(&owner, &intents, limits), None);
        for limits in [
            lash_execution_contracts::tool_intent::ToolIntentLimits::new(1, usize::MAX, 2).unwrap(),
            lash_execution_contracts::tool_intent::ToolIntentLimits::new(2, 1, 2).unwrap(),
            lash_execution_contracts::tool_intent::ToolIntentLimits::new(2, usize::MAX, 1).unwrap(),
        ] {
            assert!(admit_batch(&owner, &intents, limits).is_some());
        }
    }

    #[test]
    fn admission_is_all_or_nothing_for_total_count_overflow() {
        let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(
            (0..=lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_COUNT)
                .map(|index| cancel(&session("session"), index))
                .collect(),
        );
        assert_eq!(
            admit_batch(
                &session("session"),
                &intents,
                lash_execution_contracts::tool_intent::ToolIntentLimits::standard()
            ),
            Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::CountBudgetExceeded {
                actual: 33,
                maximum: 32,
            })
        );
    }

    #[test]
    fn admission_is_all_or_nothing_for_per_kind_overflow() {
        let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(
            (0..=lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_PER_KIND)
                .map(|index| cancel(&session("session"), index))
                .collect(),
        );
        assert_eq!(
            admit_batch(
                &session("session"),
                &intents,
                lash_execution_contracts::tool_intent::ToolIntentLimits::standard()
            ),
            Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::PerKindBudgetExceeded {
                kind: lash_vocabulary::ToolIntentKind::CancelProcess,
                actual: 17,
                maximum: 16,
            })
        );
    }

    /// A start carrying `payload` as its declared input under an execution
    /// env captured from a session whose recorded protocol prompt is `prompt`.
    fn start_under_prompt(
        payload: serde_json::Value,
        prompt: String,
    ) -> lash_execution_contracts::tool_intent::ToolIntent {
        let env = crate::testing::process_env_of_size(prompt.len());
        lash_execution_contracts::tool_intent::ToolIntent::StartProcess(Box::new(
            lash_execution_contracts::tool_intent::StartProcessIntent {
                owner: session("session"),
                declaration: lash_execution_contracts::process::ProcessStartDeclaration::engine(
                    lash_protocol_types::LASH_VM_ENGINE_KIND,
                    payload,
                    lash_execution_contracts::process::model::launch::held_launch(
                        lash_execution_contracts::process::EngineLaunch::new(
                            env.stable_ref().expect("environment digest"),
                            None,
                        ),
                    ),
                    lash_execution_contracts::process::ProcessOriginator::host(),
                    lash_execution_contracts::process::Lifetime::Detached,
                ),
            },
        ))
    }

    #[test]
    fn a_128_kib_environment_keeps_durable_start_rows_under_the_intent_budget() {
        let intent = start_under_prompt(
            serde_json::json!({"key": "child/0"}),
            "x".repeat(128 * 1024),
        );
        let batch = lash_execution_contracts::tool_intent::ToolIntents::v3(vec![intent.clone()]);
        let identity = lash_execution_contracts::tool_intent::derive_tool_intent_identity(
            &session("session"),
            "scope",
            &crate::ToolCallId::fixture("start"),
            0,
        );
        let submission = lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord::new(
            identity.clone(),
            intent.clone(),
        )
        .expect("submission");
        let lash_execution_contracts::tool_intent::ToolIntent::StartProcess(start) = intent else {
            panic!("start");
        };
        let request = start.into_request(&identity);
        let record = lash_execution_contracts::process::ProcessRecord::from_registration(
            request
                .clone()
                .into_registration()
                .stating_input()
                .expect("the start states its input"),
            lash_vocabulary::identity::ProcessId::fixture("large-environment-child"),
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
                bytes.len()
                    < lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_CANONICAL_BYTES,
                "{name} stores {} bytes for a 128 KiB environment",
                bytes.len()
            );
        }
        assert_eq!(
            batch.declared_canonical_bytes().expect("budget"),
            serde_json::to_vec(&batch).expect("batch").len()
        );
    }

    /// The same bytes in what the start declares are the attempt's, and the
    /// budget still refuses them.
    #[test]
    fn admission_still_bounds_what_a_start_declares() {
        let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
            start_under_prompt(
                serde_json::json!({"key": "x".repeat(lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_CANONICAL_BYTES)}),
                String::new(),
            ),
        ]);
        assert!(matches!(
            admit_batch(
                &session("session"),
                &intents,
                lash_execution_contracts::tool_intent::ToolIntentLimits::standard()
            ),
            Some(
                lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::CanonicalByteBudgetExceeded {
                    maximum: lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_CANONICAL_BYTES,
                    ..
                }
            )
        ));
    }

    #[test]
    fn admission_refuses_the_entire_batch_on_owner_mismatch() {
        let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
            cancel(&session("session"), 0),
            cancel(&session("other-session"), 1),
        ]);
        assert_eq!(
            admit_batch(
                &session("session"),
                &intents,
                lash_execution_contracts::tool_intent::ToolIntentLimits::standard()
            ),
            Some(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::OwnerMismatch {
                expected: session("session"),
                recorded: session("other-session"),
            })
        );
    }
}
