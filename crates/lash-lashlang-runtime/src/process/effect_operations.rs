use super::*;

/// What one run's effect-summary writer carries between run boundaries
/// (FIG-3571): the occurrences it recorded that no boundary has committed yet,
/// and the occurrences it counted past the cap. Both are restored from and
/// snapshotted into segment state, so a successor segment commits its
/// predecessor's pending occurrences with its own first boundary and a
/// redrive re-derives the same counts.
///
/// Recording is in memory: the summary reaches the log only as the prelude of
/// the run's next boundary write (a wait's enter or clear, an event the body
/// appends, or the terminal completion), in that write's own transaction. A
/// boundary write that carried a summary and failed is an incorporation
/// failure, never a program error: the run's scope is cancelled so the guest
/// stops at its next step, and the run reports the failure as infrastructure
/// so a redrive re-derives and re-commits the same summary.
#[derive(Default)]
pub(super) struct EffectSummaryWriter {
    pending: std::sync::Mutex<Vec<lash_core::ProcessEffectSummaryOccurrence>>,
    omissions: std::sync::Mutex<BTreeMap<String, lash_core::ProcessEffectOmittedCounts>>,
    incorporation_fault: std::sync::Mutex<Option<lash_core::PluginError>>,
}

/// The pending occurrences one boundary write carries: `requests` in the
/// order they were recorded, and how many of the writer's pending entries
/// they are, so the writer drops exactly those once the write settles.
pub(super) struct SummaryPrelude {
    pub(super) requests: Vec<lash_core::ProcessEventAppendRequest>,
    flushed: usize,
}

impl EffectSummaryWriter {
    pub(super) fn restore(
        pending: Vec<lash_core::ProcessEffectSummaryOccurrence>,
        omissions: BTreeMap<String, lash_core::ProcessEffectOmittedCounts>,
    ) -> Self {
        Self {
            pending: std::sync::Mutex::new(pending),
            omissions: std::sync::Mutex::new(omissions),
            incorporation_fault: std::sync::Mutex::new(None),
        }
    }

    pub(super) fn take_incorporation_fault(&self) -> Option<lash_core::PluginError> {
        self.incorporation_fault.lock_recover().take()
    }

    pub(super) fn pending(&self) -> Vec<lash_core::ProcessEffectSummaryOccurrence> {
        self.pending.lock_recover().clone()
    }

    pub(super) fn omissions(&self) -> BTreeMap<String, lash_core::ProcessEffectOmittedCounts> {
        self.omissions.lock_recover().clone()
    }

    /// Record one effect occurrence: within the cap it joins the pending
    /// summary, past it it is counted for the run's omission record. Pending
    /// entries are therefore bounded by construction: at most
    /// [`lash_core::PROCESS_EFFECT_OCCURRENCE_CAP`] per effect node, whose
    /// ids are the compiled program's execution sites, each entry spelling
    /// only runtime-minted values (the node id, the program's host operation,
    /// the run's replay key).
    pub(super) fn record(&self, occurrence: lash_core::ProcessEffectSummaryOccurrence) {
        if lash_core::ProcessEffectSummaryOccurrence::is_within_cap(occurrence.occurrence) {
            self.pending.lock_recover().push(occurrence);
        } else {
            self.omissions
                .lock_recover()
                .entry(occurrence.node_id)
                .or_default()
                .record(occurrence.outcome_class);
        }
    }

    /// The pending occurrences, oldest first, as the prelude of the next
    /// boundary write. Nothing is dropped until [`Self::settle`].
    pub(super) fn prelude(&self) -> SummaryPrelude {
        let pending = self.pending.lock_recover();
        SummaryPrelude {
            requests: pending
                .iter()
                .map(lash_core::ProcessEffectSummaryOccurrence::append_request)
                .collect(),
            flushed: pending.len(),
        }
    }

    /// Drop the occurrences a committed boundary write carried. A write that
    /// failed leaves them pending for the next boundary.
    pub(super) fn settle(&self, prelude: &SummaryPrelude) {
        let mut pending = self.pending.lock_recover();
        let flushed = prelude.flushed.min(pending.len());
        pending.drain(..flushed);
    }

    /// The run's terminal batch: every pending occurrence, then the omission
    /// record when any occurrence went uncounted one by one. `fleet_format`
    /// stamps the omission payload's version (FIG-3796).
    pub(super) fn terminal_prelude(
        &self,
        omissions_key: String,
        fleet_format: lash_core::FleetFormat,
    ) -> Vec<lash_core::ProcessEventAppendRequest> {
        let mut prelude = self.prelude().requests;
        let omissions = self.omissions();
        if !omissions.is_empty() {
            prelude.push(
                lash_core::ProcessEffectOmissions::new(omissions, fleet_format)
                    .append_request(omissions_key),
            );
        }
        prelude
    }
}

impl LashlangProcessHost<'_> {
    /// Settle the boundary write that carried `prelude`: a committed write
    /// drops the occurrences it carried; a failed one that carried any is an
    /// incorporation failure (see [`EffectSummaryWriter`]).
    pub(super) fn settle_boundary<E: std::fmt::Display>(
        &self,
        prelude: &SummaryPrelude,
        written: &Result<(), E>,
    ) {
        match written {
            Ok(()) => self.effect_summary.settle(prelude),
            Err(error) if !prelude.requests.is_empty() => {
                self.effect_summary
                    .incorporation_fault
                    .lock_recover()
                    .get_or_insert_with(|| {
                        lash_core::PluginError::Session(format!(
                            "a boundary write carrying the process's effect summary failed: {error}"
                        ))
                    });
                self.cancellation.cancel();
            }
            Err(_) => {}
        }
    }

    /// Incorporates one recorded effect outcome into the durable summary.
    ///
    /// An occurrence within the cap joins the run's pending summary under the
    /// effect's replay key, and commits with the run's next boundary; a later
    /// one is counted for the run's omission record.
    pub(super) fn record_effect_outcome(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        operation: &str,
        outcome_class: lash_core::ProcessEffectOutcomeClass,
        code: Option<lash_core::FailureCode>,
        replay_key: &str,
    ) {
        self.effect_summary
            .record(lash_core::ProcessEffectSummaryOccurrence::new(
                call_site.site.node_id.clone(),
                call_site.occurrence,
                operation,
                outcome_class,
                code,
                replay_key,
                self.ctx.fleet_format(),
            ));
    }

    pub(super) fn record_tool_reply(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        host_operation: &str,
        replay_key: &str,
        reply: &lash_core::facade_support::ToolInvocationReply,
    ) {
        let Some(record) = &reply.record else {
            return;
        };
        let (outcome_class, code) = match &record.output.outcome {
            lash_core::ToolCallOutcome::Success(_) => {
                (lash_core::ProcessEffectOutcomeClass::Success, None)
            }
            lash_core::ToolCallOutcome::Failure(failure) => (
                lash_core::ProcessEffectOutcomeClass::Failure,
                Some(lash_core::tool_failure_code(failure)),
            ),
            lash_core::ToolCallOutcome::Cancelled(_) => {
                (lash_core::ProcessEffectOutcomeClass::Cancelled, None)
            }
        };
        self.record_effect_outcome(call_site, host_operation, outcome_class, code, replay_key);
    }

    /// Journals one checked TypeScript runtime value at `key` under the
    /// command in flight, and incorporates its outcome into the durable
    /// effect summary when the call site names the node it belongs to.
    pub(super) async fn typescript_runtime_value(
        &self,
        in_flight: &crate::CommandInFlight<'_>,
        operation: &str,
        call_site: Option<&lashlang::LashlangExecutionCallSite>,
        key: String,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let host_operation = crate::typescript_runtime::TYPESCRIPT_RUNTIME_HOST_OPERATION;
        match crate::journaled_typescript_runtime_value(&in_flight.ctx, key.clone(), operation)
            .await
        {
            Ok(value) => {
                if let Some(call_site) = call_site {
                    self.record_effect_outcome(
                        call_site,
                        host_operation,
                        lash_core::ProcessEffectOutcomeClass::Success,
                        None,
                        &key,
                    );
                }
                value
            }
            Err(error) => Err(self.commands().journal_error(in_flight, error, |error| {
                ExecutionHostError::new(error.to_string())
            })),
        }
    }

    pub(super) async fn trigger_operation(
        &self,
        ctx: &lash_core::RuntimeExecutionContext<'_>,
        operation: lashlang::TriggerHostOperation,
        payload: serde_json::Value,
        effect_id: String,
        host_operation: &str,
        call_site: Option<&lashlang::LashlangExecutionCallSite>,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let mut recorded = None;
        let result = crate::trigger_commands::execute_trigger_operation_recording(
            ctx,
            &self.artifact_store,
            operation,
            payload,
            effect_id.clone(),
            &mut recorded,
        )
        .await;
        if let (Some((outcome_class, code)), Some(call_site)) = (recorded, call_site) {
            self.record_effect_outcome(call_site, host_operation, outcome_class, code, &effect_id);
        }
        result
    }

    /// One aggregate of this process's pending operations: one command, its
    /// leaves keyed under it by first-appearance index (FIG-3586). Every leaf
    /// the bridge settles itself — a TypeScript runtime value, a trigger
    /// operation, a leaf refused before dispatch — joins the immediate prefix,
    /// every tool call and timer is admitted as a group child, and the whole
    /// is answered in the VM's reply algebra (ADR 0099 §10, §11). Each tool
    /// reply the answer carries is incorporated into the durable effect
    /// summary as it is read (FIG-3464); a loser's reply is never read here.
    pub(super) async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> Result<lashlang::ResourceOperationBatchResult, ExecutionHostError> {
        let lashlang::ResourceOperationBatch {
            leaves,
            consumer,
            settled_value_after,
        } = batch;
        let commands = self.commands();
        let command = commands.issue()?;
        let in_flight = commands
            .enter(command, crate::CommandShape::Aggregate)
            .await?;
        let leaf_key = |leaf: usize| {
            format!(
                "{}:{}",
                in_flight.command.key,
                lash_core::CommandReplayKey::child_suffix(leaf)
            )
        };
        let call_sites = leaves
            .iter()
            .map(|leaf| match leaf {
                lashlang::ResourceOperationBatchLeaf::Operation(operation) => {
                    operation.call_site.clone()
                }
                lashlang::ResourceOperationBatchLeaf::Timer(sleep) => sleep.call_site.clone(),
            })
            .collect::<Vec<_>>();
        let mut bridge_leaves = Vec::with_capacity(leaves.len());
        let mut outcome_metadata: Vec<
            Option<(String, Option<lashlang::LashlangExecutionCallSite>, String)>,
        > = vec![None; leaves.len()];
        let mut dispatched = Vec::new();
        for (index, leaf) in leaves.into_iter().enumerate() {
            let operation = match leaf {
                lashlang::ResourceOperationBatchLeaf::Operation(operation) => operation,
                lashlang::ResourceOperationBatchLeaf::Timer(sleep) => {
                    bridge_leaves.push(match crate::timer_duration_ms(&sleep) {
                        Ok(duration_ms) => crate::BridgeAggregateLeaf::Timer { duration_ms },
                        Err(error) => crate::BridgeAggregateLeaf::Settled(Err(error)),
                    });
                    continue;
                }
            };
            if let Some(checked) = crate::typescript_runtime_operation(
                &operation.receiver,
                &operation.operation,
                &operation.args,
            ) {
                let result = match checked {
                    Ok(runtime_operation) => {
                        self.typescript_runtime_value(
                            &in_flight,
                            runtime_operation,
                            operation.call_site.as_ref(),
                            leaf_key(index),
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                bridge_leaves.push(crate::BridgeAggregateLeaf::Settled(result));
                continue;
            }
            match self.prepare_resource_invocation(
                operation.operation,
                operation.receiver,
                operation.args,
                operation.call_site,
                self.identities
                    .child_call_id(in_flight.command.ordinal, index),
                leaf_key(index),
            ) {
                Ok(PreparedResourceInvocation::Trigger {
                    operation,
                    payload,
                    effect_id,
                    host_operation,
                    call_site,
                }) => {
                    let result = self
                        .trigger_operation(
                            &in_flight.ctx,
                            operation,
                            payload,
                            effect_id,
                            &host_operation,
                            call_site.as_ref(),
                        )
                        .await;
                    bridge_leaves.push(crate::BridgeAggregateLeaf::Settled(result));
                }
                Ok(PreparedResourceInvocation::Tool {
                    invocation,
                    host_operation,
                    call_site,
                }) => {
                    outcome_metadata[index] = Some((host_operation, call_site, leaf_key(index)));
                    dispatched.push(index);
                    bridge_leaves.push(crate::BridgeAggregateLeaf::Tool(invocation));
                }
                Err(error) => bridge_leaves.push(crate::BridgeAggregateLeaf::Settled(Err(error))),
            }
        }

        if dispatched.len() > 1 {
            for (position, index) in dispatched.iter().copied().enumerate() {
                if let Some(Some(call_site)) = call_sites.get(index) {
                    self.lashlang_execution_trace.emit_waiting(
                        call_site,
                        TraceNodeAwaited::ToolBatch {
                            batch_id: in_flight.command.key.as_str().to_string(),
                            position,
                        },
                    );
                }
            }
        }
        // The replies the answer carries, in the order it read them, for the
        // effect summary: a loser's reply is never among them.
        let mut read = Vec::new();
        let reply = crate::settle_bridge_aggregate(
            &in_flight.ctx,
            &in_flight.command.key,
            consumer,
            settled_value_after,
            bridge_leaves,
            |leaf, reply| {
                let Some((_, _, replay_key)) = &outcome_metadata[leaf] else {
                    return Err(ExecutionHostError::new(format!(
                        "aggregate leaf {leaf} was answered with a tool reply it never dispatched"
                    )));
                };
                read.push((leaf, reply.clone()));
                protocol_tool_reply_to_lashlang_value(reply, replay_key, &self.cancellation)
            },
        )
        .await;
        commands.finish(&in_flight)?;
        for (leaf, tool_reply) in &read {
            if let Some((host_operation, Some(call_site), replay_key)) = &outcome_metadata[*leaf] {
                self.record_tool_reply(call_site, host_operation, replay_key, tool_reply);
            }
        }
        if !self.cancellation.is_cancelled() && dispatched.len() > 1 {
            for index in dispatched.iter().copied() {
                if let Some(Some(call_site)) = call_sites.get(index) {
                    self.lashlang_execution_trace
                        .emit_resumed(call_site, TraceNodeWaitResolution::Resumed);
                }
            }
        }
        reply
    }
}
