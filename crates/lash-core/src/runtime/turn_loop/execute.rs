//! The execute phase: run the prepared turn's effect loop to a terminal
//! outcome, pumping its events to the host sinks and watching for cancellation
//! alongside it.

use super::*;
use crate::TurnId;

struct TurnDriverSessionLoan<'slot, 'run> {
    session: &'slot mut Option<Session>,
    driver: Option<Box<RuntimeTurnDriver<'run>>>,
}

pub(super) struct TurnDriverRemainder {
    pub(super) policy: RuntimeSessionPolicy,
    pub(super) turn_pipeline: TurnBoundary,
    pub(super) llm_calls: Vec<crate::LlmCallRecord>,
    pub(super) failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub(super) pending_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(super) pending_turn_input_claims: Vec<crate::runtime::turn_input_ingress::TurnInputDrive>,
}

/// Everything the execute phase needs to drive an already-prepared turn.
///
/// The prepare phase (and the host-prepared entry point) builds one of these;
/// the execute phase consumes it whole.
pub(in crate::runtime) struct PreparedTurnExecuteContext<'sinks, 'run> {
    pub(in crate::runtime) turn: PreparedLogicalTurn,
    pub(in crate::runtime) sinks: TurnSinks<'sinks>,
    pub(in crate::runtime) scoped_effect_controller: ScopedEffectController<'run>,
    pub(in crate::runtime) cancel: CancellationToken,
    pub(in crate::runtime) initial_queue_claims: Vec<crate::QueuedWorkClaim>,
    pub(in crate::runtime) initial_turn_input_claims:
        Vec<super::turn_input_ingress::TurnInputDrive>,
    pub(in crate::runtime) lease: TurnLeaseScope<'sinks>,
}

/// The preamble step of the execute phase: the plugin prepare-turn hooks and
/// the context transform that produce the message sequence the driver runs.
struct TurnPreambleContext<'preamble> {
    plugins: &'preamble crate::PluginSession,
    manager: &'preamble Arc<RuntimeSessionServices>,
    messages: crate::MessageSequence,
    turn_policy: &'preamble crate::SessionPolicy,
    effective_protocol_turn_options: &'preamble crate::ProtocolTurnOptions,
    turn_context: &'preamble crate::TurnContext,
    turn_scope_id: &'preamble str,
    event_rx: &'preamble mut mpsc::Receiver<RuntimeStreamEvent>,
    assembler: &'preamble mut TurnAssembler,
    sinks: TurnSinks<'preamble>,
}

/// The state a plugin abort inherits from the preamble, so the abort commit can
/// run on its own async frame without carrying the preamble's read view.
struct PreparedTurnAbortContext<'abort, 'run> {
    prepared: crate::plugin::TurnPreparation,
    event_tx: mpsc::Sender<RuntimeStreamEvent>,
    assembler: TurnAssembler,
    turn_index: usize,
    trace_turn_id: TurnId,
    claims: &'abort LogicalTurnClaims,
    sinks: TurnSinks<'abort>,
    scoped_effect_controller: &'abort ScopedEffectController<'run>,
    cancel: &'abort CancellationToken,
    lease: TurnLeaseScope<'abort>,
    session_execution_fence: Option<crate::SessionExecutionLeaseAuthority>,
    turn_control: &'abort ActiveTurnControl,
    turn_graph_appends: TurnGraphAppendDraft,
}

/// The effect loop's own inputs: the driver, the channels it pumps, and the
/// turn-control handles the cancellation watcher runs against.
struct TurnEffectLoopContext<'loop_run, 'run> {
    driver: &'loop_run mut RuntimeTurnDriver<'run>,
    messages: crate::MessageSequence,
    event_tx: mpsc::Sender<RuntimeStreamEvent>,
    cancellation: CancellationToken,
    protocol_run_offset: usize,
    clock: Arc<dyn Clock>,
    turn_control: Arc<ActiveTurnControl>,
    turn_control_host: Arc<dyn EffectHost>,
    cancel_controller: &'loop_run ScopedEffectController<'run>,
    event_rx: &'loop_run mut mpsc::Receiver<RuntimeStreamEvent>,
    assembler: &'loop_run mut TurnAssembler,
    child_usage_event_relay: &'loop_run ChildUsageEventRelay,
    sinks: TurnSinks<'loop_run>,
}

impl<'slot, 'run> TurnDriverSessionLoan<'slot, 'run> {
    fn new(session: &'slot mut Option<Session>, driver: Box<RuntimeTurnDriver<'run>>) -> Self {
        Self {
            session,
            driver: Some(driver),
        }
    }

    fn reclaim(mut self) -> TurnDriverRemainder {
        let RuntimeTurnDriver {
            session,
            policy,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queue_claims,
            pending_turn_input_claims,
            ..
        } = *self.driver.take().expect("turn driver loan is present");
        *self.session = Some(session);
        TurnDriverRemainder {
            policy,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queue_claims,
            pending_turn_input_claims,
        }
    }
}

impl<'slot, 'run> std::ops::Deref for TurnDriverSessionLoan<'slot, 'run> {
    type Target = RuntimeTurnDriver<'run>;

    fn deref(&self) -> &Self::Target {
        self.driver.as_deref().expect("turn driver loan is present")
    }
}

impl std::ops::DerefMut for TurnDriverSessionLoan<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.driver
            .as_deref_mut()
            .expect("turn driver loan is present")
    }
}

impl Drop for TurnDriverSessionLoan<'_, '_> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            // Re-arm the still-owned driver as a loan and consume it through
            // reclaim(); the inner loan's Drop is a no-op because reclaim()
            // takes the driver.
            let loan = TurnDriverSessionLoan::new(&mut *self.session, driver);
            drop(loan.reclaim());
        }
    }
}

async fn run_turn_effect_loop(
    context: TurnEffectLoopContext<'_, '_>,
) -> Result<(crate::MessageSequence, usize), RuntimeError> {
    let TurnEffectLoopContext {
        driver,
        messages,
        event_tx,
        cancellation,
        protocol_run_offset,
        clock,
        turn_control,
        turn_control_host,
        cancel_controller,
        event_rx,
        assembler,
        child_usage_event_relay,
        sinks: TurnSinks {
            events,
            turn_events,
        },
    } = context;
    // The start gate can change the handler's control flow before its first
    // effect, so durable runtimes must observe it through the handler-scoped
    // controller. That controller journals the observation and replays the
    // same answer after an owner crash. The shared controller is intentionally
    // reserved for the concurrent live watcher below: an out-of-band peek here
    // could observe a cancel that arrived after the original attempt and make
    // a replay take a different command path.
    let start_gate = crate::runtime::RuntimeNamedPhase::begin(
        driver.turn_phase_probe.clone(),
        "turn_cancel.start_gate",
    );
    let pending_cancel = await_turn_cancellation_start_gate(clock.as_ref(), || {
        turn_control.observe_pending_cancel(
            cancel_controller,
            crate::runtime::turn_control::TurnCancelPeekIdentity::StartGate,
        )
    })
    .await?;
    drop(start_gate);
    if pending_cancel.is_some() {
        cancellation.cancel();
    }
    let cancel_watcher = await_turn_cancellation_with_retry(clock.as_ref(), || {
        turn_control.await_cancel(turn_control_host.as_ref(), CancellationToken::new())
    });
    // Canonical future-size seam: `driver.run` is boxed exactly once here.
    // Driver growth is absorbed by this allocation instead of accreting
    // opportunistic boxes through the event-pump callers below.
    let run_future = Box::pin(driver.run(
        messages,
        event_tx,
        cancellation.clone(),
        protocol_run_offset,
    ));
    let drive = drive_turn_to_completion(
        run_future,
        event_rx,
        assembler,
        child_usage_event_relay,
        events,
        turn_events,
    );
    tokio::pin!(cancel_watcher);
    tokio::pin!(drive);
    tokio::select! {
        biased;
        observation = cancel_watcher.as_mut() => match observation {
            Ok(Some(_)) => {
                cancellation.cancel();
                drive.await
            }
            Ok(None) => drive.await,
            Err(err) => {
                cancellation.cancel();
                let _ = drive.await;
                Err(err)
            }
        },
        result = drive.as_mut() => result,
    }
}

const TURN_CANCEL_WATCH_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_millis(25);

const TURN_CANCEL_WATCH_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// Keep the journaled turn-start observation bounded so a broken peek cannot
/// pin one Restate invocation forever. Exhaustion fails closed: the error
/// propagates and the turn fails without starting any effect (hosts classify
/// it as non-retryable, so the invocation retires as a failed turn). Transient
/// transport trouble does not reach this bound — a slow journaled peek stays
/// pending inside one attempt; only genuine terminal errors (revoked or
/// unknown keys) burn attempts.
pub(super) const TURN_CANCEL_START_GATE_ATTEMPTS: usize = 3;

/// Bound the live watcher to a useful transient-recovery window without letting
/// a broken resolver outlive the turn indefinitely. Eight attempts traverse
/// the whole exponential ladder through its one-second ceiling (2.575 seconds
/// of injected sleep). Exhaustion fails closed through the same cancellation
/// token that observed evidence uses, tearing down in-flight turn execution.
pub(in crate::runtime) const TURN_CANCEL_WATCH_MAX_ATTEMPTS: usize = 8;

pub(super) async fn await_turn_cancellation_start_gate<F, C>(
    clock: &dyn Clock,
    mut watch: F,
) -> Result<Option<TurnCancellationEvidence>, RuntimeError>
where
    F: FnMut() -> C,
    C: std::future::Future<Output = Result<Option<TurnCancellationEvidence>, RuntimeError>>,
{
    let mut backoff = TURN_CANCEL_WATCH_RETRY_INITIAL;
    for attempt in 1..=TURN_CANCEL_START_GATE_ATTEMPTS {
        match watch().await {
            Ok(observation) => return Ok(observation),
            Err(err) if attempt == TURN_CANCEL_START_GATE_ATTEMPTS => return Err(err),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    attempt,
                    max_attempts = TURN_CANCEL_START_GATE_ATTEMPTS,
                    retry_after_ms = backoff.as_millis(),
                    "turn cancellation start gate failed; retrying before failing the invocation"
                );
                clock.sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(TURN_CANCEL_WATCH_RETRY_MAX);
            }
        }
    }
    unreachable!("positive start-gate attempt limit")
}

pub(super) async fn await_turn_cancellation_with_retry<F, C>(
    clock: &dyn Clock,
    mut watch: F,
) -> Result<Option<TurnCancellationEvidence>, RuntimeError>
where
    F: FnMut() -> C,
    C: std::future::Future<Output = Result<Option<TurnCancellationEvidence>, RuntimeError>>,
{
    let mut backoff = TURN_CANCEL_WATCH_RETRY_INITIAL;
    for attempt in 1..=TURN_CANCEL_WATCH_MAX_ATTEMPTS {
        match watch().await {
            Ok(observation) => return Ok(observation),
            Err(err) if attempt == TURN_CANCEL_WATCH_MAX_ATTEMPTS => {
                tracing::warn!(
                    error = %err,
                    attempts = attempt,
                    max_attempts = TURN_CANCEL_WATCH_MAX_ATTEMPTS,
                    "turn cancellation watcher exhausted its retry budget; tearing down turn execution"
                );
                return Err(err);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    attempt,
                    max_attempts = TURN_CANCEL_WATCH_MAX_ATTEMPTS,
                    retry_after_ms = backoff.as_millis(),
                    "turn cancellation watcher failed; retrying while the turn remains active"
                );
                clock.sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(TURN_CANCEL_WATCH_RETRY_MAX);
            }
        }
    }
    unreachable!("positive cancellation-watch attempt limit")
}

/// Pump the turn driver's event channel into the host sinks while the run
/// future executes, then drain any events emitted between completion and the
/// sender dropping.
///
/// Both the fresh and resumed turn entry points construct a
/// `RuntimeTurnDriver`, kick off its run future, and need identical
/// event-pump/drain behavior before tearing the driver down. Only the driver
/// construction and post-run teardown differ, so each caller owns those and
/// shares this loop.
async fn drive_turn_to_completion<F>(
    mut run_future: Pin<Box<F>>,
    event_rx: &mut mpsc::Receiver<RuntimeStreamEvent>,
    assembler: &mut TurnAssembler,
    child_usage_event_relay: &ChildUsageEventRelay,
    events: &dyn EventSink,
    turn_events: &dyn TurnActivitySink,
) -> Result<(crate::MessageSequence, usize), RuntimeError>
where
    F: std::future::Future<Output = Result<(crate::MessageSequence, usize, bool), RuntimeError>>
        + ?Sized,
{
    let mut event_pump = RuntimeStreamEventPump {
        assembler,
        events,
        turn_events,
    };
    let run_result = drive_with_event_pump(
        run_future.as_mut(),
        event_rx,
        &mut event_pump,
        |pump, event| {
            Box::pin(async move {
                pump.emit(event).await;
            })
        },
    )
    .await;
    child_usage_event_relay.clear();
    while let Some(event) = event_rx.recv().await {
        emit_runtime_stream_event_to_sinks(events, turn_events, event, assembler).await;
    }
    run_result.map(|(messages, iteration, turn_limit_final_scheduled)| {
        assembler.turn_limit_final_scheduled = turn_limit_final_scheduled;
        (messages, iteration)
    })
}

impl LashRuntime {
    async fn prepare_turn_preamble(
        &mut self,
        context: TurnPreambleContext<'_>,
    ) -> Result<crate::plugin::TurnPreparation, RuntimeError> {
        let TurnPreambleContext {
            plugins,
            manager,
            messages,
            turn_policy,
            effective_protocol_turn_options,
            turn_context,
            turn_scope_id,
            event_rx,
            assembler,
            sinks: TurnSinks {
                events,
                turn_events,
            },
        } = context;
        self.mark_phase_begin(RuntimeTurnPhase::BeforeTurnHooks);
        let prepare_turn = plugins.prepare_turn_with_phase_probe(
            PrepareTurnRequest {
                session_id: self.state.session_id.clone(),
                state: crate::SessionReadView::from_runtime_state(
                    &self.state,
                    turn_policy.clone(),
                    effective_protocol_turn_options.clone(),
                )
                .map_err(|error| {
                    RuntimeError::new(RuntimeErrorCode::ContextPrepareTurn, error.to_string())
                })?,
                messages,
                sessions: manager.state_service(),
                session_lifecycle: manager.lifecycle_service(),
                session_graph: manager.graph_service(),
                turn_context: turn_context.clone(),
            },
            self.turn_phase_probe.clone(),
            turn_scope_id,
        );
        let mut prepare_turn = Box::pin(prepare_turn);

        let mut event_pump = RuntimeStreamEventPump {
            assembler,
            events,
            turn_events,
        };
        let prepared = drive_with_event_pump(
            prepare_turn.as_mut(),
            event_rx,
            &mut event_pump,
            |pump, event| {
                Box::pin(async move {
                    pump.emit(event).await;
                })
            },
        )
        .await
        .map_err(|err| RuntimeError::new(RuntimeErrorCode::PluginPrepareTurn, err.to_string()))?;
        self.mark_phase_end(RuntimeTurnPhase::BeforeTurnHooks);
        Ok(prepared)
    }

    async fn finish_prepared_turn_abort(
        &mut self,
        context: PreparedTurnAbortContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let PreparedTurnAbortContext {
            prepared,
            event_tx,
            mut assembler,
            turn_index,
            trace_turn_id,
            claims,
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
            session_execution_fence: _session_execution_fence,
            turn_control,
            turn_graph_appends,
        } = context;
        let Some(abort) = prepared.abort else {
            unreachable!("abort finisher requires a prepared plugin abort");
        };
        drop(event_tx);

        // The preparation future and its SessionReadView are gone before this
        // state clone. That keeps the graph from being held twice while the
        // turn boundary takes ownership of its working state. Appends the
        // prepare-turn hooks recorded ride this abort commit.
        let mut turn_pipeline = TurnBoundary::from_state_with_graph_appends(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
            turn_graph_appends,
        );
        turn_pipeline.apply_prepared_messages(&prepared.messages);
        emit_terminal_sequence(
            &mut assembler,
            events,
            Some(TerminalDiagnostic {
                kind: TerminalDiagnosticKind::Plugin,
                code: Some(abort.code),
                message: abort.message,
                retryable: None,
                activity: TerminalActivityTarget::TurnScopedSink(turn_events),
            }),
            TurnStop::PluginAbort,
        )
        .await;
        Box::pin(self.finish_turn(TurnCommitContext {
            finish: TurnFinishInput {
                turn_pipeline,
                assembler,
                new_messages: prepared.messages,
                policy: self.state.effective_policy().clone(),
                turn_index,
                trace_turn_id,
            },
            claims,
            events,
            scoped_effect_controller,
            cancel_state: cancel,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: session_execution_lease_release_policy,
            },
            turn_control,
        }))
        .await
    }

    pub(in crate::runtime) async fn stream_prepared_turn_inner(
        &mut self,
        context: PreparedTurnExecuteContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        // Host-prepared turns ran no prepare-turn hooks, so no in-turn graph
        // append can predate this draft.
        let turn_graph_appends = TurnGraphAppendDraft::from_resident_state(
            &self.state,
            Arc::clone(&self.host.core.clock),
        );
        self.stream_prepared_turn_inner_with_graph_appends(context, turn_graph_appends)
            .await
    }

    pub(super) async fn stream_prepared_turn_inner_with_graph_appends(
        &mut self,
        context: PreparedTurnExecuteContext<'_, '_>,
        turn_graph_appends: TurnGraphAppendDraft,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let PreparedTurnExecuteContext {
            turn:
                PreparedLogicalTurn {
                    messages,
                    previous_prompt_usage,
                    protocol_turn_options,
                    protocol_extension,
                    turn_context,
                    initial_turn_causes,
                    trace_turn_id,
                    turn_index,
                },
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel,
            initial_queue_claims,
            initial_turn_input_claims,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
        } = context;
        let scoped_turn_events = TurnScopedActivitySink {
            turn_id: trace_turn_id.clone(),
            inner: turn_events,
        };
        let turn_events: &dyn TurnActivitySink = &scoped_turn_events;
        let turn_control_host = Arc::clone(&self.host.core.control.effect_host);
        let turn_control_binding =
            turn_control_binding(turn_control_host.as_ref(), &scoped_effect_controller).await?;
        let turn_control_resolver = match &turn_control_binding {
            crate::TurnControlBinding::HostOwned { resolver, peek: _ }
            | crate::TurnControlBinding::RunScoped {
                resolver,
                durable_cancel_after_llm: _,
            } => *resolver,
        };
        let turn_control = Arc::new(
            ActiveTurnControl::new(
                turn_control_resolver,
                TurnAddress::new(&self.state.session_id, &trace_turn_id),
            )
            .await?
            .with_local_cancel_origin(turn_context.local_cancel_origin_hint()),
        );
        let session_execution_fence =
            session_execution_lease.map(SessionExecutionLeaseGuard::fence);
        let (event_tx, mut event_rx) = mpsc::channel::<RuntimeStreamEvent>(100);
        let child_usage_event_relay = ChildUsageEventRelay::new(event_tx.clone());
        let mut turn_policy = self.state.effective_policy().clone();
        let turn_provider_override = turn_context.provider().cloned();
        if let Some(provider) = turn_provider_override.as_ref() {
            turn_policy.provider_id = provider.kind().to_string();
        }
        let session_protocol_turn_options = self.state.effective_protocol_turn_options().clone();
        let effective_protocol_turn_options = protocol_turn_options
            .clone()
            .map(|options| session_protocol_turn_options.merged_with_override(&options))
            .unwrap_or(session_protocol_turn_options);
        let manager = self
            .runtime_session_services_for_turn(
                Some(child_usage_event_relay.clone()),
                session_execution_lease,
                &turn_graph_appends,
            )
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let plugins = {
            let session = self
                .session
                .as_ref()
                .expect("lash runtime session must be available");
            Arc::clone(session.plugins())
        };
        let mut assembler = TurnAssembler::new();
        let initial_claims =
            LogicalTurnClaims::new(initial_queue_claims, initial_turn_input_claims);
        // Keep preparation and plugin-abort handling in separate async frames.
        // Their SessionReadView and abort-only locals are dropped before the
        // normal driver-construction frame clones state for the turn boundary.
        let mut prepared = self
            .prepare_turn_preamble(TurnPreambleContext {
                plugins: plugins.as_ref(),
                manager: &manager,
                messages,
                turn_policy: &turn_policy,
                effective_protocol_turn_options: &effective_protocol_turn_options,
                turn_context: &turn_context,
                turn_scope_id: &trace_turn_id,
                event_rx: &mut event_rx,
                assembler: &mut assembler,
                sinks: TurnSinks {
                    events,
                    turn_events,
                },
            })
            .await?;
        for event in &prepared.events {
            assembler.push(event);
        }
        emit_session_events_to_sink(events, std::mem::take(&mut prepared.events)).await;
        if prepared.abort.is_some() {
            return Box::pin(self.finish_prepared_turn_abort(PreparedTurnAbortContext {
                prepared,
                event_tx,
                assembler,
                turn_index,
                trace_turn_id,
                claims: &initial_claims,
                sinks: TurnSinks {
                    events,
                    turn_events,
                },
                scoped_effect_controller: &scoped_effect_controller,
                cancel: &cancel,
                lease: TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
                session_execution_fence,
                turn_control: turn_control.as_ref(),
                turn_graph_appends,
            }))
            .await;
        }
        // `prepare_turn_preamble` has returned and dropped its read-view frame
        // before this clone, avoiding a transient second graph owner.
        // Restore the basis captured before preparation cleared the resident
        // value. TurnBoundary is the state persisted by a host continuation;
        // keeping this value stable prevents a mid-turn call from changing the
        // rolling-history projection when the logical turn is redriven.
        self.state.last_prompt_usage = previous_prompt_usage;
        let mut turn_pipeline = TurnBoundary::from_state_with_graph_appends(
            self.state.clone(),
            Arc::clone(&self.host.core.clock),
            self.state.turn_scope(&trace_turn_id),
            self.host.core.durability.commit_budget,
            turn_graph_appends.clone(),
        );
        turn_pipeline
            .prepared_checkpoint(
                turn_policy.clone(),
                turn_index,
                &prepared.messages,
                self.session.as_mut(),
            )
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let resolved_turn_policy = if let Some(provider) = turn_provider_override {
            RuntimeSessionPolicy::from_provider(
                turn_policy.clone(),
                provider.with_clock(Arc::clone(&self.host.core.clock)),
            )
            .map_err(|err| {
                RuntimeError::new(crate::RuntimeErrorCode::LlmProvider, err.to_string())
            })?
        } else {
            self.host
                .resolve_session_policy(&self.state.session_id, turn_policy.clone())
                .map_err(|err| {
                    RuntimeError::new(crate::RuntimeErrorCode::LlmProvider, err.to_string())
                })?
        };
        let manager = self
            .runtime_session_services_for_turn(
                Some(child_usage_event_relay.clone()),
                session_execution_lease,
                &turn_graph_appends,
            )
            .map_err(|err| {
                RuntimeError::new(RuntimeErrorCode::PluginSessionManager, err.to_string())
            })?;
        let cancel_state = cancel.clone();
        let finish_scoped_effect_controller = scoped_effect_controller.clone();
        let (turn_cancel_peek_controller, observes_durable_cancel_after_llm) =
            match &turn_control_binding {
                crate::TurnControlBinding::HostOwned { resolver: _, peek } => (peek, false),
                crate::TurnControlBinding::RunScoped {
                    resolver: _,
                    durable_cancel_after_llm,
                } => (&finish_scoped_effect_controller, *durable_cancel_after_llm),
            };
        let session = self
            .session
            .take()
            .expect("lash runtime session must be available");
        let driver = Box::new(RuntimeTurnDriver {
            session,
            policy: resolved_turn_policy,
            host: self.host.clone(),
            turn_id: trace_turn_id.clone(),
            scoped_effect_controller: scoped_effect_controller.clone(),
            session_id: self.state.session_id.clone(),
            turn_index,
            turn_pipeline,
            latest_prompt_usage: None,
            llm_stream_summaries: HashMap::new(),
            reasoning_streamed: false,
            llm_calls: Vec::new(),
            failure_evidence: Vec::new(),
            next_llm_ordinal: 0,
            session_services: manager,
            protocol_turn_options: effective_protocol_turn_options,
            protocol_extension,
            turn_context,
            turn_causes: initial_turn_causes,
            pending_queue_claims: initial_claims.queued,
            pending_turn_input_claims: initial_claims.turn_inputs,
            pending_checkpoint_turn_input_claim: None,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            session_execution_lease: session_execution_fence,
            runtime_lease_owner: self.runtime_lease_owner.clone(),
            turn_phase_probe: self.turn_phase_probe.clone(),
            turn_control: Arc::clone(&turn_control),
            observes_durable_cancel_after_llm,
            protocol_reply: Default::default(),
        });
        let protocol_run_offset = 0;
        self.mark_phase_begin(RuntimeTurnPhase::EffectLoop);
        let mut driver = TurnDriverSessionLoan::new(&mut self.session, driver);
        let run_result = Box::pin(run_turn_effect_loop(TurnEffectLoopContext {
            driver: &mut driver,
            messages: prepared.messages,
            event_tx,
            cancellation: cancel.clone(),
            protocol_run_offset,
            clock: Arc::clone(&self.host.core.clock),
            turn_control: Arc::clone(&turn_control),
            turn_control_host: Arc::clone(&turn_control_host),
            cancel_controller: turn_cancel_peek_controller,
            event_rx: &mut event_rx,
            assembler: &mut assembler,
            child_usage_event_relay: &child_usage_event_relay,
            sinks: TurnSinks {
                events,
                turn_events,
            },
        }))
        .await;
        let (new_messages, _new_protocol_iteration) = match run_result {
            Ok(result) => result,
            Err(err) => {
                if cancel.is_cancelled() {
                    if turn_control.evidence().is_none() {
                        turn_control
                            .observe_pending_cancel(
                                turn_cancel_peek_controller,
                                crate::runtime::turn_control::TurnCancelPeekIdentity::PostAbortGate,
                            )
                            .await?;
                    }
                    if turn_control.evidence().is_some() {
                        let cancellation_messages = driver.turn_pipeline.message_sequence();
                        emit_parent_end_events(
                            driver.finish_parent_end_actions().await?,
                            &mut assembler,
                            events,
                        )
                        .await;
                        let driver = driver.reclaim();
                        self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                        return Box::pin(self.finish_cancelled_turn_after_effect_abort(
                            CancelledTurnFinishContext {
                                driver,
                                assembler,
                                cancellation_messages,
                                events,
                                finish_scoped_effect_controller: &finish_scoped_effect_controller,
                                cancel: &cancel,
                                lease: TurnLeaseScope {
                                    guard: session_execution_lease,
                                    release_policy: session_execution_lease_release_policy,
                                },
                                turn_control: turn_control.as_ref(),
                                turn_index,
                                trace_turn_id,
                            },
                        ))
                        .await;
                    }
                }
                emit_parent_end_events(
                    driver.finish_parent_end_actions().await?,
                    &mut assembler,
                    events,
                )
                .await;
                let driver = driver.reclaim();
                self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
                let TurnDriverRemainder {
                    pending_queue_claims,
                    pending_turn_input_claims,
                    ..
                } = driver;
                self.abandon_queued_work_claims_after_local_abort(&err, &pending_queue_claims)
                    .await;
                self.abandon_turn_input_claims_after_local_abort(&err, &pending_turn_input_claims)
                    .await;
                return Err(err);
            }
        };
        emit_parent_end_events(
            driver.finish_parent_end_actions().await?,
            &mut assembler,
            events,
        )
        .await;
        let driver = driver.reclaim();
        self.mark_phase_end(RuntimeTurnPhase::EffectLoop);
        tracing::debug!(
            new_message_count = new_messages.len(),
            tool_call_count = assembler.tool_calls.len(),
            "runtime post-run_task"
        );

        let TurnDriverRemainder {
            policy,
            turn_pipeline,
            llm_calls,
            failure_evidence,
            pending_queue_claims,
            pending_turn_input_claims,
        } = driver;
        let pending_claims =
            LogicalTurnClaims::new(pending_queue_claims, pending_turn_input_claims);
        let finish_result = Box::pin(
            self.finish_turn(TurnCommitContext {
                finish: TurnFinishInput {
                    turn_pipeline,
                    assembler: assembler
                        .with_llm_calls(llm_calls)
                        .with_failure_evidence(failure_evidence),
                    new_messages,
                    policy: policy.policy,
                    turn_index,
                    trace_turn_id,
                },
                claims: &pending_claims,
                events,
                scoped_effect_controller: &finish_scoped_effect_controller,
                cancel_state: &cancel_state,
                lease: TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
                turn_control: turn_control.as_ref(),
            }),
        )
        .await;
        if let Err(err) = &finish_result {
            self.abandon_queued_work_claims_after_local_abort(err, &pending_claims.queued)
                .await;
            self.abandon_turn_input_claims_after_local_abort(err, &pending_claims.turn_inputs)
                .await;
        }
        finish_result
    }
}
