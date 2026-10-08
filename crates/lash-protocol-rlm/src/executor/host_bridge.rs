use lash_sansio::sync::{LockResultExt, MutexExt};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use lash_core::{
    AttachmentRef, Observation, RuntimeExecutionContext, ToolExecutionGrant, TraceEvent,
    facade_support::ToolInvocation, facade_support::ToolInvocationReply,
    facade_support::TraceBranchSelection, facade_support::TraceRuntimeSubject,
};
use lash_lashlang_runtime::{
    CommandShape, ExecutionCancellation, TraceLanguageChildExecution, TraceLanguageExecution,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, lashlang_value_to_json,
    process_sleep, protocol_tool_output_to_lashlang_value,
};
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, Record as FlowRecord, Sleep,
    Value as FlowValue,
};
use serde_json::Value;

use super::cell_run::{CellRun, LashlangCellOpener};
use crate::projection::flow_to_json_value;

mod resource_operations;

pub(super) struct HostBridge<'run> {
    ctx: RuntimeExecutionContext<'run>,
    /// The cell's replay run — the identities it mints and its command
    /// keys — or the reason this execution has no logical opener to mint
    /// under.
    cell: Arc<Result<CellRun, LashlangCellOpener>>,
    prints: Arc<Mutex<Vec<FlowValue>>>,
    printed_images: Mutex<Vec<AttachmentRef>>,
    calls: Mutex<Vec<LedgerCall>>,
    next_tool_index: Mutex<usize>,
    lashlang_execution_trace: Option<LashlangExecutionTrace>,
    host_environment: lashlang::LashlangHostEnvironment,
    deferred_execution_grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    /// The cell's journaled binding set, against the live registry (FIG-3587).
    cell_bindings: lash_lashlang_runtime::CellToolBindings,
    /// This cell's own cancellation scope, beside the turn's. A cancelled tool
    /// call ends the cell here, so `is_cancelled` refuses its next effect
    /// instead of the guest catching the cancellation as a rejected call. A
    /// replay divergence ends it the same way (FIG-3586).
    cancellation: ExecutionCancellation,
    /// The admitted operation the cell's broker is performing: the ordinal
    /// its commands are named by, the cell's one ordinal authority, and the
    /// waits its quiet point pinned.
    performing: lash_lashlang_runtime::PerformingGate,
    /// The cell's admitted calls, by call: what their bodies run.
    members: Arc<lash_core::tool_dispatch::CellMembers>,
    /// The cell's snapshots, which drive its admitted calls.
    snapshots: Arc<lash_vm_broker::DurableSnapshotStore>,
    /// The tool calls the cell admitted, counted against `max_tool_calls`.
    tool_calls: Mutex<usize>,
}

/// One dispatch the cell executed: where it ran in execution order, its
/// entry, and the host tool call's record when the dispatch made one.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct LedgerCall {
    index: usize,
    call: lash_core::ExecutedCall,
    record: Option<lash_core::ToolCallRecord>,
}

/// The host-side ledgers of a cell that a segment boundary inside it hands
/// to the segment that resumes it (FIG-4739). The prints live beside them,
/// in the list the executor shares with the bridge.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct CellHostLedgers {
    pub printed_images: Vec<AttachmentRef>,
    pub calls: Vec<LedgerCall>,
    pub next_tool_index: usize,
    /// The tool calls the cell's quiet points admitted: what it counts
    /// against `max_tool_calls`.
    pub tool_calls: usize,
}

pub(super) struct HostBridgeConfig<'run> {
    pub ctx: RuntimeExecutionContext<'run>,
    pub cell: Arc<Result<CellRun, LashlangCellOpener>>,
    pub prints: Arc<Mutex<Vec<FlowValue>>>,
    pub lashlang_execution_trace: Option<LashlangExecutionTrace>,
    pub host_environment: lashlang::LashlangHostEnvironment,
    pub deferred_execution_grants: BTreeMap<lash_core::ToolId, ToolExecutionGrant>,
    pub cell_bindings: lash_lashlang_runtime::CellToolBindings,
    /// The ledgers a predecessor segment handed over, for a resumed cell.
    pub ledgers: CellHostLedgers,
    /// The cell's admitted calls.
    pub members: Arc<lash_core::tool_dispatch::CellMembers>,
    /// The cell's snapshots.
    pub snapshots: Arc<lash_vm_broker::DurableSnapshotStore>,
}

type HostAbilityFuture<'a> =
    lash_sansio::future::SendBoxFuture<'a, Result<AbilityOutcome, ExecutionHostError>>;

impl<'run> HostBridge<'run> {
    pub(super) fn new(config: HostBridgeConfig<'run>) -> Self {
        Self {
            cell: config.cell,
            ctx: config.ctx,
            prints: config.prints,
            printed_images: Mutex::new(config.ledgers.printed_images),
            calls: Mutex::new(config.ledgers.calls),
            next_tool_index: Mutex::new(config.ledgers.next_tool_index),
            tool_calls: Mutex::new(config.ledgers.tool_calls),
            members: config.members,
            snapshots: config.snapshots,
            lashlang_execution_trace: config.lashlang_execution_trace,
            host_environment: config.host_environment,
            deferred_execution_grants: config.deferred_execution_grants,
            cell_bindings: config.cell_bindings,
            cancellation: ExecutionCancellation::new(),
            performing: lash_lashlang_runtime::PerformingGate::new(),
        }
    }

    /// The gate the cell's broker names the operation it performs through.
    pub(super) fn performing_gate(&self) -> &lash_lashlang_runtime::PerformingGate {
        &self.performing
    }

    /// The admitted operation the broker is performing.
    fn performing(&self) -> Result<lash_lashlang_runtime::Performing, ExecutionHostError> {
        self.performing.current().ok_or_else(|| {
            ExecutionHostError::new("a cell's command reached its host outside its admission")
        })
    }

    /// The ledgers a segment boundary inside the cell hands over.
    pub(super) fn ledgers(&self) -> CellHostLedgers {
        CellHostLedgers {
            printed_images: self.printed_images.lock_recover().clone(),
            calls: self.calls.lock_recover().clone(),
            next_tool_index: *self.next_tool_index.lock_recover(),
            tool_calls: *self.tool_calls.lock_recover(),
        }
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
        record: Option<lash_core::ToolCallRecord>,
    ) -> Result<(), ExecutionHostError> {
        // This ledger records dispatches only. Resolution, argument, and other
        // pre-dispatch failures deliberately produce no `Calls:` entry because
        // the source operation did not execute.
        self.calls.lock_recover().push(LedgerCall {
            index,
            call: lash_core::ExecutedCall {
                operation,
                outcome,
                call_id: record.as_ref().map(|record| record.call_id.clone()),
            },
            record,
        });
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
        // Execution-index order, so concurrent dispatch keeps one
        // deterministic order for the entries and for their records.
        let mut ledger = self.calls.into_inner().recover();
        ledger.sort_by_key(|entry| entry.index);
        let mut calls = Vec::with_capacity(ledger.len());
        let mut tool_calls = Vec::new();
        for entry in ledger {
            calls.push(entry.call);
            tool_calls.extend(entry.record);
        }
        CollectedExecutionOutput {
            observations: Vec::new(),
            printed_images: self.printed_images.into_inner().recover(),
            calls,
            tool_calls,
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
        leaf_index: Option<usize>,
    ) -> Result<lash_core::ToolCallId, ExecutionHostError> {
        // The id is the issue ordinal under the cell's scope (FIG-3586); the
        // call site only correlates it with the node on the trace.
        let identities = self.cell()?.identities();
        Ok(match leaf_index {
            Some(leaf_index) => identities.child_call_id(ordinal, leaf_index),
            None => identities.call_id(ordinal),
        })
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

    fn deferred_grant_for_tool_id(
        &self,
        tool_id: &lash_core::ToolId,
    ) -> Option<ToolExecutionGrant> {
        self.deferred_execution_grants.get(tool_id).cloned()
    }

    /// Builds one tool call's invocation: its positional id, the grant a
    /// deferred resolution pinned when the catalog does not carry the tool,
    /// and the issuing node for traces.
    fn tool_invocation(
        &self,
        call_id: lash_core::ToolCallId,
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
        invocation
    }
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::MemberAdmissions for HostBridge<'_> {
    async fn members(
        &self,
        ordinal: u64,
        request: &lash_vm_broker::OperationRequest,
    ) -> Result<Vec<lash_vm_broker::MemberDraft>, String> {
        self.admit_members(ordinal, request).await
    }
}

#[derive(Clone)]
pub(super) struct LashlangExecutionTrace {
    tracing: lash_core::plugin::PluginExecutionTrace,
    /// The dialect of the *source* that ran. The substrate is the Lashlang VM
    /// under both, which is why the event and the file keep their names.
    language: &'static str,
    identity: TraceLanguageExecutionIdentity,
    resource_call_ids: std::sync::Arc<Mutex<BTreeMap<(String, u64), lash_core::ToolCallId>>>,
    pending_resource_starts:
        std::sync::Arc<Mutex<BTreeMap<(String, u64), lashlang::LashlangExecutionSite>>>,
    active_nodes: std::sync::Arc<Mutex<BTreeSet<(String, lash_sansio::ExecutionNodeKind, u64)>>>,
    waiting_nodes: lash_lashlang_runtime::TraceWaitBookkeeping,
}

impl LashlangExecutionTrace {
    pub(super) fn new(
        tracing: lash_core::plugin::PluginExecutionTrace,
        language: &'static str,
        identity: TraceLanguageExecutionIdentity,
    ) -> Self {
        Self {
            tracing,
            language,
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

    pub(super) fn emit(&self, event: TraceLanguageExecution) {
        self.tracing.observe_language(&event.event_key, || {
            let mut context = self.tracing.trace_runtime().base_context().clone();
            context.session_id = self.identity.scope.session_id.clone();
            context.turn_id = self.identity.scope.turn_id.clone();
            context.turn_index = self.identity.scope.turn_index;
            context.protocol_iteration = self.identity.scope.protocol_iteration;
            if let TraceRuntimeSubject::Effect { effect_id, .. } = &self.identity.subject {
                context.effect_id = Some(effect_id.clone());
            }
            context.graph_node_id = language_event_node_id(&event.payload).map(str::to_string);
            (
                context,
                TraceEvent::LanguageExecution {
                    language: self.language.to_string(),
                    event: event.clone(),
                },
            )
        });
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

    fn record_resource_call(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        call_id: &lash_core::ToolCallId,
    ) {
        let key = (call_site.site.node_id.clone(), call_site.occurrence);
        self.resource_call_ids
            .lock_recover()
            .insert(key.clone(), call_id.clone());
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
                    call_id: Some(call_id.clone()),
                },
            });
        }
    }

    fn finish_resource_call(
        &self,
        site: &lashlang::LashlangExecutionSite,
        occurrence: u64,
    ) -> Option<lash_core::ToolCallId> {
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
    pub(super) fn host_environment_description(&self) -> lashlang::LashlangHostEnvironment {
        self.host_environment.clone()
    }
    /// Awaits a handle. An await the cell's run is parked on may be handed to
    /// the Run's successor segment (FIG-4739): the command returns its
    /// ordinal and its call index, nothing of it is recorded, and the run
    /// ends on the state it parked in, which issues the await again.
    async fn await_handle(&self, handle: FlowValue) -> Result<AbilityOutcome, ExecutionHostError> {
        let commands = self.commands()?;
        let command = commands.issue(self.performing()?.ordinal)?;
        let handle = handle_to_json(&handle)?;
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
        if command_ctx.take_wait_handed_over() {
            commands.hand_over(&in_flight)?;
            *self.next_tool_index.lock_recover() = index;
            return Ok(AbilityOutcome::HandedOver);
        }
        commands.finish(&in_flight)?;
        self.consume_recorded_reply(index, "await_handle", reply, call_id.as_str())
            .map(AbilityOutcome::Value)
    }

    async fn print(&self, value: FlowValue) -> Result<(), ExecutionHostError> {
        let attachment_store = self.ctx.attachment_store();
        let mut admitted = self
            .calls
            .lock_recover()
            .iter()
            .filter_map(|entry| entry.record.as_ref())
            .flat_map(|record| record.output.attachments())
            .collect::<Vec<_>>();
        for entry in self.ctx.chronological_projection().entries() {
            match &entry.payload {
                lash_core::facade_support::ChronologicalPayload::Message(message) => {
                    admitted.extend(
                        message
                            .parts
                            .iter()
                            .flat_map(lash_core::Part::attachments)
                            .cloned(),
                    );
                }
                lash_core::facade_support::ChronologicalPayload::ProtocolEvent(event) => {
                    if let Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step)) =
                        crate::projection::decode_rlm_protocol_event(event)
                            .map_err(|error| ExecutionHostError::new(error.to_string()))?
                    {
                        admitted.extend(step.images);
                        admitted.extend(step.output_archive.map(|archive| archive.reference));
                        if let Some(lash_core::OutputValue::Retained(retained)) =
                            step.outcome.terminal_value()
                        {
                            admitted.push(retained.reference.clone());
                        }
                    }
                }
            }
        }
        let images = collect_printed_images(&value, attachment_store.as_ref(), &admitted).await?;
        self.prints.lock_recover().push(value);
        if !images.is_empty() {
            self.printed_images.lock_recover().extend(images);
        }
        Ok(())
    }

    /// Sleeps until the deadline the sleep was admitted with: its quiet
    /// point pinned the timer, and a restored cell waits on that same timer,
    /// so a crash never restarts the sleep's whole duration. Nothing runs
    /// while it sleeps, so once the cell stayed hot for `idle_evict` the
    /// sleep is handed over: the cell suspends on its quiet point, its
    /// session releases as `waiting` until the deadline, and the activation
    /// that claims it then performs the sleep again (ADR 0132 §6).
    async fn sleep(&self, sleep: Sleep) -> Result<AbilityOutcome, ExecutionHostError> {
        let commands = self.commands()?;
        let performing = self.performing()?;
        let command = commands.issue(performing.ordinal)?;
        let call_site = sleep.call_site;
        let spec = process_sleep(sleep.kind, &sleep.value)?;
        let [timer] = performing.waits.as_slice() else {
            return Err(ExecutionHostError::new(
                "a cell's sleep was admitted without its timer",
            ));
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
        let deadline = lash_core::waits::deadline(self.ctx.actor_context(), timer)
            .await
            .map_err(|error| {
                commands.abort(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::EngineAwaitEventAwait,
                    error.to_string(),
                ))
            })?;
        let due = self
            .drive_members(
                &commands,
                performing.ordinal,
                &mut |_, now| match deadline {
                    Some(deadline) if deadline > now => lash_vm_broker::Decide::Wait {
                        until: Some(deadline),
                    },
                    _ => lash_vm_broker::Decide::Answer(()),
                },
            )
            .await?;
        if due.is_none() {
            commands.hand_over(&in_flight)?;
            return Ok(AbilityOutcome::HandedOver);
        }
        // The timer is due: settle its row, or read how it ended.
        let slept = lash_core::waits::sleep_until_timer(&in_flight.ctx, *timer).await;
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
        Ok(AbilityOutcome::Value(FlowValue::Null))
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
                Box::pin(self.resource_operation(operation, receiver, args, call_site)).await
            }),
            AbilityOp::ResourceOperationBatch(batch) => {
                Box::pin(async move { Box::pin(self.resource_operation_batch(batch)).await })
            }
            AbilityOp::Await(handle) => Box::pin(async move { self.await_handle(handle).await }),
            AbilityOp::Print(value) => Box::pin(async move {
                self.print(value).await?;
                Ok(AbilityOutcome::Unit)
            }),
            AbilityOp::Sleep(sleep) => Box::pin(async move { self.sleep(sleep).await }),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                Box::pin(async move { Ok(AbilityOutcome::Value(value)) })
            }
        }
    }
}

/// A module operation's payload: its one record argument as an object, or
/// its positional arguments under `args`.
async fn operation_payload(args: &[FlowValue]) -> Result<Value, ExecutionHostError> {
    let mut payload = if let [FlowValue::Record(record)] = args {
        flow_record_json(record)
    } else {
        serde_json::json!({
            "args": flow_values_to_json(args),
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
    ) -> impl Future<Output = Result<AbilityOutcome, ExecutionHostError>> + Send {
        self.perform_selected_ability(op)
    }

    /// The cell's cancel checkpoint (FIG-3672 P9): a journaled peek of the
    /// turn's cancellation gate, through the cell's context, at an instruction
    /// count a replay reaches again. It is also the cell's wait on a long
    /// stretch of pure compute, so a cancel lands without a scheduler yield.
    /// A peek the controller refuses — a replay divergence among them — ends
    /// the cell like any nested effect it refused.
    async fn cancel_checkpoint(&self, checkpoint: u64) {
        if self.is_cancelled() {
            return;
        }
        if let Err(error) = self.ctx.turn_cancel_checkpoint(checkpoint).await {
            self.ctx.record_nested_effect_error(error);
            self.cancellation.cancel();
        }
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

fn handle_to_json(value: &FlowValue) -> Result<Value, ExecutionHostError> {
    match value {
        FlowValue::Projected(_) => Ok(flow_to_json_value(value)),
        _ => lashlang_value_to_json(value),
    }
}

fn flow_values_to_json(values: &[FlowValue]) -> Vec<Value> {
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        out.push(flow_to_json_value(value));
    }
    out
}

fn flow_record_json(record: &FlowRecord) -> Value {
    let mut object = serde_json::Map::with_capacity(record.len());
    for (key, value) in record.iter() {
        object.insert(key.to_string(), flow_to_json_value(value));
    }
    Value::Object(object)
}

pub(super) struct CollectedExecutionOutput {
    pub(super) observations: Vec<Observation>,
    pub(super) printed_images: Vec<AttachmentRef>,
    pub(super) calls: Vec<lash_core::ExecutedCall>,
    pub(super) tool_calls: Vec<lash_core::ToolCallRecord>,
}

async fn collect_printed_images(
    value: &FlowValue,
    attachment_store: &lash_core::facade_support::RuntimeAttachmentStore,
    admitted: &[AttachmentRef],
) -> Result<Vec<AttachmentRef>, ExecutionHostError> {
    let mut seen = BTreeSet::new();
    let mut images = Vec::new();
    collect_printed_images_inner(value, attachment_store, admitted, &mut seen, &mut images).await?;
    Ok(images)
}

fn collect_printed_images_inner<'a>(
    value: &'a FlowValue,
    attachment_store: &'a lash_core::facade_support::RuntimeAttachmentStore,
    admitted: &'a [AttachmentRef],
    seen: &'a mut BTreeSet<String>,
    images: &'a mut Vec<AttachmentRef>,
) -> lash_sansio::future::SendBoxFuture<'a, Result<(), ExecutionHostError>> {
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
                let reference = admitted
                    .iter()
                    .find(|reference| {
                        reference.id == id
                            && reference.media_type == image.mime
                            && reference.byte_len == image.size
                    })
                    .cloned()
                    .ok_or_else(|| {
                        ExecutionHostError::new(
                            "printed image has no admitted attachment provenance",
                        )
                    })?;
                attachment_store.read(&reference).await.map_err(|_| {
                    ExecutionHostError::new(format!(
                        "image bytes for `{}` are unavailable or do not match its ref",
                        reference.id
                    ))
                })?;
                images.push(reference);
            }
            FlowValue::Tuple(values) | FlowValue::List(values) => {
                for value in values.iter() {
                    collect_printed_images_inner(value, attachment_store, admitted, seen, images)
                        .await?;
                }
            }
            FlowValue::Record(record) => {
                if matches!(record.get("$lash_tool_value"), Some(FlowValue::String(kind)) if kind.as_str() == "attachment")
                    && let Some(reference) = record.get("reference")
                    && let Ok(attachment_ref) =
                        serde_json::from_value::<AttachmentRef>(flow_to_json_value(reference))
                {
                    let adopted = lash_core::ToolValue::untrusted_json(flow_record_json(record))
                        .adopt_attachments(admitted);
                    if !adopted.attachments().contains(&attachment_ref) {
                        return Err(ExecutionHostError::new(
                            "printed attachment has no admitted attachment provenance",
                        ));
                    }
                    if seen.insert(attachment_ref.id.to_string()) {
                        attachment_store.read(&attachment_ref).await.map_err(|_| {
                            ExecutionHostError::new(format!(
                                "attachment bytes for `{}` are unavailable or were pruned",
                                attachment_ref.id
                            ))
                        })?;
                        images.push(attachment_ref);
                    }
                    return Ok(());
                }
                for (_, value) in record.iter() {
                    collect_printed_images_inner(value, attachment_store, admitted, seen, images)
                        .await?;
                }
            }
            FlowValue::Projected(value) => {
                // A projection restored without its host descriptor has no value
                // to scan; it carries no printed image either (FIG-2865).
                if let Ok(value) = value.materialize() {
                    collect_printed_images_inner(&value, attachment_store, admitted, seen, images)
                        .await?;
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

#[cfg(test)]
mod mcp_media_tests {
    use super::*;

    #[tokio::test]
    async fn printing_an_mcp_content_block_attaches_its_stored_media() {
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores");
        let store = lash_core::facade_support::RuntimeAttachmentStore::ephemeral(
            stores.attachment_store(),
            lash_core::facade_support::AttachmentPolicy::standard(),
        );
        let reference = store
            .put(
                b"audio bytes".to_vec(),
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("audio/wav").expect("media type"),
                    None,
                    Some("MCP audio".into()),
                ),
            )
            .await
            .expect("store media");
        let media = lash_core::ToolValue::Attachment(reference.clone());
        let block = crate::projection::json_to_flow_value(serde_json::json!({
            "type": "audio", "mimeType": "audio/wav", "attachment": media.to_json_value()
        }));
        assert!(collect_printed_images(&block, &store, &[]).await.is_err());
        assert_eq!(
            collect_printed_images(&block, &store, std::slice::from_ref(&reference))
                .await
                .expect("print media"),
            vec![reference]
        );
    }
}
