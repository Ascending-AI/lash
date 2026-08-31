use lash_sansio::sync::{LockResultExt, MutexExt};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_core::{
    AttachmentRef, Observation, RuntimeExecutionContext, ToolExecutionGrant, TraceContext,
    TraceEvent, facade_support::ToolChildExecutionTraceHook, facade_support::ToolInvocation,
    facade_support::ToolInvocationReply, facade_support::TraceBranchSelection,
    facade_support::TraceRecord, facade_support::TraceRuntimeSubject, facade_support::TraceSink,
};
use lash_lashlang_runtime::{
    LASHLANG_ENGINE_KIND, LashlangProcessInput, TraceLanguageChildExecution,
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload,
    lashlang_process_event_types, lashlang_process_signal_event_types, lashlang_type_expr_schema,
    lashlang_value_to_json, prepare_lashlang_process_start, protocol_tool_output_to_lashlang_value,
    resolve_lashlang_module_operation, sleep_duration_ms,
};
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ProcessSignal, ProcessStart,
    ProjectedFuture, Record as FlowRecord, Sleep, Value as FlowValue, ValueProjectionContext,
    ValueProjector,
};
use serde_json::Value;

use crate::projection::{flow_to_json_value, format_output_value};

pub(super) struct HostBridge<'run> {
    ctx: RuntimeExecutionContext<'run>,
    language_id: &'static str,
    print_projector: std::sync::Arc<dyn ValueProjector>,
    tool_result_projectors: Vec<crate::RlmToolResultProjector>,
    observations: Mutex<Vec<Observation>>,
    printed_images: Mutex<Vec<AttachmentRef>>,
    tool_calls: Mutex<Vec<lash_core::ToolCallRecord>>,
    executed_calls: Mutex<Vec<(usize, lash_core::ExecutedCallRecord)>>,
    next_tool_index: Mutex<usize>,
    sleep_sequence: AtomicU64,
    lashlang_execution_trace: Option<LashlangExecutionTrace>,
    host_environment: lashlang::LashlangHostEnvironment,
    deferred_execution_grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    artifact_store: std::sync::Arc<dyn lashlang::LashlangArtifactStore>,
    trigger_key_manifest: lashlang::TriggerKeyManifest,
}

pub(super) struct HostBridgeConfig<'run> {
    pub ctx: RuntimeExecutionContext<'run>,
    pub language_id: &'static str,
    pub print_projector: std::sync::Arc<dyn ValueProjector>,
    pub tool_result_projectors: Vec<crate::RlmToolResultProjector>,
    pub lashlang_execution_trace: Option<LashlangExecutionTrace>,
    pub host_environment: lashlang::LashlangHostEnvironment,
    pub deferred_execution_grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    pub artifact_store: std::sync::Arc<dyn lashlang::LashlangArtifactStore>,
    pub trigger_key_manifest: lashlang::TriggerKeyManifest,
    pub initial_observations: Vec<Observation>,
}

type HostAbilityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AbilityResult, ExecutionHostError>> + Send + 'a>>;

impl<'run> HostBridge<'run> {
    pub(super) fn new(config: HostBridgeConfig<'run>) -> Self {
        Self {
            ctx: config.ctx,
            language_id: config.language_id,
            print_projector: config.print_projector,
            tool_result_projectors: config.tool_result_projectors,
            observations: Mutex::new(config.initial_observations),
            printed_images: Mutex::new(Vec::new()),
            tool_calls: Mutex::new(Vec::new()),
            executed_calls: Mutex::new(Vec::new()),
            next_tool_index: Mutex::new(0),
            sleep_sequence: AtomicU64::new(0),
            lashlang_execution_trace: config.lashlang_execution_trace,
            host_environment: config.host_environment,
            deferred_execution_grants: config.deferred_execution_grants,
            artifact_store: config.artifact_store,
            trigger_key_manifest: config.trigger_key_manifest,
        }
    }

    fn next_index(&self) -> usize {
        let mut guard = self.next_tool_index.lock_recover();
        let next = *guard;
        *guard += 1;
        next
    }

    fn consume_reply(
        &self,
        tool_name: &str,
        reply: ToolInvocationReply,
    ) -> Result<FlowValue, ExecutionHostError> {
        let projected_tool_name = reply
            .record
            .as_ref()
            .map(|record| record.tool.as_str())
            .unwrap_or(tool_name)
            .to_string();
        if let Some(record) = reply.record {
            self.tool_calls.lock_recover().push(record);
        }
        if reply.output.is_success() {
            let value = reply.output.value_for_projection();
            for projector in &self.tool_result_projectors {
                if let Some(value) = projector(&projected_tool_name, &value) {
                    return Ok(value);
                }
            }
            protocol_tool_output_to_lashlang_value(&reply.output)
        } else {
            protocol_tool_output_to_lashlang_value(&reply.output)
        }
    }

    fn record_executed_call(
        &self,
        index: usize,
        operation: String,
        outcome: lash_core::ExecutedCallOutcome,
    ) -> Result<(), ExecutionHostError> {
        // This ledger records dispatches only. Resolution, argument, and other
        // pre-dispatch failures deliberately produce no `Calls:` entry because
        // the source operation did not execute.
        self.executed_calls
            .lock_recover()
            .push((index, lash_core::ExecutedCallRecord { operation, outcome }));
        Ok(())
    }

    pub(super) fn into_collected(self) -> CollectedExecutionOutput {
        let mut executed_calls = self.executed_calls.into_inner().recover();
        executed_calls.sort_by_key(|(index, _)| *index);
        CollectedExecutionOutput {
            observations: self.observations.into_inner().recover(),
            printed_images: self.printed_images.into_inner().recover(),
            tool_calls: self.tool_calls.into_inner().recover(),
            executed_calls: executed_calls
                .into_iter()
                .map(|(_, record)| record)
                .collect(),
        }
    }

    pub(super) fn cancellation_observed(&self) -> bool {
        self.ctx.is_cancelled()
    }

    fn resource_tool_call_id(
        &self,
        host_operation: &str,
        call_site: Option<&lashlang::LashlangExecutionCallSite>,
        index: Option<usize>,
    ) -> String {
        if let Some(call_site) = call_site {
            let scope = self
                .ctx
                .parent_invocation()
                .and_then(|invocation| {
                    invocation.effect_id().map(|effect_id| {
                        if let Some(turn_id) = invocation.scope.turn_id.as_deref() {
                            format!(
                                "effect:{}:{turn_id}:{effect_id}",
                                invocation.scope.session_id
                            )
                        } else {
                            format!("effect:{}:{effect_id}", invocation.scope.session_id)
                        }
                    })
                })
                .unwrap_or_else(|| self.ctx.session_id().to_string());
            let mut call_id = format!(
                "{}:{scope}:resource:{host_operation}:{}:{}",
                self.language_id, call_site.site.node_id, call_site.occurrence
            );
            if let Some(index) = index {
                call_id.push_str(&format!(":child:{index}"));
            }
            return call_id;
        }
        let index = index.unwrap_or(0);
        format!(
            "{}:{}:resource:{host_operation}:index:{index}",
            self.language_id,
            self.ctx.session_id()
        )
    }

    fn deferred_grant_for_tool_id(
        &self,
        tool_id: &lash_core::ToolId,
    ) -> Option<ToolExecutionGrant> {
        self.deferred_execution_grants.get(tool_id).cloned()
    }
}

#[derive(Clone)]
pub(super) struct LashlangExecutionTrace {
    sink: std::sync::Arc<dyn TraceSink>,
    /// The dialect of the *source* that ran. The substrate is the Lashlang VM
    /// under both, which is why the event and the file keep their names.
    language: &'static str,
    base_context: TraceContext,
    identity: TraceLanguageExecutionIdentity,
}

impl LashlangExecutionTrace {
    pub(super) fn new(
        sink: std::sync::Arc<dyn TraceSink>,
        language: &'static str,
        base_context: TraceContext,
        identity: TraceLanguageExecutionIdentity,
    ) -> Self {
        Self {
            sink,
            language,
            base_context,
            identity,
        }
    }

    pub(super) fn identity(&self) -> &TraceLanguageExecutionIdentity {
        &self.identity
    }

    pub(super) fn event_key(&self, suffix: impl std::fmt::Display) -> String {
        format!("lashlang_execution:{}:{suffix}", self.identity.graph_key())
    }

    pub(super) fn tool_child_execution_trace_hook(
        &self,
        call_site: lashlang::LashlangExecutionCallSite,
    ) -> ToolChildExecutionTraceHook {
        let trace = self.clone();
        let parent_node_id = call_site.site.node_id;
        let occurrence = call_site.occurrence;
        ToolChildExecutionTraceHook::new(move |started| {
            let child = TraceLanguageChildExecution {
                scope: trace.identity.scope.clone(),
                subject: TraceRuntimeSubject::Process {
                    process_id: started.process_id,
                },
                module_ref: None,
                entry_ref: None,
                entry_name: started.child_entry_name,
            };
            let child_graph_key = child.graph_key();
            trace.emit(TraceLanguageExecution {
                event_key: format!(
                    "lashlang_execution:{}:child:{}:{}:{}",
                    trace.identity.graph_key(),
                    parent_node_id,
                    occurrence,
                    child_graph_key
                ),
                identity: trace.identity.clone(),
                payload: TraceLanguageExecutionPayload::ChildStarted {
                    parent_node_id: parent_node_id.clone(),
                    occurrence,
                    child,
                },
            });
        })
    }

    pub(super) fn emit(&self, event: TraceLanguageExecution) {
        let mut context = self.base_context.clone();
        context.session_id = Some(self.identity.scope.session_id.clone());
        context.turn_id = self.identity.scope.turn_id.clone();
        context.turn_index = self.identity.scope.turn_index;
        context.protocol_iteration = self.identity.scope.protocol_iteration;
        if let TraceRuntimeSubject::Effect { effect_id, .. } = &self.identity.subject {
            context.effect_id = Some(effect_id.clone());
        }
        let _ = self.sink.append(&TraceRecord::new(
            context,
            TraceEvent::LanguageExecution {
                language: self.language.to_string(),
                event,
            },
        ));
    }
}

impl HostBridge<'_> {
    async fn resource_operation(
        &self,
        operation: String,
        receiver: FlowValue,
        args: Vec<FlowValue>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    ) -> Result<FlowValue, ExecutionHostError> {
        if lash_lashlang_runtime::is_typescript_runtime_receiver(&receiver) {
            let fallback_index = call_site.is_none().then(|| self.next_index());
            let effect_id = self.resource_tool_call_id(
                "typescript.runtime",
                call_site.as_ref(),
                fallback_index,
            );
            return lash_lashlang_runtime::journaled_typescript_runtime_value(
                &self.ctx, effect_id, &receiver, &operation, &args,
            )
            .await
            .expect("TypeScript runtime receiver checked above");
        }
        let receiver = match &receiver {
            FlowValue::Resource(receiver) => receiver,
            _ => {
                return Err(ExecutionHostError::new(format!(
                    "module operation `{operation}` requires a module authority receiver"
                )));
            }
        };
        let host_operation =
            resolve_lashlang_module_operation(&self.host_environment, receiver, &operation)?;
        let source_operation = format!("{}.{}", receiver.alias, operation);
        let mut payload = if let [FlowValue::Record(record)] = args.as_slice() {
            flow_record_json(record).await
        } else {
            serde_json::json!({
                "args": flow_values_to_json(&args).await,
            })
        };
        payload
            .as_object_mut()
            .ok_or_else(|| ExecutionHostError::new("module operation payload must be an object"))?;
        let index = self.next_index();
        let call_id = self.resource_tool_call_id(
            &host_operation,
            call_site.as_ref(),
            call_site.is_none().then_some(index),
        );
        if let Some(trigger_operation) =
            lashlang::TriggerHostOperation::from_host_operation(&host_operation)
        {
            let result = self
                .trigger_operation(trigger_operation, payload, call_id)
                .await;
            let outcome = if result.is_ok() {
                lash_core::ExecutedCallOutcome::Ok
            } else {
                lash_core::ExecutedCallOutcome::Err
            };
            self.record_executed_call(index, source_operation, outcome)?;
            return result;
        }
        let tool_id = lash_core::ToolId::from(host_operation.as_str());
        let execution_grant = self
            .ctx
            .callable_tool_manifest_by_id(&tool_id)
            .is_none()
            .then(|| self.deferred_grant_for_tool_id(&tool_id))
            .flatten();
        let call_site = call_site.and_then(|call_site| {
            self.lashlang_execution_trace
                .as_ref()
                .map(|trace| trace.tool_child_execution_trace_hook(call_site))
        });
        let reply = match (execution_grant, call_site) {
            (Some(grant), Some(call_site)) => {
                self.ctx
                    .call_tool_with_execution_grant_and_child_execution_trace_hook(
                        call_id, grant, payload, index, call_site,
                    )
                    .await
            }
            (Some(grant), None) => {
                self.ctx
                    .call_tool_with_execution_grant(call_id, grant, payload, index)
                    .await
            }
            (None, Some(call_site)) => {
                self.ctx
                    .call_tool_by_id_with_child_execution_trace_hook(
                        call_id, tool_id, payload, index, call_site,
                    )
                    .await
            }
            (None, None) => {
                self.ctx
                    .call_tool_by_id(call_id, tool_id, payload, index)
                    .await
            }
        };
        // Invocation replies are terminal: pending calls are resolved before
        // this path. Exhaustiveness keeps a future terminal outcome honest.
        let outcome = match &reply.output.outcome {
            lash_core::ToolCallOutcome::Success(_) => lash_core::ExecutedCallOutcome::Ok,
            lash_core::ToolCallOutcome::Failure(_) | lash_core::ToolCallOutcome::Cancelled(_) => {
                lash_core::ExecutedCallOutcome::Err
            }
        };
        let result = self.consume_reply(&host_operation, reply);
        self.record_executed_call(index, source_operation, outcome)?;
        result
    }

    async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> lashlang::ResourceOperationBatchResult {
        let mut results = vec![None; batch.operations.len()];
        let mut positions = Vec::new();
        let mut host_operations = Vec::new();
        let mut source_operations = Vec::new();
        let mut execution_indices = Vec::new();
        let mut invocations = Vec::new();

        for (source_index, operation) in batch.operations.into_iter().enumerate() {
            if lash_lashlang_runtime::is_typescript_runtime_receiver(&operation.receiver) {
                let effect_id = self.resource_tool_call_id(
                    "typescript.runtime",
                    operation.call_site.as_ref(),
                    Some(source_index),
                );
                let result = lash_lashlang_runtime::journaled_typescript_runtime_value(
                    &self.ctx,
                    effect_id,
                    &operation.receiver,
                    &operation.operation,
                    &operation.args,
                )
                .await
                .expect("TypeScript runtime receiver checked above");
                results[source_index] =
                    Some(lashlang::ResourceOperationResult::from_result(result));
                continue;
            }
            let result = async {
                let receiver = match &operation.receiver {
                    FlowValue::Resource(receiver) => receiver,
                    _ => {
                        return Err(ExecutionHostError::new(format!(
                            "module operation `{}` requires a module authority receiver",
                            operation.operation
                        )));
                    }
                };
                let host_operation = resolve_lashlang_module_operation(
                    &self.host_environment,
                    receiver,
                    &operation.operation,
                )?;
                let source_operation = format!("{}.{}", receiver.alias, operation.operation);
                let mut payload = if let [FlowValue::Record(record)] = operation.args.as_slice() {
                    flow_record_json(record).await
                } else {
                    serde_json::json!({
                        "args": flow_values_to_json(&operation.args).await,
                    })
                };
                payload.as_object_mut().ok_or_else(|| {
                    ExecutionHostError::new("module operation payload must be an object")
                })?;
                Ok::<_, ExecutionHostError>((
                    host_operation,
                    source_operation,
                    payload,
                    operation.call_site,
                ))
            }
            .await;

            let (host_operation, source_operation, payload, call_site) = match result {
                Ok(prepared) => prepared,
                Err(error) => {
                    results[source_index] = Some(lashlang::ResourceOperationResult::Error(error));
                    continue;
                }
            };
            let execution_index = self.next_index();

            if let Some(trigger_operation) =
                lashlang::TriggerHostOperation::from_host_operation(&host_operation)
            {
                let call_id = self.resource_tool_call_id(
                    &host_operation,
                    call_site.as_ref(),
                    Some(source_index),
                );
                let result = self
                    .trigger_operation(trigger_operation, payload, call_id)
                    .await;
                let outcome = if result.is_ok() {
                    lash_core::ExecutedCallOutcome::Ok
                } else {
                    lash_core::ExecutedCallOutcome::Err
                };
                let result = self
                    .record_executed_call(execution_index, source_operation, outcome)
                    .and(result);
                results[source_index] =
                    Some(lashlang::ResourceOperationResult::from_result(result));
                continue;
            }

            let call_id =
                self.resource_tool_call_id(&host_operation, call_site.as_ref(), Some(source_index));
            let mut invocation = ToolInvocation::new(
                call_id,
                lash_core::ToolId::from(host_operation.as_str()),
                payload,
            );
            if self
                .ctx
                .callable_tool_manifest_by_id(&invocation.tool_id)
                .is_none()
                && let Some(grant) = self.deferred_grant_for_tool_id(&invocation.tool_id)
            {
                invocation = invocation.with_execution_grant(grant);
            }
            if let Some(call_site) = call_site.and_then(|call_site| {
                self.lashlang_execution_trace
                    .as_ref()
                    .map(|trace| trace.tool_child_execution_trace_hook(call_site))
            }) {
                invocation = invocation.with_child_execution_trace_hook(call_site);
            }
            positions.push(source_index);
            host_operations.push(host_operation);
            source_operations.push(source_operation);
            execution_indices.push(execution_index);
            invocations.push(invocation);
        }

        let batch = self.ctx.call_tool_batch(invocations).await;
        for ((((source_index, host_operation), source_operation), execution_index), reply) in
            positions
                .iter()
                .copied()
                .zip(host_operations)
                .zip(source_operations)
                .zip(execution_indices)
                .zip(batch.replies)
        {
            // Batch replies are terminal for the same reason as scalar replies.
            let outcome = match &reply.output.outcome {
                lash_core::ToolCallOutcome::Success(_) => lash_core::ExecutedCallOutcome::Ok,
                lash_core::ToolCallOutcome::Failure(_)
                | lash_core::ToolCallOutcome::Cancelled(_) => lash_core::ExecutedCallOutcome::Err,
            };
            let result = self.consume_reply(&host_operation, reply);
            let result = self
                .record_executed_call(execution_index, source_operation, outcome)
                .and(result);
            results[source_index] = Some(lashlang::ResourceOperationResult::from_result(result));
        }

        // The batch reports settlement in its own invocation positions; the VM
        // reads leaf positions. Leaves resolved before the batch ran — the
        // journaled TypeScript runtime values above, and anything that failed
        // during preparation — had already settled, so they lead the order.
        let mut settlement_order = (0..results.len())
            .filter(|index| !positions.contains(index))
            .collect::<Vec<_>>();
        // `call_tool_batch` refuses a malformed order at its boundary, so every
        // reported position is a real invocation position here. Filtering again
        // would only convert a future defect back into a silent repair.
        settlement_order.extend(
            batch
                .settlement_order
                .iter()
                .filter_map(|position| positions.get(*position).copied()),
        );

        lashlang::ResourceOperationBatchResult::settled_in_order(
            results
                .into_iter()
                .map(|result| result.expect("every batch result slot should be filled"))
                .collect(),
            settlement_order,
        )
    }

    async fn trigger_operation(
        &self,
        operation: lashlang::TriggerHostOperation,
        payload: Value,
        effect_id: String,
    ) -> Result<FlowValue, ExecutionHostError> {
        match operation {
            lashlang::TriggerHostOperation::Register => {
                self.register_trigger(payload, effect_id).await
            }
            lashlang::TriggerHostOperation::List => self.list_triggers(payload, effect_id).await,
            lashlang::TriggerHostOperation::Update => {
                self.update_trigger(payload, effect_id, false).await
            }
            lashlang::TriggerHostOperation::Enable => {
                self.set_trigger_enabled(payload, effect_id, true).await
            }
            lashlang::TriggerHostOperation::Disable => {
                self.set_trigger_enabled(payload, effect_id, false).await
            }
            lashlang::TriggerHostOperation::Delete => self.delete_trigger(payload, effect_id).await,
            lashlang::TriggerHostOperation::Revive => {
                self.update_trigger(payload, effect_id, true).await
            }
            lashlang::TriggerHostOperation::Prune => self.prune_triggers(payload, effect_id).await,
        }
    }

    async fn register_trigger(
        &self,
        payload: Value,
        effect_id: String,
    ) -> Result<FlowValue, ExecutionHostError> {
        let request = lashlang::TriggerRegistrationRequest::decode(&payload)
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let draft = self.prepare_trigger_draft(&request).await?;
        let command = lash_core::TriggerCommand::Register {
            owner_scope: self.trigger_owner_scope()?,
            actor: self.ctx.trigger_actor(),
            draft,
        };
        self.execute_trigger_command(effect_id, command).await
    }

    async fn prepare_trigger_draft(
        &self,
        request: &lashlang::TriggerRegistrationRequest,
    ) -> Result<lash_core::TriggerSubscriptionDraft, ExecutionHostError> {
        let artifact = self
            .artifact_store
            .get_module_artifact(&request.target.module_ref)
            .await
            .map_err(|err| {
                ExecutionHostError::new(format!("failed to load lashlang module artifact: {err}"))
            })?
            .ok_or_else(|| {
                ExecutionHostError::new(format!(
                    "missing lashlang module artifact `{}` for trigger target `{}`",
                    request.target.module_ref, request.target.process_name
                ))
            })?;
        let compatibility =
            lashlang::check_trigger_compatibility(lashlang::TriggerCompatibilityRequest {
                artifact: artifact.as_ref(),
                definition: &request.target,
                source_type: &request.source.source_type,
                inputs: &request.inputs,
            })
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let subscription_key =
            materialized_trigger_subscription_key(request.subscription_key.as_deref())?;
        let source_key = lash_core::facade_support::default_trigger_source_key(
            &request.source.source_type,
            &request.source.value,
        );
        let target = trigger_target_process_input(&request.target).map_err(|err| {
            ExecutionHostError::new(format!("failed to encode trigger target: {err}"))
        })?;
        let target_identity = lashlang_process_identity_for_definition(&request.target);
        let process = artifact
            .canonical_ir
            .process(&request.target.process_name)
            .ok_or_else(|| {
                ExecutionHostError::new(format!(
                    "trigger target artifact `{}` is missing process `{}`",
                    request.target.module_ref, request.target.process_name
                ))
            })?;
        let event_types = lashlang_process_event_types()
            .into_iter()
            .chain(lashlang_process_signal_event_types(process))
            .collect::<Vec<_>>();
        let env_ref = self
            .ctx
            .captured_process_execution_env_ref()
            .await
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let draft = lash_core::TriggerSubscriptionDraft {
            subscription_key,
            env_ref,
            wake_target: self.ctx.trigger_registration_wake_target(),
            name: request.name.clone(),
            source_type: request.source.source_type.clone(),
            source_key,
            source: request.source.to_json(),
            payload_schema: lash_core::LashSchema::new(lashlang_type_expr_schema(
                &compatibility.resolved_event_type,
            )),
            target,
            target_identity,
            event_types,
            input_template: core_trigger_input_template(&request.inputs),
            target_label: Some(request.target.process_name.clone()),
        };
        draft
            .validate()
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        Ok(draft)
    }

    async fn list_triggers(
        &self,
        payload: Value,
        effect_id: String,
    ) -> Result<FlowValue, ExecutionHostError> {
        let request = lashlang::TriggerListRequest::decode(&payload)
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let owner_scope = self.trigger_owner_scope()?;
        let mut filter =
            lash_core::TriggerSubscriptionFilter::for_registrant_scope(owner_scope.namespace());
        filter.name = request.name;
        filter.source_type = request.source_type;
        filter.enabled = request.enabled;
        filter.target = request
            .target
            .as_ref()
            .map(lashlang_process_definition_for_identity);
        self.execute_trigger_command(
            effect_id,
            lash_core::TriggerCommand::List {
                owner_scope,
                filter,
            },
        )
        .await
    }

    async fn update_trigger(
        &self,
        payload: Value,
        effect_id: String,
        revive: bool,
    ) -> Result<FlowValue, ExecutionHostError> {
        let request = lashlang::TriggerRegistrationRequest::decode(&payload)
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let subscription_key = request
            .subscription_key
            .clone()
            .ok_or_else(|| ExecutionHostError::new("trigger update requires `subscription_key`"))?;
        let expected_revision = trigger_expected_revision(&payload)?;
        let draft = self.prepare_trigger_draft(&request).await?;
        let owner_scope = self.trigger_owner_scope()?;
        let actor = self.ctx.trigger_actor();
        let command = if revive {
            lash_core::TriggerCommand::Revive {
                owner_scope,
                actor,
                subscription_key,
                draft,
                expected_revision,
            }
        } else {
            lash_core::TriggerCommand::Update {
                owner_scope,
                actor,
                subscription_key,
                draft,
                expected_revision,
            }
        };
        self.execute_trigger_command(effect_id, command).await
    }

    async fn set_trigger_enabled(
        &self,
        payload: Value,
        effect_id: String,
        enabled: bool,
    ) -> Result<FlowValue, ExecutionHostError> {
        let (subscription_key, expected_revision) = trigger_key_and_revision(&payload)?;
        let owner_scope = self.trigger_owner_scope()?;
        let actor = self.ctx.trigger_actor();
        let command = if enabled {
            lash_core::TriggerCommand::Enable {
                owner_scope,
                actor,
                subscription_key,
                expected_revision,
            }
        } else {
            lash_core::TriggerCommand::Disable {
                owner_scope,
                actor,
                subscription_key,
                expected_revision,
            }
        };
        self.execute_trigger_command(effect_id, command).await
    }

    async fn delete_trigger(
        &self,
        payload: Value,
        effect_id: String,
    ) -> Result<FlowValue, ExecutionHostError> {
        let (subscription_key, expected_revision) = trigger_key_and_revision(&payload)?;
        let command = lash_core::TriggerCommand::Delete {
            owner_scope: self.trigger_owner_scope()?,
            actor: self.ctx.trigger_actor(),
            subscription_key,
            expected_revision,
        };
        self.execute_trigger_command(effect_id, command).await
    }

    async fn prune_triggers(
        &self,
        payload: Value,
        effect_id: String,
    ) -> Result<FlowValue, ExecutionHostError> {
        let request = lashlang::TriggerPruneRequest::decode(&payload)
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let command = lash_core::TriggerCommand::Prune {
            owner_scope: self.trigger_owner_scope()?,
            actor: self.ctx.trigger_actor(),
            subscription_keys: request.subscription_keys,
        };
        self.execute_trigger_command(effect_id, command).await
    }

    fn trigger_owner_scope(&self) -> Result<lash_core::TriggerOwnerScope, ExecutionHostError> {
        self.ctx
            .trigger_owner_scope()
            .map_err(|err| ExecutionHostError::new(err.to_string()))
    }

    async fn execute_trigger_command(
        &self,
        effect_id: String,
        command: lash_core::TriggerCommand,
    ) -> Result<FlowValue, ExecutionHostError> {
        let outcome = self
            .ctx
            .execute_trigger_effect(effect_id, command)
            .await
            .map_err(|err| ExecutionHostError::new(err.to_string()))?
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        let value = match outcome {
            lash_core::TriggerCommandOutcome::Mutation { receipt } => {
                let mut value = serde_json::to_value(&receipt).map_err(|err| {
                    ExecutionHostError::new(format!("failed to encode trigger receipt: {err}"))
                })?;
                let object = value.as_object_mut().ok_or_else(|| {
                    ExecutionHostError::new("trigger mutation receipt must encode as a record")
                })?;
                object.insert("type".to_string(), serde_json::json!("trigger_handle"));
                object.insert(
                    "id".to_string(),
                    serde_json::json!(receipt.subscription_key),
                );
                value
            }
            lash_core::TriggerCommandOutcome::List { records } => serde_json::to_value(
                records
                    .iter()
                    .map(|record| {
                        lash_core::facade_support::TriggerRegistration::from_record_with_manifest(
                            record,
                            Some(&self.trigger_key_manifest.subscription_keys),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|err| {
                ExecutionHostError::new(format!("failed to encode trigger records: {err}"))
            })?,
            lash_core::TriggerCommandOutcome::Prune { receipts } => {
                let values = receipts
                    .iter()
                    .map(|receipt| {
                        let mut value = serde_json::to_value(receipt).map_err(|err| {
                            ExecutionHostError::new(format!(
                                "failed to encode trigger prune receipt: {err}"
                            ))
                        })?;
                        let object = value.as_object_mut().ok_or_else(|| {
                            ExecutionHostError::new("trigger prune receipt must encode as a record")
                        })?;
                        object.insert("type".to_string(), serde_json::json!("trigger_handle"));
                        object.insert(
                            "id".to_string(),
                            serde_json::json!(receipt.subscription_key),
                        );
                        Ok(value)
                    })
                    .collect::<Result<Vec<_>, ExecutionHostError>>()?;
                serde_json::Value::Array(values)
            }
        };
        Ok(lashlang::from_json(value))
    }

    async fn start_process(&self, start: ProcessStart) -> Result<FlowValue, ExecutionHostError> {
        let prepared = {
            let _phase = self.ctx.named_phase("rlm_process.prepare_start");
            let parent_start_seed = lashlang_parent_start_seed(&self.ctx);
            prepare_lashlang_process_start(
                std::sync::Arc::clone(&self.artifact_store),
                &parent_start_seed,
                start,
            )
            .await
            .map_err(|err| ExecutionHostError::new(err.to_string()))?
        };
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.start");
            self.ctx
                .start_child_process(prepared.registration, LASHLANG_ENGINE_KIND, prepared.label)
                .await
        };
        self.consume_reply("start_process", reply)
    }

    async fn await_handle(&self, handle: FlowValue) -> Result<FlowValue, ExecutionHostError> {
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.await_handle");
            self.ctx
                .await_tool_handle(
                    uuid::Uuid::new_v4().to_string(),
                    handle_to_json(&handle).await?,
                )
                .await
        };
        self.consume_reply("await_handle", reply)
    }

    async fn cancel_handle(&self, handle: FlowValue) -> Result<FlowValue, ExecutionHostError> {
        let reply = self
            .ctx
            .cancel_tool_handle(
                uuid::Uuid::new_v4().to_string(),
                handle_to_json(&handle).await?,
            )
            .await;
        self.consume_reply("cancel_handle", reply)
    }

    async fn signal_run(&self, signal: ProcessSignal) -> Result<FlowValue, ExecutionHostError> {
        let handle = handle_to_json(&signal.run).await?;
        let payload = handle_to_json(&signal.payload).await?;
        let reply = self
            .ctx
            .signal_tool_handle(
                uuid::Uuid::new_v4().to_string(),
                handle,
                signal.name,
                payload,
            )
            .await;
        self.consume_reply("signal_run", reply)?;
        // `signal_run` evaluates to null in the language; the appended event is
        // recorded as a tool call but not surfaced as the expression value.
        Ok(FlowValue::Null)
    }

    async fn print(&self, value: FlowValue) -> Result<(), ExecutionHostError> {
        let attachment_store = self.ctx.attachment_store();
        let images = collect_printed_images(&value, attachment_store.as_ref()).await?;
        let projected_text = {
            let _phase = self.ctx.named_phase("rlm_lashlang.print_project");
            self.print_projector
                .project(ValueProjectionContext::new(&value))
                .await
        };
        let raw_text = format_output_value(&value).await;
        let projection =
            crate::rlm_support::observation_projection_metadata(&raw_text, &projected_text);
        self.observations.lock_recover().push(Observation {
            text: raw_text,
            projection,
        });
        if !images.is_empty() {
            self.printed_images.lock_recover().extend(images);
        }
        Ok(())
    }

    async fn sleep(&self, sleep: Sleep) -> Result<FlowValue, ExecutionHostError> {
        let duration_ms = sleep_duration_ms(sleep.kind, &sleep.value)?;
        let sequence = self.sleep_sequence.fetch_add(1, Ordering::Relaxed);
        self.ctx
            .sleep_process("foreground", sequence, duration_ms)
            .await
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        Ok(FlowValue::Null)
    }

    fn perform_selected_ability<'a>(&'a self, op: AbilityOp) -> HostAbilityFuture<'a> {
        match op {
            AbilityOp::ResourceOperation(operation) => Box::pin(async move {
                let lashlang::ResourceOperation {
                    operation,
                    receiver,
                    args,
                    call_site,
                } = operation;
                Box::pin(self.resource_operation(operation, receiver, args, call_site))
                    .await
                    .map(AbilityResult::Value)
            }),
            AbilityOp::ResourceOperationBatch(batch) => Box::pin(async move {
                Ok(AbilityResult::ResourceOperationBatch(
                    self.resource_operation_batch(batch).await,
                ))
            }),
            AbilityOp::StartProcess(start) => {
                Box::pin(async move { self.start_process(*start).await.map(AbilityResult::Value) })
            }
            AbilityOp::Await(handle) => {
                Box::pin(async move { self.await_handle(handle).await.map(AbilityResult::Value) })
            }
            AbilityOp::Cancel(handle) => {
                Box::pin(async move { self.cancel_handle(handle).await.map(AbilityResult::Value) })
            }
            AbilityOp::Print(value) => Box::pin(async move {
                self.print(value).await?;
                Ok(AbilityResult::Unit)
            }),
            AbilityOp::Sleep(sleep) => {
                Box::pin(async move { self.sleep(sleep).await.map(AbilityResult::Value) })
            }
            AbilityOp::ProcessEvent(_) => Box::pin(async {
                Err(ExecutionHostError::new(
                    "process events are only available inside lashlang process bodies",
                ))
            }),
            AbilityOp::WaitSignal { .. } => Box::pin(async {
                Err(ExecutionHostError::new(
                    "`wait_signal` is only available inside lashlang process bodies",
                ))
            }),
            AbilityOp::SignalRun(signal) => {
                Box::pin(async move { self.signal_run(signal).await.map(AbilityResult::Value) })
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                Box::pin(async move { Ok(AbilityResult::Value(value)) })
            }
        }
    }
}

impl ExecutionHost for HostBridge<'_> {
    fn perform(
        &self,
        op: AbilityOp,
    ) -> impl Future<Output = Result<AbilityResult, ExecutionHostError>> + Send {
        self.perform_selected_ability(op)
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }

    fn is_cancelled(&self) -> bool {
        self.ctx.is_cancelled()
    }

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        let Some(trace) = &self.lashlang_execution_trace else {
            return;
        };
        let (suffix, payload) = match observation {
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence } => (
                format!("node:{}:{occurrence}:started", site.node_id),
                TraceLanguageExecutionPayload::NodeStarted {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                },
            ),
            lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence } => (
                format!("node:{}:{occurrence}:completed", site.node_id),
                TraceLanguageExecutionPayload::NodeCompleted {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                },
            ),
            lashlang::LashlangExecutionObservation::NodeFailed {
                site,
                occurrence,
                error,
            } => (
                format!("node:{}:{occurrence}:failed", site.node_id),
                TraceLanguageExecutionPayload::NodeFailed {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                    error,
                },
            ),
            lashlang::LashlangExecutionObservation::BranchSelected {
                site,
                occurrence,
                edge_id,
                selected,
            } => (
                format!("branch:{}:{occurrence}:{edge_id}", site.node_id),
                TraceLanguageExecutionPayload::BranchSelected {
                    node_id: site.node_id,
                    occurrence,
                    edge_id,
                    selected: match selected {
                        lashlang::ProcessBranchSelection::Then => TraceBranchSelection::Then,
                        lashlang::ProcessBranchSelection::Else => TraceBranchSelection::Else,
                    },
                },
            ),
            lashlang::LashlangExecutionObservation::ChildStarted {
                site,
                occurrence,
                child,
            } => (
                format!("child:{}:{occurrence}:{}", site.node_id, child.process_id),
                TraceLanguageExecutionPayload::ChildStarted {
                    parent_node_id: site.node_id,
                    occurrence,
                    child: TraceLanguageChildExecution {
                        scope: trace.identity().scope.clone(),
                        subject: TraceRuntimeSubject::Process {
                            process_id: child.process_id,
                        },
                        module_ref: Some(child.module_ref.to_string()),
                        entry_ref: Some(lashlang::process_ref_key(&child.process_ref)),
                        entry_name: Some(child.process_name),
                    },
                },
            ),
        };
        trace.emit(TraceLanguageExecution {
            event_key: trace.event_key(suffix),
            identity: trace.identity().clone(),
            payload,
        });
    }
}

async fn handle_to_json(value: &FlowValue) -> Result<Value, ExecutionHostError> {
    match value {
        FlowValue::Projected(_) => Ok(flow_to_json_value(value).await),
        _ => lashlang_value_to_json(value),
    }
}

fn lashlang_parent_start_seed(ctx: &RuntimeExecutionContext<'_>) -> String {
    if let Some(invocation) = ctx.parent_invocation() {
        if let Some(replay_key) = invocation.replay_key() {
            return format!("runtime-replay:{replay_key}");
        }
        if let Some(effect_id) = invocation.effect_id() {
            return format!("runtime-effect:{effect_id}");
        }
    }
    format!("runtime-scope:{}", ctx.execution_scope_id())
}

fn lashlang_process_input_for_definition(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> LashlangProcessInput {
    LashlangProcessInput {
        module_ref: definition.module_ref.clone(),
        process_ref: definition.process_ref.clone(),
        host_requirements_ref: definition.host_requirements_ref.clone(),
        process_name: definition.process_name.clone(),
        args: serde_json::Map::new(),
    }
}

fn lashlang_process_definition_for_identity(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> serde_json::Value {
    lashlang_process_input_for_definition(definition).definition()
}

fn lashlang_process_identity_for_definition(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> lash_core::ProcessIdentity {
    lash_core::ProcessIdentity::new(LASHLANG_ENGINE_KIND)
        .with_label(Some(definition.process_name.clone()))
        .with_definition(Some(lashlang_process_definition_for_identity(definition)))
}

fn trigger_key_and_revision(payload: &Value) -> Result<(String, u64), ExecutionHostError> {
    let subscription_key = payload
        .get("subscription_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExecutionHostError::new("trigger operation requires `subscription_key`"))?;
    Ok((subscription_key, trigger_expected_revision(payload)?))
}

fn trigger_expected_revision(payload: &Value) -> Result<u64, ExecutionHostError> {
    payload
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ExecutionHostError::new(
                "trigger operation requires a non-negative integer `expected_revision`",
            )
        })
}

fn materialized_trigger_subscription_key(
    subscription_key: Option<&str>,
) -> Result<String, ExecutionHostError> {
    subscription_key.map(ToOwned::to_owned).ok_or_else(|| {
        ExecutionHostError::new(
            "linked lashlang trigger registrations must carry a materialized `subscription_key`",
        )
    })
}

fn trigger_target_process_input(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> Result<lash_core::ProcessInput, serde_json::Error> {
    lashlang_process_input_for_definition(definition).into_process_input()
}

fn core_trigger_input_template(
    input: &lashlang::TriggerInputTemplate,
) -> BTreeMap<String, lash_core::TriggerInputBinding> {
    input
        .entries()
        .map(|(name, binding)| {
            let binding = match binding {
                lashlang::TriggerInputBinding::Event => lash_core::TriggerInputBinding::Event,
                lashlang::TriggerInputBinding::Fixed { value } => {
                    lash_core::TriggerInputBinding::Fixed {
                        value: value.clone(),
                    }
                }
            };
            (name.to_string(), binding)
        })
        .collect()
}

fn flow_values_to_json<'a>(values: &'a [FlowValue]) -> ProjectedFuture<'a, Vec<Value>> {
    Box::pin(async move {
        let mut out = Vec::with_capacity(values.len());
        for value in values {
            out.push(flow_to_json_value(value).await);
        }
        out
    })
}

fn flow_record_json<'a>(record: &'a FlowRecord) -> ProjectedFuture<'a, Value> {
    Box::pin(async move {
        let mut object = serde_json::Map::with_capacity(record.len());
        for (key, value) in record.iter() {
            object.insert(key.to_string(), flow_to_json_value(value).await);
        }
        Value::Object(object)
    })
}

pub(super) struct CollectedExecutionOutput {
    pub(super) observations: Vec<Observation>,
    pub(super) printed_images: Vec<AttachmentRef>,
    pub(super) tool_calls: Vec<lash_core::ToolCallRecord>,
    pub(super) executed_calls: Vec<lash_core::ExecutedCallRecord>,
}

async fn collect_printed_images(
    value: &FlowValue,
    attachment_store: &lash_core::facade_support::SessionAttachmentStore,
) -> Result<Vec<AttachmentRef>, ExecutionHostError> {
    let mut seen = HashSet::new();
    let mut images = Vec::new();
    collect_printed_images_inner(value, attachment_store, &mut seen, &mut images).await?;
    Ok(images)
}

fn collect_printed_images_inner<'a>(
    value: &'a FlowValue,
    attachment_store: &'a lash_core::facade_support::SessionAttachmentStore,
    seen: &'a mut HashSet<String>,
    images: &'a mut Vec<AttachmentRef>,
) -> ProjectedFuture<'a, Result<(), ExecutionHostError>> {
    Box::pin(async move {
        match value {
            FlowValue::Image(image) => {
                if !seen.insert(image.id.clone()) {
                    return Ok(());
                }
                // The image id rides in from program-produced values, so it is
                // untrusted: a malformed one is a host error, not a lookup.
                let id = lash_core::AttachmentId::parse(&image.id).map_err(|err| {
                    ExecutionHostError::new(format!("printed image id is unusable: {err}"))
                })?;
                attachment_store.get(&id).await.map_err(|_| {
                    ExecutionHostError::new(format!(
                        "image bytes for `{}` are unavailable or were pruned",
                        image.id
                    ))
                })?;
                let reference = AttachmentRef {
                    id,
                    media_type: image.mime.clone(),
                    byte_len: image.size,
                    type_metadata: Some(lash_core::AttachmentTypeMetadata::image(
                        image.width,
                        image.height,
                    )),
                    label: Some(image.label.clone()),
                };
                images.push(reference);
            }
            FlowValue::Tuple(values) | FlowValue::List(values) => {
                for value in values.iter() {
                    collect_printed_images_inner(value, attachment_store, seen, images).await?;
                }
            }
            FlowValue::Record(record) => {
                for (_, value) in record.iter() {
                    collect_printed_images_inner(value, attachment_store, seen, images).await?;
                }
            }
            FlowValue::Projected(value) => {
                collect_printed_images_inner(
                    &value.materialize_async().await,
                    attachment_store,
                    seen,
                    images,
                )
                .await?;
            }
            FlowValue::Null
            | FlowValue::Undefined
            | FlowValue::Bool(_)
            | FlowValue::Number(_)
            | FlowValue::String(_)
            | FlowValue::Resource(_) => {}
            FlowValue::Ref(_) => {
                unreachable!("VM heap references must be materialized before host rendering")
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_registration_rejects_an_unmaterialized_subscription_key() {
        let error =
            materialized_trigger_subscription_key(None).expect_err("missing key must be rejected");

        assert_eq!(
            error.to_string(),
            "linked lashlang trigger registrations must carry a materialized `subscription_key`"
        );
    }
}
