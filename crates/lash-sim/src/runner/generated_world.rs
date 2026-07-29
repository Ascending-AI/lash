use super::*;

pub(super) struct GeneratedRuntimeWorld {
    clock: Arc<SimClock>,
    sessions: BTreeMap<String, GeneratedRuntimeSession>,
    queued_inputs: BTreeMap<String, String>,
    lease_ticks: BTreeMap<String, Vec<u64>>,
    backend_faults: SimBackendFaultInjector,
    provider_mutations: SimProviderMutationHarness,
    trigger_harness: SimTriggerHarness,
    store_factory: Arc<dyn SessionStoreFactory>,
    durable_writes: CheckpointWriteCollector,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
    runtime_boundaries: RuntimeBoundaryHarness,
    suspending_turns: BTreeMap<String, SuspendingTurn>,
    /// When set, the driver admits at most one live provider turn at a time
    /// (see `RuntimeCompletionState::serialize_provider_turns`). Enabled for the
    /// cross-backend durable re-run; left off for the in-memory reference/search.
    pub(super) serialize_provider_turns: bool,
}

/// A real generated turn that parks mid-flight on a tool/durable/exec await key
/// and is resumed only when the boundary scheduler delivers the matching
/// completion. This generalizes the fixed `prove_pending_tool_completion_through_turn`
/// proof into the live, interleaved generated search.
struct SuspendingTurn {
    core: lash::LashCore,
    handle: tokio::task::JoinHandle<Result<lash::TurnResult, FixedScriptRunnerError>>,
    events: Arc<RuntimeProofRecordingEvents>,
    key_slot: Arc<tokio::sync::Mutex<Option<lash_core::AwaitEventKey>>>,
    suspend_kind: BoundaryKind,
    tool_name: String,
    resolution: Value,
    /// `true` once the world has observed the turn parked on its await key
    /// (the tool registered its completion key and the turn future is not yet
    /// finished). Recorded before any resolution so the oracle can prove the
    /// turn suspended rather than running synchronously.
    suspended_before_completion: Option<bool>,
    resolution_scheduled: bool,
    completed_before_resolution: usize,
}

struct GeneratedRuntimeSession {
    _core: lash::LashCore,
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_schedule: ScriptedTransportSchedule,
    provider_scripts: Vec<ProviderWireScript>,
    provider_kind: String,
    active_provider_turns: BTreeMap<String, ActiveProviderTurn>,
    finished_provider_turns: BTreeMap<String, Value>,
}

struct ActiveProviderTurn {
    completion_event: BoundaryEvent,
    handle: tokio::task::JoinHandle<Result<Value, FixedScriptRunnerError>>,
    final_ready_at: u64,
    logical_ms_at_start: u64,
}

const SCHEDULE_TICK_MS: u64 = 40_000;

impl GeneratedRuntimeWorld {
    pub(super) fn new() -> Self {
        // The in-memory reference / generated SEARCH lane keeps full preserved
        // cross-session concurrency (serialize_provider_turns = false).
        let clock = SimClock::new();
        Self::with_backend(
            Arc::new(lash::persistence::InMemorySessionStoreFactory::with_clock(
                clock.clone(),
            )),
            RuntimeEffectReplayStore::Memory,
            Arc::new(lash::persistence::InMemoryAttachmentStore::new()),
            Arc::new(lash::persistence::InMemoryProcessExecutionEnvStore::new()),
            false,
            clock,
        )
    }

    /// Build the generated runtime world over an explicit backend (session store
    /// factory + durable-effect replay store + attachment/process-env stores). The
    /// reference in-memory run and the cross-backend SQLite re-run drive the SAME
    /// workload through the SAME scheduler-driven, concurrency-faithful driver,
    /// differing ONLY in this backend. That makes the cross-backend comparison
    /// genuinely apples-to-apples: any divergence is a real store divergence, not
    /// an artifact of a separate, fixed-order, provider-event-gated re-drive. A
    /// durable session store requires durable attachment/process-env stores, so
    /// those are supplied per backend rather than hard-coded to in-memory.
    pub(super) fn with_backend(
        store_factory: Arc<dyn SessionStoreFactory>,
        effect_replay_store: RuntimeEffectReplayStore,
        attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
        process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
        serialize_provider_turns: bool,
        clock: Arc<SimClock>,
    ) -> Self {
        let durable_writes = CheckpointWriteCollector::default();
        let store_factory: Arc<dyn SessionStoreFactory> = Arc::new(
            ObservedSessionStoreFactory::new(store_factory, durable_writes.clone()),
        );
        Self {
            clock: Arc::clone(&clock),
            sessions: BTreeMap::new(),
            queued_inputs: BTreeMap::new(),
            lease_ticks: BTreeMap::new(),
            backend_faults: SimBackendFaultInjector::default(),
            provider_mutations: SimProviderMutationHarness::default(),
            trigger_harness: SimTriggerHarness::default(),
            runtime_boundaries: RuntimeBoundaryHarness::new(
                Arc::clone(&store_factory),
                effect_replay_store,
                clock,
            ),
            store_factory,
            durable_writes,
            attachment_store,
            process_env_store,
            suspending_turns: BTreeMap::new(),
            serialize_provider_turns,
        }
    }

    pub(super) fn checkpoint_write_events(&self) -> Vec<CheckpointWriteEvent> {
        self.durable_writes.events()
    }

    pub(super) fn checkpoint_write_collector(&self) -> CheckpointWriteCollector {
        self.durable_writes.clone()
    }

    pub(super) async fn advance_time_for_boundary(&self, event: &BoundaryEvent) {
        let schedule_time = if event.kind == BoundaryKind::LeaseTime {
            event
                .payload
                .get("tick")
                .and_then(Value::as_u64)
                .unwrap_or(event.at)
        } else {
            event.at
        };
        let schedule_time = if event.kind == BoundaryKind::ProviderEvent {
            self.clock.logical_ms().saturating_add(SCHEDULE_TICK_MS)
        } else {
            schedule_time
        };
        self.clock.advance_to(schedule_time).await;
    }

    pub(super) fn pending_suspend_turn_count(&self) -> usize {
        self.suspending_turns
            .values()
            .filter(|turn| !turn.resolution_scheduled)
            .count()
    }

    pub(super) async fn deliver_boundary(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        if event.payload.get("suspend_resume").and_then(Value::as_bool) == Some(true) {
            return self.resolve_suspended_turn(event).await;
        }
        match event.kind {
            BoundaryKind::Ingress => {
                if event.payload.get("suspend_kind").is_some() {
                    self.open_suspending_session(event).await
                } else {
                    self.open_runtime_session(event).await
                }
            }
            BoundaryKind::QueuedIngress => self.queue_turn_input(event).await,
            BoundaryKind::Provider => self.finish_provider_turn(event).await,
            BoundaryKind::ProviderEvent => self.release_provider_event(event).await,
            BoundaryKind::Observer => self.observe_session(event),
            BoundaryKind::Cancellation => self.cancel_queued_input(event).await,
            BoundaryKind::Trigger => self.trigger_harness.deliver(event).await,
            BoundaryKind::BackendFailure => Ok(self.backend_faults.inject(event)),
            BoundaryKind::ProviderMutation => self.provider_mutations.reject(event).await,
            BoundaryKind::DurableEffect
            | BoundaryKind::ProcessWake
            | BoundaryKind::ProcessLifecycle
            | BoundaryKind::Worker
            | BoundaryKind::Tool
            | BoundaryKind::ExecCode => self
                .runtime_boundaries
                .deliver(event)
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string())),
            BoundaryKind::LeaseTime => self.advance_lease_time(event).await,
        }
    }

    async fn open_runtime_session(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let provider_texts = event
            .payload
            .get("provider_texts")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "ingress boundary `{}` missing provider_texts",
                    event.boundary_id
                ))
            })?;
        if provider_texts.is_empty() {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "ingress boundary `{}` provided no runtime provider scripts",
                event.boundary_id
            )));
        }
        let provider_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "ingress boundary `{}` missing provider_kind",
                    event.boundary_id
                ))
            })?;
        let scripts = runtime_provider_scripts_for_texts(provider_kind, &provider_texts)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let provider_scripts = scripts.clone();
        let provider_schedule = ScriptedTransportSchedule::new();
        let (core, transport, provider_kind) = runtime_core_for_scripts(
            scripts,
            Arc::clone(&self.store_factory),
            Arc::clone(&self.attachment_store),
            Arc::clone(&self.process_env_store),
            Some(provider_schedule.clone()),
            // The generated harness owns provider execution through explicit
            // `Provider` boundaries. Each modeled success turn gets one scripted
            // exchange slot and one scheduler-owned release sequence. The runtime
            // queued-work driver would run next-turn inputs autonomously against the
            // same scripted transport, consuming exchange slots and shifting later
            // modeled turns onto unreleased gates. Queued-work behavior is exercised
            // by dedicated runtime boundary facts; this scripted provider core keeps
            // queued work inert so modeled provider boundaries remain the only
            // provider exchanges in the session.
            true,
            self.clock.clone(),
        )?;
        let session = core
            .session(event.actor_alias.clone())
            .open()
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        if session.session_id() != event.actor_alias {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "ingress opened session `{}`, expected `{}`",
                session.session_id(),
                event.actor_alias
            )));
        }
        self.sessions.insert(
            event.actor_alias.clone(),
            GeneratedRuntimeSession {
                _core: core,
                session,
                transport,
                provider_schedule,
                provider_scripts,
                provider_kind,
                active_provider_turns: BTreeMap::new(),
                finished_provider_turns: BTreeMap::new(),
            },
        );
        Ok(json!({
            "session": event.actor_alias,
            "opened": true,
            "ingress_count": 1,
        }))
    }

    async fn queue_turn_input(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "queued ingress boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let text = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("queued input");
        let source_key = event
            .payload
            .get("source_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id);
        let mut enqueue = runtime_session
            .session
            .enqueue(lash::TurnInput::text(text.to_string()))
            .id(source_key);
        let observed_active_turn_id = event
            .payload
            .get("active_turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if event.payload.get("ingress_mode").and_then(Value::as_str) == Some("active_turn") {
            let active_turn_id = observed_active_turn_id
                .as_deref()
                .unwrap_or(&event.boundary_id);
            enqueue = enqueue.ingress(lash_core::TurnInputIngress::active_turn(
                active_turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ));
        }
        let acceptance = enqueue
            .send()
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        self.queued_inputs
            .insert(event.boundary_id.clone(), acceptance.input_id.clone());
        let input_state = match acceptance.ingress {
            lash_core::TurnInputIngress::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputIngress::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        Ok(json!({
            "session": event.actor_alias,
            "queued_ingress": true,
            "source_key": source_key,
            "input_id": acceptance.input_id,
            "input_state": input_state.as_str(),
            "ingress_mode": event
                .payload
                .get("ingress_mode")
                .and_then(Value::as_str)
                .unwrap_or("next_turn"),
            "active_turn_id": observed_active_turn_id,
        }))
    }

    pub(super) async fn start_provider_turn(
        &mut self,
        event: BoundaryEvent,
        completion_event: BoundaryEvent,
        scheduler: &mut BoundaryScheduler,
    ) -> Result<(), FixedScriptRunnerError> {
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "provider boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let expected_turn_index = event
            .payload
            .get("turn_index")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let script = runtime_session
            .provider_scripts
            .get(expected_turn_index.saturating_sub(1))
            .cloned()
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "provider boundary `{}` had no runtime provider script for turn {}",
                    event.boundary_id, expected_turn_index
                ))
            })?;
        let exchange_index = expected_turn_index.saturating_sub(1);
        if runtime_session
            .active_provider_turns
            .contains_key(&event.boundary_id)
        {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "provider boundary `{}` was already active",
                event.boundary_id
            )));
        }

        let turn_started_at = completion_event.at;
        let mut final_ready_at = turn_started_at.saturating_add(1);
        for (event_index, wire_event) in script.timeline.iter().enumerate() {
            let release_at = turn_started_at.saturating_add(wire_event.at());
            final_ready_at = final_ready_at.max(release_at.saturating_add(1));
            scheduler.schedule(provider_release_boundary(
                &completion_event,
                &script,
                exchange_index,
                event_index,
                wire_event,
                release_at,
            ));
        }

        let mut completion_event = completion_event;
        completion_event.at = final_ready_at;
        set_runtime_completion_ready_at(&mut completion_event, final_ready_at);

        let session = runtime_session.session.clone();
        let transport = Arc::clone(&runtime_session.transport);
        let provider_kind = runtime_session.provider_kind.clone();
        let task_event = event.clone();
        let mut handle = tokio::spawn(async move {
            run_provider_turn_task(session, transport, provider_kind, task_event).await
        });
        tokio::select! {
            _ = runtime_session.provider_schedule.wait_until_blocked(exchange_index, 0) => {}
            result = &mut handle => {
                let result = result.map_err(|err| {
                    FixedScriptRunnerError::Runtime(format!(
                        "provider turn `{}` task failed before its first scheduled gate: {err}",
                        event.boundary_id
                    ))
                })?;
                return match result {
                    Ok(_) => Err(FixedScriptRunnerError::Assertion(format!(
                        "provider turn `{}` completed before its first scheduled gate",
                        event.boundary_id
                    ))),
                    Err(err) => Err(err),
                };
            }
        }
        runtime_session.active_provider_turns.insert(
            event.boundary_id.clone(),
            ActiveProviderTurn {
                completion_event,
                handle,
                final_ready_at,
                logical_ms_at_start: self.clock.logical_ms(),
            },
        );
        Ok(())
    }

    async fn release_provider_event(
        &self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let turn_boundary_id = event
            .payload
            .get("turn_boundary_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "provider event `{}` missing turn_boundary_id",
                    event.boundary_id
                ))
            })?;
        let event_index = event
            .payload
            .get("event_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "provider event `{}` missing event_index",
                    event.boundary_id
                ))
            })? as usize;
        let exchange_index = event
            .payload
            .get("exchange_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "provider event `{}` missing exchange_index",
                    event.boundary_id
                ))
            })? as usize;
        let event_name = event
            .payload
            .get("event_name")
            .and_then(Value::as_str)
            .unwrap_or("provider_event");
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "provider event `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let active_turn_pending = runtime_session
            .active_provider_turns
            .contains_key(turn_boundary_id);
        let release = active_turn_pending.then(|| {
            runtime_session.provider_schedule.release(
                exchange_index,
                event_index,
                event_name,
                event.at,
            )
        });
        let mut observed = json!({
            "session": event.actor_alias,
            "provider_event_release": true,
            "turn_boundary_id": turn_boundary_id,
            "exchange_index": exchange_index,
            "event_index": event_index,
            "event_name": event_name,
            "provider_kind": runtime_session.provider_kind,
        });
        if let Some(release) = release {
            observed["active_turn_pending_before_release"] = json!(active_turn_pending);
            observed["released_while_turn_pending"] = json!(active_turn_pending);
            observed["scripted_transport_release"] = json!({
                "exchange_index": release.exchange_index,
                "event_index": release.event_index,
                "event_name": release.event_name,
                "at": release.at,
                "blocked_before_release": release.blocked_before_release,
            });
        } else {
            observed["provider_event_release_noop_turn_finished"] = json!(true);
        }
        Ok(observed)
    }

    async fn finish_provider_turn(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "provider completion `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        runtime_session
            .finished_provider_turns
            .remove(&event.boundary_id)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "provider completion `{}` was delivered before its turn future completed",
                    event.boundary_id
                ))
            })
    }

    pub(super) async fn schedule_finished_provider_turns(
        &mut self,
        scheduler: &mut BoundaryScheduler,
    ) -> Result<(), FixedScriptRunnerError> {
        tokio::task::yield_now().await;
        let session_aliases = self.sessions.keys().cloned().collect::<Vec<_>>();
        for session_alias in session_aliases {
            let finished_ids = self
                .sessions
                .get(&session_alias)
                .into_iter()
                .flat_map(|session| {
                    session
                        .active_provider_turns
                        .iter()
                        .filter(|(_, active)| active.handle.is_finished())
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            for turn_id in finished_ids {
                let active = self
                    .sessions
                    .get_mut(&session_alias)
                    .and_then(|session| session.active_provider_turns.remove(&turn_id))
                    .ok_or_else(|| {
                        FixedScriptRunnerError::Assertion(format!(
                            "finished provider turn `{turn_id}` disappeared before scheduling completion"
                        ))
                    })?;
                let ActiveProviderTurn {
                    completion_event,
                    handle,
                    final_ready_at,
                    logical_ms_at_start,
                } = active;
                let mut observed = handle.await.map_err(|err| {
                    FixedScriptRunnerError::Runtime(format!(
                        "provider turn `{turn_id}` task failed to join: {err}"
                    ))
                })??;
                observed["sim_clock"] = json!({
                    "virtual_time_fast_forwarded": true,
                    "logical_ms": self.clock.logical_ms(),
                    "scheduled_elapsed_ms": self.clock.logical_ms().saturating_sub(logical_ms_at_start),
                });
                let runtime_session = self.sessions.get_mut(&session_alias).ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "provider turn `{turn_id}` session `{session_alias}` disappeared"
                    ))
                })?;
                runtime_session
                    .finished_provider_turns
                    .insert(turn_id, observed);
                debug_assert_eq!(completion_event.at, final_ready_at);
                scheduler.schedule(completion_event);
            }
        }
        Ok(())
    }

    pub(super) fn active_provider_turn_count(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.active_provider_turns.len())
            .sum()
    }

    /// The earliest completion time (`final_ready_at`) across all live provider
    /// turns, or `None` when none is live. In serialize mode this is the delivery
    /// barrier: the driver holds back any boundary scheduled at or after this time
    /// until the turn finishes and its completion lands in the scheduler, so the
    /// completion is always delivered at its own `at` ahead of later boundaries —
    /// making the delivery order independent of how long the (sync in-memory vs
    /// async durable) store takes to drive the turn to completion.
    pub(super) fn min_active_final_ready_at(&self) -> Option<u64> {
        self.sessions
            .values()
            .flat_map(|session| session.active_provider_turns.values())
            .map(|active| active.final_ready_at)
            .min()
    }

    fn observe_session(&self, event: &BoundaryEvent) -> Result<Value, FixedScriptRunnerError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "observer boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let expected_turn_index = event
            .payload
            .get("turn_index")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let observation = runtime_session.session.observe().current_observation();
        let read_view = observation.read_view;
        let graph_node_count = read_view.session_graph().nodes.len();
        let transcript_message_count = read_view.messages().len();
        let graph_non_empty = graph_node_count > 0;
        let observer_ok = read_view.session_id() == event.actor_alias
            && read_view.turn_index() == expected_turn_index
            && graph_non_empty;
        if !observer_ok {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "observer invariants failed for `{}`: session_id={} turn_index={} graph_nodes={}",
                event.boundary_id,
                read_view.session_id(),
                read_view.turn_index(),
                read_view.session_graph().nodes.len()
            )));
        }
        Ok(json!({
            "session": event.actor_alias,
            "turn_index": expected_turn_index,
            "reconnected": event.payload
                .get("reconnect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "graph_node_count": graph_node_count,
            "transcript_message_count": transcript_message_count,
            "observer_invariants": {
                "session_id": true,
                "turn_index_converged": true,
                "graph_non_empty": true,
                "transcript_message_count_converged": transcript_message_count >= expected_turn_index * 2,
            },
        }))
    }

    async fn cancel_queued_input(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let runtime_session = self.sessions.get(&event.actor_alias).ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "cancellation boundary `{}` ran before ingress for `{}`",
                event.boundary_id, event.actor_alias
            ))
        })?;
        let target = event
            .payload
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "cancellation boundary `{}` missing target",
                    event.boundary_id
                ))
            })?;
        let input_id = self.queued_inputs.get(target).cloned().ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "cancellation boundary `{}` target `{target}` was not queued",
                event.boundary_id
            ))
        })?;
        let outcome = runtime_session
            .session
            .cancel_pending_turn_input(&input_id)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let (cancelled, cancel_outcome) = match &outcome {
            lash::PendingTurnInputCancelOutcome::Cancelled(_) => (true, "cancelled"),
            lash::PendingTurnInputCancelOutcome::AlreadyClaimed { .. } => {
                (false, "already_claimed")
            }
            lash::PendingTurnInputCancelOutcome::AlreadyCompleted(_) => {
                (false, "already_completed")
            }
            lash::PendingTurnInputCancelOutcome::AlreadyCancelled(_) => {
                (false, "already_cancelled")
            }
            lash::PendingTurnInputCancelOutcome::NotFound => (false, "not_found"),
        };
        Ok(json!({
            "session": event.actor_alias,
            "target": target,
            "cancelled": cancelled,
            "cancel_outcome": cancel_outcome,
        }))
    }

    async fn advance_lease_time(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let tick = event
            .payload
            .get("tick")
            .and_then(Value::as_u64)
            .unwrap_or(event.at);
        let ticks = self
            .lease_ticks
            .entry(event.actor_alias.clone())
            .or_default();
        let monotonic = ticks
            .last()
            .copied()
            .is_none_or(|previous| previous <= tick);
        ticks.push(tick);
        // Ground the lease-time tick in a real session-execution-lease fencing
        // token from the store. This token (not the generator-fed `tick`) is
        // what the lease-time-monotonic oracle now asserts; the field is
        // normalized away on cross-backend replay because the abstract model
        // store cannot reproduce a real lease fence.
        let lease_fencing_token = self
            .runtime_boundaries
            .lease_probe_fencing_token(&event.actor_alias)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        Ok(json!({
            "session": event.actor_alias,
            "lease_time_tick": tick,
            "monotonic": monotonic,
            "runtime_lease_probe": {
                "session_execution_lease_fencing_token": lease_fencing_token,
                "real_lease_store": true,
            },
        }))
    }

    /// Open a suspend session and spawn its real turn. The turn calls a sim tool
    /// that registers its await key and returns `ToolResult::pending`, so the
    /// turn future parks mid-flight and cannot finish until the scheduler later
    /// delivers the matching completion boundary. The observed masquerades as a
    /// normal ingress for the abstract store; suspend evidence lives in a
    /// normalized-away field so cross-backend replay stays green.
    async fn open_suspending_session(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let suspend_kind_label = event
            .payload
            .get("suspend_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "suspend ingress `{}` missing suspend_kind",
                    event.boundary_id
                ))
            })?;
        let suspend_kind = match suspend_kind_label {
            "tool" => BoundaryKind::Tool,
            "durable_effect" => BoundaryKind::DurableEffect,
            "exec_code" => BoundaryKind::ExecCode,
            other => {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "suspend ingress `{}` had unknown suspend_kind `{other}`",
                    event.boundary_id
                )));
            }
        };
        let session_alias = event.actor_alias.clone();
        let tool_name = format!("await_{suspend_kind_label}");
        let resolution = json!({
            "ok": true,
            "suspend_kind": suspend_kind_label,
            "session": session_alias,
            "resolved_by": "lash-sim-boundary-scheduler",
        });

        let key_slot = Arc::new(tokio::sync::Mutex::new(None));
        let events = Arc::new(RuntimeProofRecordingEvents::default());
        // Route the parked turn through the real openai-compatible provider wire
        // transport (not a TestProvider), so both the tool-call exchange that
        // suspends the turn and the post-resume exchange exercise real provider
        // wire parsing.
        let suspend_scripts = suspend_roundtrip_scripts(&tool_name)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts(suspend_scripts));
        let (provider_handle, model, _provider_kind) =
            runtime_provider_components(OPENAI_COMPATIBLE, &transport)
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let core = lash::LashCore::standard_builder()
            .effect_host(Arc::new(
                lash::durability::InlineEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            ))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .store_factory(Arc::new(ObservedSessionStoreFactory::new(
                Arc::new(lash::persistence::InMemorySessionStoreFactory::with_clock(
                    self.clock.clone(),
                )),
                self.durable_writes.clone(),
            )))
            .clock(self.clock.clone())
            .lease_timings(crate::lease::sim_runtime_lease_timings())
            .process_registry(Arc::new(lash_core::TestLocalProcessRegistry::default())
                as Arc<dyn lash_core::ProcessRegistry>)
            .provider(provider_handle)
            .model(model)
            .tools(Arc::new(SuspendToolProvider::new(
                tool_name.clone(),
                Arc::clone(&key_slot),
            )) as Arc<dyn lash_core::ToolProvider>)
            .build()
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let session = core
            .session(session_alias.clone())
            .open()
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let turn_session = session.clone();
        let turn_events = Arc::clone(&events);
        let suspend_label = suspend_kind_label.to_string();
        let handle = tokio::spawn(async move {
            turn_session
                .turn(lash::TurnInput::text(format!(
                    "await {suspend_label} completion"
                )))
                .stream_to(turn_events.as_ref())
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
        });
        self.suspending_turns.insert(
            session_alias.clone(),
            SuspendingTurn {
                core,
                handle,
                events,
                key_slot,
                suspend_kind,
                tool_name,
                resolution,
                suspended_before_completion: None,
                resolution_scheduled: false,
                completed_before_resolution: 0,
            },
        );
        Ok(json!({
            "session": session_alias,
            "opened": true,
            "ingress_count": 1,
            "runtime_suspend": {
                "suspend_kind": suspend_kind_label,
                "spawned": true,
            },
        }))
    }

    /// Poll the spawned suspend turns. Once a turn has registered its await key
    /// (it parked on the tool) and is still in flight, schedule the matching
    /// completion boundary into the scheduler — mirroring how finished provider
    /// turns schedule their completion. The completion is the only thing that
    /// can resume the parked turn.
    pub(super) async fn schedule_parked_suspend_resolutions(
        &mut self,
        scheduler: &mut BoundaryScheduler,
        ready_at: u64,
    ) -> Result<(), FixedScriptRunnerError> {
        tokio::task::yield_now().await;
        for (session_alias, turn) in self.suspending_turns.iter_mut() {
            if turn.resolution_scheduled {
                continue;
            }
            let key_present = turn.key_slot.lock().await.is_some();
            if !key_present {
                continue;
            }
            // The await key exists, so the tool parked the turn. Record that the
            // turn suspended before any completion was delivered.
            let suspended =
                !turn.handle.is_finished() && turn.events.tool_completed_count().await == 0;
            turn.suspended_before_completion = Some(suspended);
            turn.completed_before_resolution = turn.events.tool_completed_count().await;
            let boundary_id = format!("{session_alias}:suspend-resume:001");
            let label = format!("suspend.{}.resume", boundary_kind_label(turn.suspend_kind));
            scheduler.schedule(BoundaryEvent::new(
                boundary_id,
                session_alias.clone(),
                turn.suspend_kind,
                ready_at,
                label,
                json!({
                    "suspend_resume": true,
                    "tool": turn.tool_name,
                    "output": turn.resolution,
                    "session": session_alias,
                }),
            ));
            turn.resolution_scheduled = true;
        }
        Ok(())
    }

    /// Resolve a parked suspend turn via `core.completions().resolve(...)` when
    /// the scheduler delivers its completion boundary, then await the resumed
    /// turn to completion. The observed masquerades as the matching runtime
    /// boundary (tool/exec/durable) for the abstract store, with suspend/resume
    /// evidence in a normalized-away field.
    async fn resolve_suspended_turn(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let turn = self
            .suspending_turns
            .remove(&event.actor_alias)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "suspend resume `{}` had no parked turn for `{}`",
                    event.boundary_id, event.actor_alias
                ))
            })?;
        let key = turn.key_slot.lock().await.take().ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "suspend resume `{}` delivered before the turn registered its await key",
                event.boundary_id
            ))
        })?;
        let suspended_before_completion = turn.suspended_before_completion.unwrap_or(false);
        let completed_before = turn.completed_before_resolution;
        let resolution = turn.resolution.clone();
        let accepted = turn
            .core
            .completions()
            .resolve(key, lash_core::Resolution::Ok(resolution.clone()))
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let result = turn.handle.await.map_err(|err| {
            FixedScriptRunnerError::Runtime(format!(
                "suspend turn `{}` task failed to join: {err}",
                event.actor_alias
            ))
        })??;
        let completed_after = turn.events.tool_completed_count().await;
        let assistant_message = result.assistant_message().unwrap_or_default().to_string();
        let resumed_after_completion = completed_after > completed_before
            && matches!(
                &result.outcome,
                lash_core::TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
            );
        let resolve_accepted = matches!(accepted, lash_core::ResolveOutcome::Accepted);
        Ok(json!({
            "session": event.actor_alias,
            "tool_output": resolution,
            "tool_name": turn.tool_name,
            "tool_call_id": event.boundary_id,
            "execution_count": 1,
            "runtime_tool_output": lash_core::ToolCallOutput::success(resolution.clone()),
            "runtime_suspend": {
                "suspend_kind": boundary_kind_label(turn.suspend_kind),
                "turn_suspended_before_completion": suspended_before_completion,
                "scheduler_delivered_completion": true,
                "resolve_accepted": resolve_accepted,
                "resumed_after_completion": resumed_after_completion,
                "completed_event_count_before_resolution": completed_before,
                "completed_event_count_after_resolution": completed_after,
                "final_assistant_message": assistant_message,
            },
        }))
    }
}

fn boundary_kind_label(kind: BoundaryKind) -> &'static str {
    match kind {
        BoundaryKind::Tool => "tool",
        BoundaryKind::DurableEffect => "durable_effect",
        BoundaryKind::ExecCode => "exec_code",
        _ => "unknown",
    }
}

pub(super) fn provider_release_boundary(
    turn_event: &BoundaryEvent,
    script: &ProviderWireScript,
    exchange_index: usize,
    event_index: usize,
    wire_event: &ProviderWireEvent,
    at: u64,
) -> BoundaryEvent {
    BoundaryEvent::new(
        format!(
            "{}:provider-event:{event_index:03}:{}",
            turn_event.boundary_id,
            wire_event.event_name()
        ),
        turn_event.actor_alias.clone(),
        BoundaryKind::ProviderEvent,
        at,
        format!("provider.{}", wire_event.event_name()),
        json!({
            "turn_boundary_id": turn_event.boundary_id,
            "provider_kind": turn_event
                .payload
                .get("provider_kind")
                .cloned()
                .unwrap_or_else(|| json!(script.provider_kind.clone())),
            "script": turn_event.payload.get("script").cloned().unwrap_or(Value::Null),
            "script_name": script.name.clone(),
            "exchange_index": exchange_index,
            "event_index": event_index,
            "event_name": wire_event.event_name(),
            "wire_at": wire_event.at(),
        }),
    )
}

fn set_runtime_completion_ready_at(event: &mut BoundaryEvent, ready_at: u64) {
    if let Some(completion) = event
        .payload
        .get_mut("runtime_completion")
        .and_then(Value::as_object_mut)
    {
        completion.insert("ready_at".to_string(), json!(ready_at));
    }
}

async fn run_provider_turn_task(
    session: lash::LashSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_kind: String,
    event: BoundaryEvent,
) -> Result<Value, FixedScriptRunnerError> {
    let expected_text = event
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let expected_turn_index = event
        .payload
        .get("turn_index")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let output = session
        .turn(lash::TurnInput::text(format!(
            "Run generated provider turn {}.",
            event.boundary_id
        )))
        .turn_id(event.boundary_id.clone())
        .run()
        .await
        .map_err(|err| {
            FixedScriptRunnerError::Runtime(format!(
                "runtime turn `{}` error: {err}",
                event.boundary_id
            ))
        })?;
    let assistant_message = output.assistant_message().unwrap_or_default().to_string();
    let read_view = output.result.state.read_view();
    let graph_node_count = output.result.state.session_graph.nodes.len();
    let transcript_message_count = read_view.messages().len();
    let provider_exchange_count = transport_exchanges(transport.as_ref())?.len();
    let graph_invariant = runtime_graph_invariant_facts(&output.result.state.session_graph);
    let agent_frame_invariant = runtime_agent_frame_invariant_facts(&output.result.state);
    let usage_invariant = runtime_usage_invariant_facts(&output.result, &output.activities);
    let final_value_invariant =
        runtime_final_value_invariant_facts(&output.result, &output.activities);
    if !output.is_success() {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "runtime turn `{}` did not succeed; turn_index={} outcome={:?} activities={:?}",
            event.boundary_id,
            output.result.state.turn_index,
            output.result.outcome,
            output.activities
        )));
    }
    let observation = RuntimeTurnObservation {
        session_id: output.result.state.session_id.clone(),
        turn_index: output.result.state.turn_index,
        assistant_message: assistant_message.clone(),
        graph_node_count,
        transcript_message_count,
        activity_count: output.activities.len(),
        provider_exchange_count,
        graph_invariant: Some(graph_invariant.clone()),
        agent_frame_invariant: Some(agent_frame_invariant.clone()),
        usage_invariant: Some(usage_invariant.clone()),
    };
    let expected_exchange_count = event
        .payload
        .get("expected_provider_exchange_count")
        .and_then(Value::as_u64)
        .unwrap_or(expected_turn_index as u64) as usize;
    let runtime_contract = runtime_turn_contract(
        &observation,
        &event.actor_alias,
        expected_turn_index,
        expected_text,
        expected_exchange_count,
    );
    if let Err(message) = require_passed(&runtime_contract) {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "runtime invariants failed for `{}`: {message}; success={} session_id={} turn_index={} graph_nodes={} transcript_messages={} activities={}",
            event.boundary_id,
            output.is_success(),
            output.result.state.session_id,
            output.result.state.turn_index,
            graph_node_count,
            transcript_message_count,
            output.activities.len()
        )));
    }
    Ok(json!({
        "session": event.actor_alias,
        "runtime_session_id": event.actor_alias,
        "turn_index": expected_turn_index,
        "success": true,
        "provider_output": assistant_message,
        "provider_script": event.payload.get("script").cloned().unwrap_or(Value::Null),
        "provider_exchange_count": provider_exchange_count,
        "graph_node_count": graph_node_count,
        "transcript_message_count": transcript_message_count,
        "activity_count_nonzero": !output.activities.is_empty(),
        "provider_kind": provider_kind,
        "runtime_invariants": {
            "session_id": true,
            "turn_index": true,
            "graph_non_empty": graph_node_count > 0,
            "graph_acyclic": graph_invariant.passed,
            "single_active_agent_frame": agent_frame_invariant.passed,
            "usage_monotonic": usage_invariant.passed,
            "transcript_contains_provider_output": read_view.messages().iter().any(|message| {
                message.parts.iter().any(|part| part.content.contains(expected_text))
            }),
            "activity_count_nonzero": !output.activities.is_empty(),
        },
        "runtime_invariant_facts": {
            "graph": graph_invariant,
            "agent_frame": agent_frame_invariant,
            "usage": usage_invariant,
        },
        "runtime_final_value_facts": final_value_invariant,
        "runtime_contract": runtime_contract,
    }))
}

#[derive(Default)]
struct SimBackendFaultInjector {
    attempts_by_operation: BTreeMap<String, usize>,
}

impl SimBackendFaultInjector {
    fn inject(&mut self, event: &BoundaryEvent) -> Value {
        let operation = event
            .payload
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("backend_operation")
            .to_string();
        let attempts = self
            .attempts_by_operation
            .entry(operation.clone())
            .or_insert(0);
        *attempts += 1;
        let retryable = event
            .payload
            .get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        backend_fault_observation(
            event
                .payload
                .get("session")
                .cloned()
                .unwrap_or_else(|| json!(event.actor_alias)),
            operation,
            *attempts,
            retryable,
        )
    }
}

#[derive(Default)]
struct SimProviderMutationHarness {
    rejected_mutations: BTreeSet<String>,
    matrix_cache: ProviderMutationMatrixCache,
}

impl SimProviderMutationHarness {
    async fn reject(&mut self, event: &BoundaryEvent) -> Result<Value, FixedScriptRunnerError> {
        let mutation = event
            .payload
            .get("mutation")
            .and_then(Value::as_str)
            .unwrap_or("unknown_mutation")
            .to_string();
        let mutation_key = format!("{}:{mutation}", event.actor_alias);
        let first_rejection = self.rejected_mutations.insert(mutation_key);
        let observed = json!({
            "session": event.actor_alias,
            "provider_mutation": true,
            "mutation": mutation,
            "rejected": true,
            "first_rejection": first_rejection,
            "oracle": event.payload.get("oracle").cloned().unwrap_or(Value::Null),
        });
        self.matrix_cache
            .augment_observation(event, observed)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
    }
}

struct SimTriggerHarness {
    store: Arc<lash_core::InMemoryTriggerStore>,
    registered_source_keys: BTreeSet<String>,
}

impl Default for SimTriggerHarness {
    fn default() -> Self {
        Self {
            store: Arc::new(lash_core::InMemoryTriggerStore::default()),
            registered_source_keys: BTreeSet::new(),
        }
    }
}

impl SimTriggerHarness {
    async fn deliver(&mut self, event: &BoundaryEvent) -> Result<Value, FixedScriptRunnerError> {
        let session = event
            .payload
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or(&event.actor_alias)
            .to_string();
        let source_key = event
            .payload
            .get("source_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let source_type = "sim.trigger";
        if self.registered_source_keys.insert(source_key.clone()) {
            let draft = lash_core::TriggerSubscriptionDraft::for_process(
                format!("sim/{}", event.boundary_id),
                lash_core::ProcessExecutionEnvRef::new("process-env:sim-trigger"),
                source_type,
                source_key.clone(),
                lash_core::ProcessInput::External {
                    metadata: json!({
                        "trigger_boundary": event.boundary_id,
                    }),
                },
                lash_core::ProcessIdentity::new("sim-trigger").with_label(Some("sim trigger")),
            )
            .with_wake_target(lash_core::SessionScope::new(session.clone()));
            self.store
                .execute_command(
                    &format!("sim-trigger-register:{}", event.boundary_id),
                    lash_core::TriggerCommand::Register {
                        owner_scope: lash_core::TriggerOwnerScope::session(session.clone()),
                        actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                            session.clone(),
                        )),
                        draft,
                    },
                )
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        }
        let ingress = self
            .store
            .ingest_occurrence(
                lash_core::TriggerOccurrenceRequest::new(
                    source_type,
                    source_key.clone(),
                    json!({
                        "boundary_id": event.boundary_id,
                        "session": session,
                    }),
                    format!("sim-trigger:{}", event.boundary_id),
                )
                .with_source(json!({"sim": true})),
            )
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        Ok(json!({
            "session": session,
            "trigger_delivered": true,
            "source_key": source_key,
            "occurrence_id": ingress.occurrence.occurrence_id,
            "reservation_count": ingress.reservations.len(),
            "started_process": event.payload.get("started_process").cloned().unwrap_or(Value::Bool(true)),
        }))
    }
}
