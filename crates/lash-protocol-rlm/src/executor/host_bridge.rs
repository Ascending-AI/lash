use lash_sansio::sync::{LockResultExt, MutexExt};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use lash_core::{
    AttachmentRef, Observation, RuntimeExecutionContext, ToolExecutionGrant, TraceContext,
    TraceEvent, facade_support::ToolChildExecutionTraceHook, facade_support::ToolInvocation,
    facade_support::ToolInvocationReply, facade_support::TraceBranchSelection,
    facade_support::TraceRecord, facade_support::TraceRuntimeSubject, facade_support::TraceSink,
};
use lash_lashlang_runtime::{
    CommandShape, ExecutionCancellation, TraceLanguageChildExecution, TraceLanguageExecution,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, lashlang_value_to_json,
    process_sleep, protocol_tool_output_to_lashlang_value, resolve_lashlang_module_operation,
};
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ProjectedFuture,
    Record as FlowRecord, Sleep, Value as FlowValue, ValueProjectionContext, ValueProjector,
};
use serde_json::Value;

use super::cell_run::{CellRun, LashlangCellOpener};
use crate::projection::{flow_to_json_value, format_output_value};

pub(super) struct HostBridge<'run> {
    ctx: RuntimeExecutionContext<'run>,
    /// The cell's replay run — the identities it mints, its issue-ordinal
    /// mint and its recorded frontier — or the reason this execution has no
    /// logical opener to mint under.
    cell: Arc<Result<CellRun, LashlangCellOpener>>,
    print_projector: std::sync::Arc<dyn ValueProjector>,
    observations: Mutex<Vec<Observation>>,
    printed_images: Mutex<Vec<AttachmentRef>>,
    calls: Mutex<Vec<(usize, lash_core::ExecutedCall)>>,
    next_tool_index: Mutex<usize>,
    lashlang_execution_trace: Option<LashlangExecutionTrace>,
    host_environment: lashlang::LashlangHostEnvironment,
    deferred_execution_grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    /// The cell's journaled binding set, against the live registry (FIG-3587).
    cell_bindings: lash_lashlang_runtime::CellToolBindings,
    artifact_store: std::sync::Arc<dyn lashlang::LashlangArtifactStore>,
    /// Attempt bound stamped onto children this execution starts. `None` until
    /// this execution actually starts a child: an execution that never starts
    /// one pins nothing and leaves the durable snapshot root alone.
    child_max_attempts: Mutex<Option<std::num::NonZeroU32>>,
    /// This cell's own cancellation scope, beside the turn's. A cancelled tool
    /// call ends the cell here, so `is_cancelled` refuses its next effect
    /// instead of the guest catching the cancellation as a rejected call. A
    /// replay divergence ends it the same way (FIG-3586).
    cancellation: ExecutionCancellation,
}

pub(super) struct HostBridgeConfig<'run> {
    pub ctx: RuntimeExecutionContext<'run>,
    pub cell: Arc<Result<CellRun, LashlangCellOpener>>,
    pub print_projector: std::sync::Arc<dyn ValueProjector>,
    pub lashlang_execution_trace: Option<LashlangExecutionTrace>,
    pub host_environment: lashlang::LashlangHostEnvironment,
    pub deferred_execution_grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    pub cell_bindings: lash_lashlang_runtime::CellToolBindings,
    pub artifact_store: std::sync::Arc<dyn lashlang::LashlangArtifactStore>,
    /// Bound already pinned by an earlier cell of this execution, if any.
    pub child_max_attempts: Option<std::num::NonZeroU32>,
}

type HostAbilityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AbilityResult, ExecutionHostError>> + Send + 'a>>;

impl<'run> HostBridge<'run> {
    pub(super) fn new(config: HostBridgeConfig<'run>) -> Self {
        Self {
            cell: config.cell,
            ctx: config.ctx,
            print_projector: config.print_projector,
            observations: Mutex::new(Vec::new()),
            printed_images: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            next_tool_index: Mutex::new(0),
            lashlang_execution_trace: config.lashlang_execution_trace,
            host_environment: config.host_environment,
            deferred_execution_grants: config.deferred_execution_grants,
            cell_bindings: config.cell_bindings,
            artifact_store: config.artifact_store,
            child_max_attempts: Mutex::new(config.child_max_attempts),
            cancellation: ExecutionCancellation::new(),
        }
    }

    /// The bound this execution has pinned, if it started a child at all.
    pub(super) fn pinned_child_max_attempts(&self) -> Option<std::num::NonZeroU32> {
        *self.child_max_attempts.lock_recover()
    }

    fn next_index(&self) -> usize {
        let mut guard = self.next_tool_index.lock_recover();
        let next = *guard;
        *guard += 1;
        next
    }

    fn cell(&self) -> Result<&CellRun, ExecutionHostError> {
        self.cell
            .as_ref()
            .as_ref()
            .map_err(|error| ExecutionHostError::new(error.to_string()))
    }

    /// The run's command protocol over this cell's execution (FIG-3586).
    fn commands(
        &self,
    ) -> Result<lash_lashlang_runtime::ReplayCommands<'_, 'run>, ExecutionHostError> {
        Ok(self.cell()?.commands(&self.ctx, &self.cancellation))
    }

    fn consume_reply(
        &self,
        reply: ToolInvocationReply,
        replay_key: &str,
    ) -> (
        Result<FlowValue, ExecutionHostError>,
        Option<lash_core::ToolCallRecord>,
    ) {
        let result =
            protocol_tool_output_to_lashlang_value(&reply.output, replay_key, &self.cancellation);
        (result, reply.record)
    }

    fn record_executed_call(
        &self,
        index: usize,
        operation: String,
        outcome: lash_core::ExecutedCallOutcome,
        host_record: Option<lash_core::ToolCallRecord>,
    ) -> Result<(), ExecutionHostError> {
        // This ledger records dispatches only. Resolution, argument, and other
        // pre-dispatch failures deliberately produce no `Calls:` entry because
        // the source operation did not execute.
        self.calls.lock_recover().push((
            index,
            lash_core::ExecutedCall {
                operation,
                outcome,
                host_record,
            },
        ));
        Ok(())
    }

    fn consume_recorded_reply(
        &self,
        index: usize,
        operation: &str,
        reply: ToolInvocationReply,
        replay_key: &str,
    ) -> Result<FlowValue, ExecutionHostError> {
        let outcome = if reply.output.is_success() {
            lash_core::ExecutedCallOutcome::Ok
        } else {
            lash_core::ExecutedCallOutcome::Err
        };
        let (result, host_record) = self.consume_reply(reply, replay_key);
        if let Some(host_record) = host_record {
            self.record_executed_call(index, operation.to_string(), outcome, Some(host_record))?;
        }
        result
    }

    pub(super) fn into_collected(self) -> CollectedExecutionOutput {
        let mut calls = self.calls.into_inner().recover();
        calls.sort_by_key(|(index, _)| *index);
        CollectedExecutionOutput {
            observations: self.observations.into_inner().recover(),
            printed_images: self.printed_images.into_inner().recover(),
            calls: calls.into_iter().map(|(_, call)| call).collect(),
        }
    }

    pub(super) fn cancellation_observed(&self) -> bool {
        self.ctx.is_cancelled()
    }

    /// The identity of one leaf this cell calls, or of one child of an
    /// aggregate when `leaf_index` names a position inside a batch.
    ///
    /// One derivation, one scope. It was two: the sited path scoped on the
    /// effect address the cell runs under and the unsited one on the bare
    /// session id, so the two cells of one session minted one identity for
    /// their first unsited call.
    fn resource_tool_call_id(
        &self,
        ordinal: u64,
        call_site: &lashlang::LashlangExecutionCallSite,
        leaf_index: Option<usize>,
    ) -> Result<String, ExecutionHostError> {
        // The id is the issue ordinal under the cell's scope (FIG-3586); the
        // call site only correlates it with the node on the trace.
        let identities = self.cell()?.identities();
        let call_id = match leaf_index {
            Some(leaf_index) => identities.child_call_id(ordinal, leaf_index),
            None => identities.call_id(ordinal),
        };
        if let Some(trace) = &self.lashlang_execution_trace {
            trace.record_resource_call(call_site, &call_id);
        }
        Ok(call_id)
    }

    /// The call site a leaf must carry, or the refusal both bridges give.
    ///
    /// This tier used to fall back: an unsited scalar call took the host's
    /// dispatch counter, and an unsited batch leaf took its position inside
    /// the batch — which two identical aggregates share, so they minted one
    /// set of identities twice. The fallback was never reachable. Every
    /// production compile for this bridge and for the process bridge goes
    /// through the one entry, `lashlang::compile`, which always enables
    /// execution-site tracking; `lashlang_execution_paths` walks
    /// `program.main` through the total `Expr::children()` walk, which
    /// descends into function literals, process literals, callbacks, `try`
    /// bodies and comprehension clauses; a TypeScript `function` statement
    /// lowers to a function *literal bound in main*, never to a
    /// `Declaration::Function`; and `execution_site_descriptor` names every
    /// `Expr::ReceiverCall`. A leaf without a site is therefore a defect
    /// upstream of here, refused with the same typed error the process bridge
    /// already used rather than given an invented identity.
    fn require_call_site<'site>(
        operation: &str,
        host_operation: &str,
        call_site: Option<&'site lashlang::LashlangExecutionCallSite>,
    ) -> Result<&'site lashlang::LashlangExecutionCallSite, ExecutionHostError> {
        call_site.ok_or_else(|| {
            ExecutionHostError::from(
                lash_lashlang_runtime::LashlangHostError::OperationCallSiteMissing {
                    operation: operation.to_string(),
                    host_operation: host_operation.to_string(),
                },
            )
        })
    }

    /// The drift refusal of the first aggregate leaf naming a drifted binding.
    fn aggregate_drift(
        &self,
        leaves: &[lashlang::ResourceOperationBatchLeaf],
    ) -> Option<lash_core::RuntimeEffectControllerError> {
        if !self.cell_bindings.has_drift() {
            return None;
        }
        leaves.iter().find_map(|leaf| {
            let lashlang::ResourceOperationBatchLeaf::Operation(operation) = leaf else {
                return None;
            };
            let FlowValue::Resource(receiver) = &operation.receiver else {
                return None;
            };
            let host_operation = resolve_lashlang_module_operation(
                &self.host_environment,
                receiver,
                &operation.operation,
            )
            .ok()?;
            self.cell_bindings
                .drift_for(&lash_core::ToolId::from(host_operation.as_str()))
                .map(lash_lashlang_runtime::CellBindingDrift::refusal)
        })
    }

    fn deferred_grant_for_tool_id(
        &self,
        tool_id: &lash_core::ToolId,
    ) -> Option<ToolExecutionGrant> {
        self.deferred_execution_grants.get(tool_id).cloned()
    }

    /// Builds one tool call's invocation: its positional id, the grant a
    /// deferred resolution pinned when the catalog does not carry the tool,
    /// the issuing node for traces, and the child-execution trace hook.
    fn tool_invocation(
        &self,
        call_id: String,
        host_operation: &str,
        payload: Value,
        call_site: Option<&lashlang::LashlangExecutionCallSite>,
    ) -> ToolInvocation {
        let mut invocation =
            ToolInvocation::new(call_id, lash_core::ToolId::from(host_operation), payload);
        if let Some(call_site) = call_site {
            invocation = invocation.with_issuing_language_node_id(call_site.site.node_id.clone());
        }
        if self
            .ctx
            .callable_tool_manifest_by_id(&invocation.tool_id)
            .is_none()
            && let Some(grant) = self.deferred_grant_for_tool_id(&invocation.tool_id)
        {
            invocation = invocation.with_execution_grant(grant);
        }
        if let (Some(trace), Some(call_site)) = (&self.lashlang_execution_trace, call_site) {
            invocation = invocation.with_child_execution_trace_hook(
                trace.tool_child_execution_trace_hook(call_site.clone()),
            );
        }
        invocation
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
    resource_call_ids: std::sync::Arc<Mutex<BTreeMap<(String, u64), String>>>,
    pending_resource_starts:
        std::sync::Arc<Mutex<BTreeMap<(String, u64), lashlang::LashlangExecutionSite>>>,
    active_nodes: std::sync::Arc<Mutex<HashSet<(String, lash_sansio::ExecutionNodeKind, u64)>>>,
    waiting_nodes: lash_lashlang_runtime::TraceWaitBookkeeping,
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
            resource_call_ids: std::sync::Arc::default(),
            pending_resource_starts: std::sync::Arc::default(),
            active_nodes: std::sync::Arc::default(),
            waiting_nodes: lash_lashlang_runtime::TraceWaitBookkeeping::default(),
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
                process_id: started.process_id,
                incarnation: started.incarnation.registration_sequence(),
                attempt: started.attempt,
                module_ref: None,
                entry_ref: None,
                entry_name: started.child_entry_name,
            };
            let child_graph_key = child.graph_key().unwrap_or_else(|| {
                format!(
                    "process:{}:incarnation:{}",
                    child.process_id, child.incarnation
                )
            });
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
        context.session_id = self.identity.scope.session_id.clone();
        context.turn_id = self.identity.scope.turn_id.clone();
        context.turn_index = self.identity.scope.turn_index;
        context.protocol_iteration = self.identity.scope.protocol_iteration;
        if let TraceRuntimeSubject::Effect { effect_id, .. } = &self.identity.subject {
            context.effect_id = Some(effect_id.clone());
        }
        context.graph_node_id = language_event_node_id(&event.payload).map(str::to_string);
        let _ = self.sink.append(&TraceRecord::new(
            context,
            TraceEvent::LanguageExecution {
                language: self.language.to_string(),
                event,
            },
        ));
    }

    fn emit_waiting(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        awaited: lash_lashlang_runtime::TraceNodeAwaited,
    ) {
        let site = &call_site.site;
        self.waiting_nodes
            .mark_waiting(&site.node_id, site.node_kind, call_site.occurrence);
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(format!(
                "node:{}:{}:waiting",
                site.node_id, call_site.occurrence
            )),
            identity: self.identity.clone(),
            payload: TraceLanguageExecutionPayload::NodeWaiting {
                node_id: site.node_id.clone(),
                node_kind: site.node_kind,
                label: site.label.clone(),
                occurrence: call_site.occurrence,
                awaited,
            },
        });
    }

    fn emit_resumed(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        resolution: lash_lashlang_runtime::TraceNodeWaitResolution,
    ) {
        let site = &call_site.site;
        self.waiting_nodes
            .finish(&site.node_id, site.node_kind, call_site.occurrence);
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(format!(
                "node:{}:{}:resumed",
                site.node_id, call_site.occurrence
            )),
            identity: self.identity.clone(),
            payload: TraceLanguageExecutionPayload::NodeResumed {
                node_id: site.node_id.clone(),
                node_kind: site.node_kind,
                label: site.label.clone(),
                occurrence: call_site.occurrence,
                resolution,
            },
        });
    }

    fn emit_cancelled_wait(&self, site: &lashlang::LashlangExecutionSite, occurrence: u64) {
        if self
            .waiting_nodes
            .finish(&site.node_id, site.node_kind, occurrence)
        {
            self.emit(TraceLanguageExecution {
                event_key: self.event_key(format!("node:{}:{occurrence}:resumed", site.node_id)),
                identity: self.identity.clone(),
                payload: TraceLanguageExecutionPayload::NodeResumed {
                    node_id: site.node_id.clone(),
                    node_kind: site.node_kind,
                    label: site.label.clone(),
                    occurrence,
                    resolution: lash_lashlang_runtime::TraceNodeWaitResolution::Cancelled,
                },
            });
        }
    }

    fn emit_cancelled_site(&self, site: lashlang::LashlangExecutionSite, occurrence: u64) {
        if !self.active_nodes.lock_recover().remove(&(
            site.node_id.clone(),
            site.node_kind,
            occurrence,
        )) {
            return;
        }
        self.finish_resource_call(&site, occurrence);
        self.emit_cancelled_wait(&site, occurrence);
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(format!("node:{}:{occurrence}:cancelled", site.node_id)),
            identity: self.identity.clone(),
            payload: TraceLanguageExecutionPayload::NodeCancelled {
                node_id: site.node_id,
                node_kind: site.node_kind,
                label: site.label,
                occurrence,
            },
        });
    }

    fn record_resource_call(&self, call_site: &lashlang::LashlangExecutionCallSite, call_id: &str) {
        let key = (call_site.site.node_id.clone(), call_site.occurrence);
        self.resource_call_ids
            .lock_recover()
            .insert(key.clone(), call_id.to_string());
        if let Some(site) = self.pending_resource_starts.lock_recover().remove(&key) {
            self.emit(TraceLanguageExecution {
                event_key: self.event_key(format!(
                    "node:{}:{}:started",
                    site.node_id, call_site.occurrence
                )),
                identity: self.identity.clone(),
                payload: TraceLanguageExecutionPayload::NodeStarted {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence: call_site.occurrence,
                    call_id: Some(call_id.to_string()),
                },
            });
        }
    }

    fn finish_resource_call(
        &self,
        site: &lashlang::LashlangExecutionSite,
        occurrence: u64,
    ) -> Option<String> {
        if site.node_kind != lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND {
            return None;
        }
        let key = (site.node_id.clone(), occurrence);
        let call_id = self.resource_call_ids.lock_recover().remove(&key);
        if let Some(started) = self.pending_resource_starts.lock_recover().remove(&key) {
            self.emit(TraceLanguageExecution {
                event_key: self.event_key(format!("node:{}:{occurrence}:started", started.node_id)),
                identity: self.identity.clone(),
                payload: TraceLanguageExecutionPayload::NodeStarted {
                    node_id: started.node_id,
                    node_kind: started.node_kind,
                    label: started.label,
                    occurrence,
                    call_id: call_id.clone(),
                },
            });
        }
        call_id
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
        let commands = self.commands()?;
        let command = commands.issue()?;
        if let Some(checked) =
            lash_lashlang_runtime::typescript_runtime_operation(&receiver, &operation, &args)
        {
            let runtime_operation = match checked {
                Ok(runtime_operation) => runtime_operation,
                Err(error) => {
                    commands.skipped(&command)?;
                    return Err(error);
                }
            };
            let in_flight = commands.enter(command, CommandShape::Value).await?;
            let value = lash_lashlang_runtime::journaled_typescript_runtime_value(
                &in_flight.ctx,
                in_flight.command.key.as_str().to_string(),
                runtime_operation,
            )
            .await;
            commands.finish(&in_flight)?;
            return match value {
                Ok(value) => value,
                Err(error) => Err(commands.journal_error(&in_flight, error, |error| {
                    ExecutionHostError::new(error.to_string())
                })),
            };
        }
        let prepared = async {
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
            let payload = operation_payload(&args).await?;
            Ok((host_operation, source_operation, payload))
        }
        .await;
        let (host_operation, source_operation, payload) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        let site = match Self::require_call_site(&operation, &host_operation, call_site.as_ref()) {
            Ok(site) => site.clone(),
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        let index = self.next_index();
        let call_id = self.resource_tool_call_id(command.ordinal, &site, None)?;
        if let Some(trigger_operation) =
            lashlang::TriggerHostOperation::from_host_operation(&host_operation)
        {
            let in_flight = commands.enter(command, CommandShape::Value).await?;
            let result = lash_lashlang_runtime::execute_trigger_operation(
                &in_flight.ctx,
                self.artifact_store.as_ref(),
                trigger_operation,
                payload,
                in_flight.command.key.as_str().to_string(),
            )
            .await;
            commands.finish(&in_flight)?;
            let outcome = if result.is_ok() {
                lash_core::ExecutedCallOutcome::Ok
            } else {
                lash_core::ExecutedCallOutcome::Err
            };
            self.record_executed_call(index, source_operation, outcome, None)?;
            return result;
        }
        let mut invocation = self.tool_invocation(
            call_id.clone(),
            &host_operation,
            payload,
            call_site.as_ref(),
        );
        // A call on a drifted binding replays under its recorded binding and
        // is served only from the journal (FIG-3587).
        let drift = self
            .cell_bindings
            .drift_for(&invocation.tool_id)
            .map(|drift| {
                invocation = invocation
                    .clone()
                    .with_recorded_binding(drift.recorded_binding());
                drift.refusal()
            });
        let in_flight = commands
            .enter_bound(command, CommandShape::ToolCall, drift)
            .await?;
        let reply = Box::pin(
            in_flight
                .ctx
                .call_command_tool(&in_flight.command.key, invocation),
        )
        .await;
        commands.finish(&in_flight)?;
        // Invocation replies are terminal: pending calls are resolved before
        // this path. Exhaustiveness keeps a future terminal outcome honest.
        let outcome = match &reply.output.outcome {
            lash_core::ToolCallOutcome::Success(_) => lash_core::ExecutedCallOutcome::Ok,
            lash_core::ToolCallOutcome::Failure(_) | lash_core::ToolCallOutcome::Cancelled(_) => {
                lash_core::ExecutedCallOutcome::Err
            }
        };
        let (result, host_record) = self.consume_reply(reply, in_flight.command.key.as_str());
        self.record_executed_call(index, source_operation, outcome, host_record)?;
        result
    }

    async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> Result<lashlang::ResourceOperationBatchResult, ExecutionHostError> {
        let lashlang::ResourceOperationBatch {
            leaves,
            consumer,
            settled_value_after,
        } = batch;
        // One whole aggregate is one command: its leaves are keyed under the
        // command by their first-appearance index, never by their own sites.
        let commands = self.commands()?;
        let command = commands.issue()?;
        // An aggregate's leaves re-drive through the tool child host, which
        // resolves each tool live: one naming a drifted binding cannot be
        // served from the recorded binding, so the aggregate refuses before
        // anything is dispatched (FIG-3587).
        if let Some(drift) = self.aggregate_drift(&leaves) {
            return Err(commands.stop(drift));
        }
        let in_flight = commands.enter(command, CommandShape::Aggregate).await?;
        let mut bridge_leaves = Vec::with_capacity(leaves.len());
        // Per dispatched leaf: its call site, source operation, executed-call
        // index and call id, keyed by leaf index.
        let mut dispatched: std::collections::BTreeMap<
            usize,
            (
                Option<lashlang::LashlangExecutionCallSite>,
                String,
                usize,
                String,
            ),
        > = std::collections::BTreeMap::new();

        for (leaf_index, leaf) in leaves.into_iter().enumerate() {
            let operation = match leaf {
                lashlang::ResourceOperationBatchLeaf::Operation(operation) => operation,
                lashlang::ResourceOperationBatchLeaf::Timer(sleep) => {
                    bridge_leaves.push(match lash_lashlang_runtime::timer_duration_ms(&sleep) {
                        Ok(duration_ms) => {
                            lash_lashlang_runtime::BridgeAggregateLeaf::Timer { duration_ms }
                        }
                        Err(error) => {
                            lash_lashlang_runtime::BridgeAggregateLeaf::Settled(Err(error))
                        }
                    });
                    continue;
                }
            };
            let lashlang::ResourceOperation {
                operation,
                receiver,
                args,
                call_site,
            } = operation;
            if let Some(checked) =
                lash_lashlang_runtime::typescript_runtime_operation(&receiver, &operation, &args)
            {
                let result = match checked {
                    Ok(runtime_operation) => {
                        match lash_lashlang_runtime::journaled_typescript_runtime_value(
                            &in_flight.ctx,
                            format!(
                                "{}:{}",
                                in_flight.command.key,
                                lash_core::CommandReplayKey::child_suffix(leaf_index)
                            ),
                            runtime_operation,
                        )
                        .await
                        {
                            Ok(value) => value,
                            Err(error) => Err(commands.journal_error(&in_flight, error, |error| {
                                ExecutionHostError::new(error.to_string())
                            })),
                        }
                    }
                    Err(error) => Err(error),
                };
                bridge_leaves.push(lash_lashlang_runtime::BridgeAggregateLeaf::Settled(result));
                continue;
            }
            let prepared = async {
                let receiver = match &receiver {
                    FlowValue::Resource(receiver) => receiver,
                    _ => {
                        return Err(ExecutionHostError::new(format!(
                            "module operation `{operation}` requires a module authority receiver"
                        )));
                    }
                };
                let host_operation = resolve_lashlang_module_operation(
                    &self.host_environment,
                    receiver,
                    &operation,
                )?;
                let source_operation = format!("{}.{}", receiver.alias, operation);
                let payload = operation_payload(&args).await?;
                Ok::<_, ExecutionHostError>((host_operation, source_operation, payload))
            }
            .await;
            let (host_operation, source_operation, payload) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    bridge_leaves.push(lash_lashlang_runtime::BridgeAggregateLeaf::Settled(Err(
                        error,
                    )));
                    continue;
                }
            };
            let site =
                match Self::require_call_site(&operation, &host_operation, call_site.as_ref()) {
                    Ok(site) => site.clone(),
                    Err(error) => {
                        bridge_leaves.push(lash_lashlang_runtime::BridgeAggregateLeaf::Settled(
                            Err(error),
                        ));
                        continue;
                    }
                };
            let execution_index = self.next_index();
            let call_id =
                self.resource_tool_call_id(in_flight.command.ordinal, &site, Some(leaf_index))?;
            if let Some(trigger_operation) =
                lashlang::TriggerHostOperation::from_host_operation(&host_operation)
            {
                let result = lash_lashlang_runtime::execute_trigger_operation(
                    &in_flight.ctx,
                    self.artifact_store.as_ref(),
                    trigger_operation,
                    payload,
                    format!(
                        "{}:{}",
                        in_flight.command.key,
                        lash_core::CommandReplayKey::child_suffix(leaf_index)
                    ),
                )
                .await;
                let outcome = if result.is_ok() {
                    lash_core::ExecutedCallOutcome::Ok
                } else {
                    lash_core::ExecutedCallOutcome::Err
                };
                let result = self
                    .record_executed_call(execution_index, source_operation, outcome, None)
                    .and(result);
                bridge_leaves.push(lash_lashlang_runtime::BridgeAggregateLeaf::Settled(result));
                continue;
            }
            let invocation = self.tool_invocation(
                call_id.clone(),
                &host_operation,
                payload,
                call_site.as_ref(),
            );
            dispatched.insert(
                leaf_index,
                (
                    call_site,
                    source_operation,
                    execution_index,
                    format!(
                        "{}:{}",
                        in_flight.command.key,
                        lash_core::CommandReplayKey::child_suffix(leaf_index)
                    ),
                ),
            );
            bridge_leaves.push(lash_lashlang_runtime::BridgeAggregateLeaf::Tool(invocation));
        }

        if let Some(trace) = &self.lashlang_execution_trace
            && dispatched.len() > 1
        {
            for (position, (call_site, ..)) in dispatched.values().enumerate() {
                if let Some(call_site) = call_site {
                    trace.emit_waiting(
                        call_site,
                        lash_lashlang_runtime::TraceNodeAwaited::ToolBatch {
                            batch_id: in_flight.command.key.as_str().to_string(),
                            position,
                        },
                    );
                }
            }
        }

        let reply = lash_lashlang_runtime::settle_bridge_aggregate(
            &in_flight.ctx,
            &in_flight.command.key,
            consumer,
            settled_value_after,
            bridge_leaves,
            |leaf, reply| {
                let Some((_, source_operation, execution_index, replay_key)) =
                    dispatched.get(&leaf)
                else {
                    return Err(ExecutionHostError::new(format!(
                        "aggregate leaf {leaf} was answered as a tool call it never dispatched"
                    )));
                };
                // Batch replies are terminal for the same reason as scalar replies.
                let outcome = match &reply.output.outcome {
                    lash_core::ToolCallOutcome::Success(_) => lash_core::ExecutedCallOutcome::Ok,
                    lash_core::ToolCallOutcome::Failure(_)
                    | lash_core::ToolCallOutcome::Cancelled(_) => {
                        lash_core::ExecutedCallOutcome::Err
                    }
                };
                let (result, host_record) = self.consume_reply(reply, replay_key);
                self.record_executed_call(
                    *execution_index,
                    source_operation.clone(),
                    outcome,
                    host_record,
                )
                .and(result)
            },
        )
        .await;
        commands.finish(&in_flight)?;
        if !self.is_cancelled()
            && dispatched.len() > 1
            && let Some(trace) = &self.lashlang_execution_trace
        {
            for (call_site, ..) in dispatched.values() {
                if let Some(call_site) = call_site {
                    trace.emit_resumed(
                        call_site,
                        lash_lashlang_runtime::TraceNodeWaitResolution::Resumed,
                    );
                }
            }
        }
        reply
    }

    async fn await_handle(&self, handle: FlowValue) -> Result<FlowValue, ExecutionHostError> {
        let commands = self.commands()?;
        let command = commands.issue()?;
        let handle = match handle_to_json(&handle).await {
            Ok(handle) => handle,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        let index = self.next_index();
        let call_id = self.cell()?.identities().call_id(command.ordinal);
        let in_flight = commands.enter(command, CommandShape::AwaitHandle).await?;
        // The await's process command journals under the command's key, as
        // a child of the command's own invocation.
        let command_ctx = in_flight.ctx.under_command(&in_flight.command.key);
        let reply = {
            let _phase = self.ctx.named_phase("rlm_process.await_handle");
            command_ctx.await_tool_handle(call_id.clone(), handle).await
        };
        commands.finish(&in_flight)?;
        self.consume_recorded_reply(index, "await_handle", reply, &call_id)
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
        let commands = self.commands()?;
        let command = commands.issue()?;
        let call_site = sleep.call_site;
        let spec = match process_sleep(sleep.kind, &sleep.value) {
            Ok(spec) => spec,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        let in_flight = commands.enter(command, CommandShape::Sleep).await?;
        if let Some(trace) = &self.lashlang_execution_trace
            && let Some(call_site) = &call_site
        {
            trace.emit_waiting(
                call_site,
                lash_lashlang_runtime::TraceNodeAwaited::Sleep {
                    deadline_ms: match spec {
                        lash_core::SleepSpec::Until { deadline_ms } => Some(deadline_ms),
                        lash_core::SleepSpec::For { .. } => None,
                    },
                },
            );
        }
        let slept = in_flight
            .ctx
            .sleep_command(&in_flight.command.key, spec)
            .await;
        commands.finish(&in_flight)?;
        slept.map_err(|error| {
            commands.journal_error(&in_flight, error, |error| {
                ExecutionHostError::new(error.to_string())
            })
        })?;
        if !self.is_cancelled()
            && let Some(trace) = &self.lashlang_execution_trace
            && let Some(call_site) = &call_site
        {
            trace.emit_resumed(
                call_site,
                lash_lashlang_runtime::TraceNodeWaitResolution::TimedOut,
            );
        }
        Ok(FlowValue::Null)
    }

    /// An ability a cell may not use: it holds its ordinal — the recorded run
    /// held one there too — and is refused before it reaches the host.
    fn refused_in_cell(&self, message: &'static str) -> Result<AbilityResult, ExecutionHostError> {
        let commands = self.commands()?;
        let command = commands.issue()?;
        commands.skipped(&command)?;
        Err(ExecutionHostError::new(message))
    }

    fn perform_selected_ability<'a>(&'a self, op: AbilityOp) -> HostAbilityFuture<'a> {
        match op {
            AbilityOp::ResourceOperation(operation) => Box::pin(async move {
                let lashlang::ResourceOperation {
                    operation,
                    receiver,
                    args,
                    call_site,
                } = *operation;
                Box::pin(self.resource_operation(operation, receiver, args, call_site))
                    .await
                    .map(AbilityResult::Value)
            }),
            AbilityOp::ResourceOperationBatch(batch) => Box::pin(async move {
                self.resource_operation_batch(batch)
                    .await
                    .map(AbilityResult::ResourceOperationBatch)
            }),
            AbilityOp::Await(handle) => {
                Box::pin(async move { self.await_handle(handle).await.map(AbilityResult::Value) })
            }
            AbilityOp::Print(value) => Box::pin(async move {
                self.print(value).await?;
                Ok(AbilityResult::Unit)
            }),
            AbilityOp::Sleep(sleep) => {
                Box::pin(async move { self.sleep(sleep).await.map(AbilityResult::Value) })
            }
            AbilityOp::ProcessEvent(_) => Box::pin(async {
                self.refused_in_cell(
                    "process events are only available inside lashlang process bodies",
                )
            }),
            AbilityOp::WaitSignal { .. } => Box::pin(async {
                self.refused_in_cell(
                    "`wait_signal` is only available inside lashlang process bodies",
                )
            }),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                Box::pin(async move { Ok(AbilityResult::Value(value)) })
            }
        }
    }
}

/// A module operation's payload: its one record argument as an object, or
/// its positional arguments under `args`.
async fn operation_payload(args: &[FlowValue]) -> Result<Value, ExecutionHostError> {
    let mut payload = if let [FlowValue::Record(record)] = args {
        flow_record_json(record).await
    } else {
        serde_json::json!({
            "args": flow_values_to_json(args).await,
        })
    };
    payload
        .as_object_mut()
        .ok_or_else(|| ExecutionHostError::new("module operation payload must be an object"))?;
    Ok(payload)
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
        self.ctx.is_cancelled() || self.cancellation.is_cancelled()
    }

    fn observes_lashlang_execution(&self) -> bool {
        self.lashlang_execution_trace.is_some()
    }

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        let Some(trace) = &self.lashlang_execution_trace else {
            return;
        };
        let observation = match observation {
            lashlang::LashlangExecutionObservation::NodeFailed {
                site, occurrence, ..
            } if self.is_cancelled()
                && trace.active_nodes.lock_recover().contains(&(
                    site.node_id.clone(),
                    site.node_kind,
                    occurrence,
                )) =>
            {
                trace.emit_cancelled_site(site, occurrence);
                return;
            }
            lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence }
                if self.is_cancelled()
                    && trace.waiting_nodes.is_waiting(
                        &site.node_id,
                        site.node_kind,
                        occurrence,
                    ) =>
            {
                trace.emit_cancelled_site(site, occurrence);
                return;
            }
            observation => observation,
        };
        match &observation {
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence } => {
                trace.active_nodes.lock_recover().insert((
                    site.node_id.clone(),
                    site.node_kind,
                    *occurrence,
                ));
            }
            lashlang::LashlangExecutionObservation::ChildProcessWaiting {
                site,
                occurrence,
                ..
            } => {
                trace
                    .waiting_nodes
                    .mark_waiting(&site.node_id, site.node_kind, *occurrence);
            }
            lashlang::LashlangExecutionObservation::NodeResumed { site, occurrence }
            | lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence }
            | lashlang::LashlangExecutionObservation::NodeFailed {
                site, occurrence, ..
            } => {
                trace
                    .waiting_nodes
                    .finish(&site.node_id, site.node_kind, *occurrence);
            }
            _ => {}
        }
        if let lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence }
        | lashlang::LashlangExecutionObservation::NodeFailed {
            site, occurrence, ..
        } = &observation
        {
            trace.active_nodes.lock_recover().remove(&(
                site.node_id.clone(),
                site.node_kind,
                *occurrence,
            ));
        }
        let (suffix, payload) = match observation {
            lashlang::LashlangExecutionObservation::ChildProcessWaiting {
                site,
                occurrence,
                process_ids,
            } => (
                format!("node:{}:{occurrence}:waiting", site.node_id),
                TraceLanguageExecutionPayload::NodeWaiting {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                    awaited: lash_lashlang_runtime::TraceNodeAwaited::ChildProcesses {
                        process_ids,
                    },
                },
            ),
            lashlang::LashlangExecutionObservation::NodeResumed { site, occurrence } => (
                format!("node:{}:{occurrence}:resumed", site.node_id),
                TraceLanguageExecutionPayload::NodeResumed {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                    resolution: lash_lashlang_runtime::TraceNodeWaitResolution::Resumed,
                },
            ),
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence }
                if site.node_kind == lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND =>
            {
                trace
                    .pending_resource_starts
                    .lock_recover()
                    .insert((site.node_id.clone(), occurrence), site);
                return;
            }
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence } => (
                format!("node:{}:{occurrence}:started", site.node_id),
                TraceLanguageExecutionPayload::NodeStarted {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                    call_id: None,
                },
            ),
            lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence } => {
                let call_id = trace.finish_resource_call(&site, occurrence);
                (
                    format!("node:{}:{occurrence}:completed", site.node_id),
                    TraceLanguageExecutionPayload::NodeCompleted {
                        node_id: site.node_id,
                        node_kind: site.node_kind,
                        label: site.label,
                        occurrence,
                        call_id,
                    },
                )
            }
            lashlang::LashlangExecutionObservation::NodeFailed {
                site,
                occurrence,
                failure,
            } => {
                let call_id = trace.finish_resource_call(&site, occurrence);
                (
                    format!("node:{}:{occurrence}:failed", site.node_id),
                    TraceLanguageExecutionPayload::NodeFailed {
                        node_id: site.node_id,
                        node_kind: site.node_kind,
                        label: site.label,
                        occurrence,
                        call_id,
                        failure: lash_lashlang_runtime::trace_failure(failure),
                    },
                )
            }
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
                        process_id: child.process_id,
                        incarnation: child.incarnation,
                        attempt: child.attempt,
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

fn language_event_node_id(payload: &TraceLanguageExecutionPayload) -> Option<&str> {
    match payload {
        TraceLanguageExecutionPayload::NodeStarted { node_id, .. }
        | TraceLanguageExecutionPayload::NodeWaiting { node_id, .. }
        | TraceLanguageExecutionPayload::NodeResumed { node_id, .. }
        | TraceLanguageExecutionPayload::NodeCancelled { node_id, .. }
        | TraceLanguageExecutionPayload::NodeCompleted { node_id, .. }
        | TraceLanguageExecutionPayload::NodeFailed { node_id, .. }
        | TraceLanguageExecutionPayload::BranchSelected { node_id, .. } => Some(node_id),
        TraceLanguageExecutionPayload::ChildStarted { parent_node_id, .. } => Some(parent_node_id),
        TraceLanguageExecutionPayload::ExecutionStarted { .. }
        | TraceLanguageExecutionPayload::ExecutionFinished { .. } => None,
    }
}

async fn handle_to_json(value: &FlowValue) -> Result<Value, ExecutionHostError> {
    match value {
        FlowValue::Projected(_) => Ok(flow_to_json_value(value).await),
        _ => lashlang_value_to_json(value),
    }
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
    pub(super) calls: Vec<lash_core::ExecutedCall>,
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
                // A projection restored without its host descriptor has no value
                // to scan; it carries no printed image either (FIG-2865).
                if let Ok(value) = value.materialize_async().await {
                    collect_printed_images_inner(&value, attachment_store, seen, images).await?;
                }
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
