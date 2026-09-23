use super::*;

impl LashlangProcessExecutionTrace {
    pub(super) fn new(
        sink: Option<Arc<dyn TraceSink>>,
        base_context: TraceContext,
        identity: LashlangProcessTraceIdentity,
    ) -> Self {
        Self {
            sink,
            base_context,
            session_id: identity.session_id,
            process_id: identity.process_id,
            source_identity: identity.source_identity,
            module_ref: identity.module_ref,
            process_ref: identity.process_ref,
            process_name: identity.process_name,
            attempt: identity.attempt,
            incarnation: identity.incarnation,
            restate_invocation_id: identity.restate_invocation_id,
            resource_call_ids: Arc::default(),
            pending_resource_starts: Arc::default(),
            active_nodes: Arc::default(),
            waiting_nodes: crate::TraceWaitBookkeeping::default(),
            execution_map: None,
        }
    }

    pub(super) fn scope(&self) -> TraceRuntimeScope {
        TraceRuntimeScope {
            session_id: self.session_id.clone(),
            turn_id: None,
            turn_index: None,
            protocol_iteration: None,
        }
    }

    pub(super) fn identity(&self) -> TraceLanguageExecutionIdentity {
        TraceLanguageExecutionIdentity {
            scope: self.scope(),
            subject: TraceRuntimeSubject::Process {
                process_id: self.process_id.clone(),
            },
            source_identity: self.source_identity.clone(),
            module_ref: self.module_ref.to_string(),
            entry_kind: "process".to_string(),
            entry_ref: Some(lashlang::process_ref_key(&self.process_ref)),
            entry_name: self.process_name.clone(),
            restate_invocation_id: self.restate_invocation_id.clone(),
            generation: Some(lash_trace::TraceLanguageExecutionGeneration::new(
                self.attempt,
                self.incarnation.registration_sequence(),
            )),
        }
    }

    pub(super) fn event_key(&self, suffix: impl std::fmt::Display) -> String {
        format!(
            "lashlang_execution:{}:{suffix}",
            self.identity().graph_key()
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "process admission verified the named process exists in this artifact"
    )]
    pub(super) fn emit_started(&self, artifact: &lashlang::ModuleArtifact) {
        self.emit(TraceLanguageExecution {
            event_key: self.event_key("started"),
            identity: self.identity(),
            payload: TraceLanguageExecutionPayload::ExecutionStarted {
                execution_map: trace_lashlang_process_map(artifact, &self.process_name)
                    .expect("admission verified the process exists in the artifact"),
            },
        });
    }

    pub(super) fn emit_finished(&self, output: &lash_core::ProcessAwaitOutput) {
        let (status, error) = match output {
            lash_core::ProcessAwaitOutput::Settled { output } => match &output.outcome {
                lash_core::ToolCallOutcome::Success(_) => {
                    (TraceLanguageExecutionStatus::Completed, None)
                }
                lash_core::ToolCallOutcome::Failure(failure) => (
                    TraceLanguageExecutionStatus::Failed,
                    Some(failure.message.clone()),
                ),
                lash_core::ToolCallOutcome::Cancelled(cancellation) => (
                    TraceLanguageExecutionStatus::Cancelled,
                    Some(cancellation.message.clone()),
                ),
            },
            // `emit_finished` fires after an actual execution, whose outcome is
            // Success/Failure/Cancelled — abandonment is written out-of-band by the sweep,
            // never returned by a run.
            lash_core::ProcessAwaitOutput::Abandoned { .. } => (
                TraceLanguageExecutionStatus::Failed,
                Some("process abandoned".to_string()),
            ),
            lash_core::ProcessAwaitOutput::NoLongerRetained { terminal_label, .. } => (
                TraceLanguageExecutionStatus::Failed,
                Some(format!("process no longer retained ({terminal_label})")),
            ),
        };
        if status == TraceLanguageExecutionStatus::Cancelled {
            self.emit_cancelled_in_flight();
        }
        self.emit(TraceLanguageExecution {
            event_key: self.event_key("finished"),
            identity: self.identity(),
            payload: TraceLanguageExecutionPayload::ExecutionFinished { status, error },
        });
    }

    pub(super) fn emit_observation(&self, observation: lashlang::LashlangExecutionObservation) {
        if self.sink.is_none() {
            return;
        }
        match &observation {
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence } => {
                self.active_nodes.lock_recover().insert(
                    (site.node_id.clone(), site.node_kind, *occurrence),
                    site.clone(),
                );
            }
            lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence }
            | lashlang::LashlangExecutionObservation::NodeFailed {
                site, occurrence, ..
            } => {
                self.active_nodes.lock_recover().remove(&(
                    site.node_id.clone(),
                    site.node_kind,
                    *occurrence,
                ));
                self.waiting_nodes
                    .finish(&site.node_id, site.node_kind, *occurrence);
            }
            lashlang::LashlangExecutionObservation::ChildProcessWaiting {
                site,
                occurrence,
                ..
            } => {
                self.waiting_nodes
                    .mark_waiting(&site.node_id, site.node_kind, *occurrence);
            }
            lashlang::LashlangExecutionObservation::NodeResumed { site, occurrence } => {
                self.waiting_nodes
                    .finish(&site.node_id, site.node_kind, *occurrence);
            }
            _ => {}
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
                    awaited: TraceNodeAwaited::ChildProcesses { process_ids },
                },
            ),
            lashlang::LashlangExecutionObservation::NodeResumed { site, occurrence } => (
                format!("node:{}:{occurrence}:resumed", site.node_id),
                TraceLanguageExecutionPayload::NodeResumed {
                    node_id: site.node_id,
                    node_kind: site.node_kind,
                    label: site.label,
                    occurrence,
                    resolution: TraceNodeWaitResolution::Resumed,
                },
            ),
            lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence }
                if site.node_kind == lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND =>
            {
                self.pending_resource_starts
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
                let call_id = self.finish_resource_call(&site, occurrence);
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
                let call_id = self.finish_resource_call(&site, occurrence);
                (
                    format!("node:{}:{occurrence}:failed", site.node_id),
                    TraceLanguageExecutionPayload::NodeFailed {
                        node_id: site.node_id,
                        node_kind: site.node_kind,
                        label: site.label,
                        occurrence,
                        call_id,
                        failure: crate::language_trace_host::trace_failure(failure),
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
                        scope: self.scope(),
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
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(suffix),
            identity: self.identity(),
            payload,
        });
    }

    pub(super) fn emit_waiting(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        awaited: TraceNodeAwaited,
    ) {
        if self.sink.is_none() {
            return;
        }
        let site = &call_site.site;
        self.waiting_nodes
            .mark_waiting(&site.node_id, site.node_kind, call_site.occurrence);
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(format!(
                "node:{}:{}:waiting",
                site.node_id, call_site.occurrence
            )),
            identity: self.identity(),
            payload: TraceLanguageExecutionPayload::NodeWaiting {
                node_id: site.node_id.clone(),
                node_kind: site.node_kind,
                label: site.label.clone(),
                occurrence: call_site.occurrence,
                awaited,
            },
        });
    }

    pub(super) fn emit_resumed(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        resolution: TraceNodeWaitResolution,
    ) {
        if self.sink.is_none() {
            return;
        }
        let site = &call_site.site;
        self.waiting_nodes
            .finish(&site.node_id, site.node_kind, call_site.occurrence);
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(format!(
                "node:{}:{}:resumed",
                site.node_id, call_site.occurrence
            )),
            identity: self.identity(),
            payload: TraceLanguageExecutionPayload::NodeResumed {
                node_id: site.node_id.clone(),
                node_kind: site.node_kind,
                label: site.label.clone(),
                occurrence: call_site.occurrence,
                resolution,
            },
        });
    }

    pub(super) fn emit_cancelled_in_flight(&self) {
        let active = std::mem::take(&mut *self.active_nodes.lock_recover());
        for ((_, _, occurrence), site) in active {
            self.emit_cancelled_site(site, occurrence);
        }
    }

    pub(super) fn emit_cancelled_site(
        &self,
        site: lashlang::LashlangExecutionSite,
        occurrence: u64,
    ) {
        self.active_nodes.lock_recover().remove(&(
            site.node_id.clone(),
            site.node_kind,
            occurrence,
        ));
        self.finish_resource_call(&site, occurrence);
        if self
            .waiting_nodes
            .finish(&site.node_id, site.node_kind, occurrence)
        {
            self.emit(TraceLanguageExecution {
                event_key: self.event_key(format!("node:{}:{occurrence}:resumed", site.node_id)),
                identity: self.identity(),
                payload: TraceLanguageExecutionPayload::NodeResumed {
                    node_id: site.node_id.clone(),
                    node_kind: site.node_kind,
                    label: site.label.clone(),
                    occurrence,
                    resolution: TraceNodeWaitResolution::Cancelled,
                },
            });
        }
        self.emit(TraceLanguageExecution {
            event_key: self.event_key(format!("node:{}:{occurrence}:cancelled", site.node_id)),
            identity: self.identity(),
            payload: TraceLanguageExecutionPayload::NodeCancelled {
                node_id: site.node_id,
                node_kind: site.node_kind,
                label: site.label,
                occurrence,
            },
        });
    }

    pub(super) fn record_resource_call(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        call_id: &str,
    ) {
        if self.sink.is_none() {
            return;
        }
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
                identity: self.identity(),
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

    pub(super) fn finish_resource_call(
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
                identity: self.identity(),
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

    pub(super) fn tool_child_execution_trace_hook(
        &self,
        call_site: lashlang::LashlangExecutionCallSite,
    ) -> Option<ToolChildExecutionTraceHook> {
        self.sink.as_ref()?;
        let trace = self.clone();
        let parent_node_id = call_site.site.node_id;
        let occurrence = call_site.occurrence;
        Some(ToolChildExecutionTraceHook::new(move |started| {
            let child = TraceLanguageChildExecution {
                scope: trace.scope(),
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
                event_key: trace.event_key(format!(
                    "child:{parent_node_id}:{occurrence}:{child_graph_key}"
                )),
                identity: trace.identity(),
                payload: TraceLanguageExecutionPayload::ChildStarted {
                    parent_node_id: parent_node_id.clone(),
                    occurrence,
                    child,
                },
            });
        }))
    }

    pub(super) fn emit(&self, event: TraceLanguageExecution) {
        let Some(sink) = &self.sink else {
            return;
        };
        let mut context = self.base_context.clone();
        context.session_id = self.session_id.clone();
        context.graph_node_id = language_event_node_id(&event.payload).map(str::to_string);
        let _ = sink.append(&TraceRecord::new(
            context,
            TraceEvent::LanguageExecution {
                language: LASHLANG_ENGINE_KIND.to_string(),
                event,
            },
        ));
    }
}
