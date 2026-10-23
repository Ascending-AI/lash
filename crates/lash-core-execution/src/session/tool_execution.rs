use super::execution_context::RuntimeExecutionContext;
use crate::tool_dispatch::{
    ToolCallIds, ToolDispatchOutcome, ToolPreparationOutcome, prepare_tool_call_with_context,
};
use crate::{
    SessionStreamEvent, ToolCallOutput, ToolCallRecord, ToolCancellation, ToolFailure,
    ToolFailureClass, TurnActivityId, TurnEvent,
};

#[derive(Clone)]
pub struct ToolInvocation {
    /// The call's lash-minted identity: its idempotency key and the root of
    /// every key its attempts, awaits and presentation journal under.
    pub id: crate::ToolCallId,
    pub tool_id: lash_vocabulary::tool_contract::ToolId,
    pub args: serde_json::Value,
    /// Original native request correlation and replay data, when supplied.
    pub pending: Option<Box<crate::sansio::PendingToolCall>>,
    pub execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    /// The contract the performing code requires of the call's operation;
    /// none for a model round's call, which is pinned to its recorded
    /// definition.
    pub contract: Option<lash_vocabulary::operations::ContractId>,
    pub child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    pub issuing_language_node_id: Option<String>,
}

impl ToolInvocation {
    pub fn new(
        id: crate::ToolCallId,
        tool_id: lash_vocabulary::tool_contract::ToolId,
        args: serde_json::Value,
    ) -> Self {
        Self {
            id,
            tool_id,
            args,
            pending: None,
            execution_grant: None,
            contract: None,
            child_execution_trace_hook: None,
            issuing_language_node_id: None,
        }
    }

    pub fn from_pending(
        pending: crate::sansio::PendingToolCall,
        tool_id: lash_vocabulary::tool_contract::ToolId,
    ) -> Self {
        let mut invocation = Self::new(pending.call_id.clone(), tool_id, pending.args.clone());
        invocation.pending = Some(Box::new(pending));
        invocation
    }

    pub fn with_child_execution_trace_hook(
        mut self,
        hook: crate::ToolChildExecutionTraceHook,
    ) -> Self {
        self.child_execution_trace_hook = Some(hook);
        self
    }

    pub fn with_execution_grant(mut self, grant: crate::ToolExecutionGrant) -> Self {
        self.execution_grant = Some(Box::new(grant));
        self
    }

    pub fn with_issuing_language_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.issuing_language_node_id = Some(node_id.into());
        self
    }
}

impl std::fmt::Debug for ToolInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInvocation")
            .field("id", &self.id)
            .field("tool_id", &self.tool_id)
            .field("args", &self.args)
            .field(
                "execution_grant",
                &self.execution_grant.as_ref().map(|_| "<grant>"),
            )
            .field("contract", &self.contract)
            .field(
                "child_execution_trace_hook",
                &self.child_execution_trace_hook.as_ref().map(|_| "<hook>"),
            )
            .field("issuing_language_node_id", &self.issuing_language_node_id)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ToolInvocationReply {
    pub output: ToolCallOutput,
    pub record: Option<ToolCallRecord>,
    pub completed: Option<Box<crate::sansio::CompletedToolCall>>,
}

impl ToolInvocationReply {
    pub fn success(value: serde_json::Value) -> Self {
        Self {
            output: ToolCallOutput::success(value),
            record: None,
            completed: None,
        }
    }

    pub fn error(value: serde_json::Value) -> Self {
        let message = value
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| value.to_string());
        let mut failure = ToolFailure::tool(ToolFailureClass::Execution, "tool_error", message);
        failure.raw = Some(crate::ToolValue::untrusted_json(value));
        Self {
            output: ToolCallOutput::failure(failure),
            record: None,
            completed: None,
        }
    }

    pub fn from_output(output: ToolCallOutput) -> Self {
        Self {
            output,
            record: None,
            completed: None,
        }
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::from_output(ToolCallOutput::cancelled(ToolCancellation::runtime(
            message,
        )))
    }

    pub(crate) fn with_record(mut self, record: ToolCallRecord) -> Self {
        self.record = Some(record);
        self
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompletedProtocolToolCall {
    pub completed: crate::sansio::CompletedToolCall,
    #[serde(serialize_with = "lash_protocol_types::durable_serde::serialize")]
    pub record: ToolCallRecord,
}

pub(crate) fn tool_activity_id(call_id: &crate::ToolCallId) -> TurnActivityId {
    TurnActivityId::new(format!("tool:{call_id}"))
}

/// The tool failure a call refused by the session's `max_tool_calls` settles
/// with: typed by its code, never retried, and worded by the refusal so the
/// limit is named wherever the failure is shown (FIG-4546).
pub(crate) fn tool_call_limit_failure(exceeded: crate::ToolCallLimitExceeded) -> ToolFailure {
    ToolFailure::runtime(
        ToolFailureClass::ResourceLimit,
        crate::ToolCallLimitExceeded::CODE,
        exceeded.to_string(),
    )
}

impl RuntimeExecutionContext<'_> {
    /// The one provider identity a call of `tool` binds to and dispatches
    /// through: the provider `granted` names, else its surface entry's.
    /// The binding formats the provider registered under `codec` reads on
    /// this node; `None` when this node registers no provider under it.
    pub fn binding_formats(
        &self,
        codec: &lash_vocabulary::providers::ToolProviderHandle,
    ) -> Option<&'static [lash_vocabulary::providers::FormatVersion]> {
        self.dispatch
            .routes
            .registry()
            .provider(codec)
            .map(|provider| provider.readable_binding_formats())
    }

    pub fn tool_provider(
        &self,
        tool: &lash_vocabulary::tool_contract::ToolId,
        granted: Option<&lash_vocabulary::providers::ToolProviderHandle>,
    ) -> Result<
        lash_vocabulary::providers::ToolProviderHandle,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.dispatch
            .composition
            .tool_provider(tool, self.dispatch.routed_provider(tool, granted))
    }

    /// `call_key` is the material the call's observation lanes key under —
    /// the call's own effect-invocation replay key where it has one (a group
    /// child's `{group}:child:{position}`, a command's key), else the
    /// caller's positional material (`{iteration}:{index}:{call_id}` on the
    /// protocol path, `{batch_id}:{index}:{call_id}` in a batch) — qualified
    /// against this context's observation base (ADR 0105 §1).
    pub(crate) fn emit_tool_call_started(
        &self,
        call_key: &str,
        ids: &ToolCallIds,
        name: &str,
        args: serde_json::Value,
        activity_id: TurnActivityId,
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{}:start", ids.call_id));
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Session(SessionStreamEvent::ToolCallStart {
                call_id: ids.call_id.clone(),
                provider_call_id: ids.provider_call_id.clone(),
                name: name.to_string(),
                args: args.clone(),
            }),
        );
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(activity_id),
                event: TurnEvent::ToolCallStarted {
                    call_id: ids.call_id.clone(),
                    provider_call_id: ids.provider_call_id.clone(),
                    name: name.to_string(),
                    args,
                    graph_key: self.code_block_graph_key(),
                },
            },
        );
    }

    /// `call_key` is the material the call's observation lanes key under —
    /// `{iteration}:{index}:{call_id}` on the turn-dispatched protocol path —
    /// qualified against this context's observation base (ADR 0105 §1).
    pub async fn prepare_tool_call(
        &self,
        pending: crate::sansio::PendingToolCall,
        call_key: &str,
    ) -> ToolPreparationOutcome {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let preparation = prepare_tool_call_with_context(context.dispatch.as_ref(), pending).await;
        if let ToolPreparationOutcome::Completed(outcome) = &preparation
            && let Err(error) =
                context.trace_tool_call_started((&outcome.record).into(), requested_at_ms)
        {
            context.record_nested_effect_error(error);
        }
        preparation
    }

    /// Prepares a call on a tool of the turn's recorded surface whose live
    /// definition drifted (FIG-3672 P7b): under `binding`, the recorded
    /// definition, with identity preparation, so the call's envelope is the
    /// one the journal recorded and the live tool is not consulted.
    pub async fn prepare_recorded_tool_call(
        &self,
        binding: &lash_vocabulary::tool_contract::ToolDefinition,
        pending: crate::sansio::PendingToolCall,
        call_key: &str,
    ) -> ToolPreparationOutcome {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let preparation = crate::tool_dispatch::prepare_recorded_tool_call_with_context(
            context.dispatch.as_ref(),
            binding,
            pending,
        )
        .await;
        if let ToolPreparationOutcome::Completed(outcome) = &preparation
            && let Err(error) =
                context.trace_tool_call_started((&outcome.record).into(), requested_at_ms)
        {
            context.record_nested_effect_error(error);
        }
        preparation
    }

    /// The catalog entry a model-issued call names, if the catalog holds it.
    pub fn callable_tool_id_by_name(
        &self,
        tool_name: &str,
    ) -> Option<lash_vocabulary::tool_contract::ToolId> {
        self.dispatch
            .tool_catalog
            .entries()
            .iter()
            .find(|tool| tool.manifest.tool() == tool_name)
            .map(|tool| tool.manifest.id.clone())
    }

    /// Presents a settled call through `PresentToolResult` and incorporates its
    /// settlement. The admitted call's outcome or the cell's snapshot owns
    /// durability (ADR 0132 §5, §8).
    ///
    /// # Errors
    /// Presentation failures stay typed and leave the call with no presentation
    /// to show; they never become the tool's model-facing result.
    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`].
    pub async fn complete_tool_call(
        &self,
        ids: ToolCallIds,
        tool_id: lash_vocabulary::tool_contract::ToolId,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        call_key: &str,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        Box::pin(async {
            let context = self.with_call_observation_key(self.call_observation_key(call_key));
            let this = &context;
            let call_id = &ids.call_id;
            let attempts = outcome.attempts.clone();
            let output = outcome.record.output.clone();
            let facts = crate::plugin::ToolPresentationFacts {
                intent_outcomes: outcome.intent_outcomes.clone(),
            };
            // Select the local plan and run its steps before the admitted
            // execution commits the result. Neither is an independent journal.
            let plan = self.dispatch.composition.tool_presentation_plan();
            let presentation_replay_key = format!("{call_id}:present");
            let scoped = self.dispatch.effect_controller.clone();
            let presentation = match crate::EffectAddress::new(
                scoped.execution_scope().clone(),
                presentation_replay_key.clone(),
            ) {
                Ok(address) => scoped
                    .tool_effect(
                        crate::RuntimeEffectEnvelope::new(
                            crate::RuntimeEffectInvocation::new(
                                address,
                                self.dispatch.parentless_attribution(),
                                presentation_replay_key,
                            ),
                            crate::RuntimeEffectCommand::PresentToolResult {
                                plan: Box::new(plan.clone()),
                                call_id: call_id.clone(),
                                tool_id,
                                tool_name: outcome.record.tool.clone(),
                                render: self.dispatch.facts.render.clone(),
                                args: outcome.record.args.clone(),
                                output: Box::new(outcome.record.output.clone()),
                            },
                        ),
                        crate::RuntimeEffectLocalExecutor::presentation(
                            std::sync::Arc::clone(&self.dispatch.composition),
                            std::sync::Arc::clone(&self.dispatch.state),
                            std::sync::Arc::new(facts),
                            std::sync::Arc::clone(&self.dispatch.attachment_store),
                            &plan,
                        ),
                    )
                    .await
                    .and_then(crate::RuntimeEffectOutcome::into_tool_presentation),
                Err(error) => Err(error.into()),
            }?;
            let model_return = presentation.model_return;
            let possession = outcome
                .intent_outcomes
                .iter()
                .filter_map(|outcome| match outcome {
                    lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome::Executed {
                        realized: lash_execution_contracts::tool_intent_outcome::ToolIntentRealized::StartProcess(handle),
                        ..
                    } => Some(handle.process_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            this.incorporate_tool_facts(
                crate::session::SettlementSource::Invocation {
                    call_id: call_id.clone(),
                },
                &possession,
            )?;
            self.emit_tool_intent_outcome_activities(call_key, call_id, &outcome.intent_outcomes);

            let record = ToolCallRecord {
                call_id: ids.call_id.clone(),
                provider_call_id: ids.provider_call_id.clone(),
                tool: outcome.record.tool.clone(),
                args: outcome.record.args.clone(),
                output: output.clone(),
            };
            this.emit_tool_call_completed(call_key, &record, &attempts, Some(duration_ms))
                .await;
            Ok(CompletedProtocolToolCall {
                completed: crate::sansio::CompletedToolCall {
                    control_origin: crate::sansio::ToolControlOrigin::OwnCall,
                    call_id: ids.call_id,
                    provider_call_id: ids.provider_call_id,
                    tool_name: outcome.record.tool,
                    args: outcome.record.args,
                    output,
                    model_return,
                    intent_outcomes: outcome.intent_outcomes,
                    replay,
                },
                record,
            })
        })
        .await
    }

    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`]. `duration_ms` is the measured
    /// wall-clock the caller observed for the call — an observation-only
    /// value: recorded content carries no durations, so it arrives on this
    /// path rather than on the record (FIG-3696).
    async fn emit_tool_call_completed(
        &self,
        call_key: &str,
        record: &ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
        duration_ms: Option<u64>,
    ) {
        self.trace_tool_call_completed(record, attempts, None);
        self.emit_tool_call_completed_activity(call_key, record, duration_ms);
    }

    /// Observes each of a call's intent outcomes, ahead of its completion.
    pub(crate) fn emit_tool_intent_outcome_activities(
        &self,
        call_key: &str,
        call_id: &crate::ToolCallId,
        intent_outcomes: &[lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome],
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{call_id}:intents"));
        for intent_outcome in intent_outcomes {
            cursor.observe(
                context.dispatch.observer.as_ref(),
                crate::engine::ObservedEvent::Activity {
                    correlation_id: Some(tool_activity_id(call_id)),
                    event: TurnEvent::ToolIntentOutcome {
                        call_id: call_id.clone(),
                        outcome: crate::ToolIntentActivityOutcome::from(intent_outcome),
                    },
                },
            );
        }
    }

    pub(crate) fn emit_tool_call_completed_activity(
        &self,
        call_key: &str,
        record: &ToolCallRecord,
        duration_ms: Option<u64>,
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{}:complete", record.call_id));
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(tool_activity_id(&record.call_id)),
                event: TurnEvent::ToolCallCompleted {
                    call_id: record.call_id.clone(),
                    provider_call_id: record.provider_call_id.clone(),
                    name: record.tool.clone(),
                    args: record.args.clone(),
                    output: record.output.clone(),
                    duration_ms,
                    graph_key: self.code_block_graph_key(),
                },
            },
        );
    }

    /// `call_key` is the material the call's observation lanes key under —
    /// `{iteration}:{index}:{call_id}` on the turn-dispatched protocol path;
    /// see [`Self::emit_tool_call_started`]. `duration_ms` is the caller's
    /// measured window for the call; it rides the observation only.
    pub async fn complete_undispatched_tool_call(
        &self,
        ids: ToolCallIds,
        tool_id: lash_vocabulary::tool_contract::ToolId,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        call_key: &str,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        if let Some(error) = self.peek_nested_effect_error() {
            return Err(error);
        }
        self.emit_tool_call_started(
            call_key,
            &ids,
            &outcome.record.tool,
            outcome.record.args.clone(),
            tool_activity_id(&ids.call_id),
        );
        self.complete_tool_call(ids, tool_id, replay, outcome, call_key, duration_ms)
            .await
    }

    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`].
    pub async fn report_undispatched_tool_call(
        &self,
        completed: &crate::sansio::CompletedToolCall,
        call_key: &str,
    ) {
        let record = ToolCallRecord {
            call_id: completed.call_id.clone(),
            provider_call_id: completed.provider_call_id.clone(),
            tool: completed.tool_name.clone(),
            args: completed.args.clone(),
            output: completed.output.clone(),
        };
        // The protocol supplied this original call identity and refused it
        // before dispatch.
        if let Err(error) =
            self.trace_tool_call_started((&record).into(), self.dispatch.clock.timestamp_ms())
        {
            self.record_nested_effect_error(error);
            return;
        }
        self.emit_tool_call_started(
            call_key,
            &ToolCallIds {
                call_id: completed.call_id.clone(),
                provider_call_id: completed.provider_call_id.clone(),
            },
            &completed.tool_name,
            completed.args.clone(),
            tool_activity_id(&completed.call_id),
        );
        // The protocol refused the call before dispatch; no measured window exists.
        self.emit_tool_call_completed(call_key, &record, &[], None)
            .await;
    }

    pub(crate) async fn await_process_with_cancellation(
        &self,
        process_id: &lash_vocabulary::identity::ProcessId,
        parent_invocation: Option<crate::RuntimeInvocation>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<
        lash_execution_contracts::process::ProcessAwaitOutput,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let _phase = self.named_phase("process.await_handle");
        let cancellation = cancellation.unwrap_or_default();
        let process_scope = self
            .process_scope(parent_invocation)
            .with_turn_cancellation(&self.turn_cancel_wait(cancellation));
        self.dispatch
            .processes
            .await_process_ref(process_id, process_scope)
            .await
    }

    /// Delivers cancellation to a deferred tool handle for code-executor implementors.
    pub async fn cancel_tool_handle(
        &self,
        call_id: crate::ToolCallId,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        self.cancel_process_handle(call_id, handle).await
    }

    /// Awaits a deferred tool handle for code-executor implementors without re-executing the call.
    pub async fn await_tool_handle(
        &self,
        call_id: crate::ToolCallId,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        self.await_process_handle(call_id, handle).await
    }
}
