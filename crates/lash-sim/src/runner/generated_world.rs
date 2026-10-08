//! The generated runtime world: a workload's sessions, each a real core on
//! lash's durable engine, driven boundary by boundary by the scheduler.

use super::runtime_boundaries::RuntimeBoundaryHarness;
use super::*;
use crate::backend_fault::GeneratedBackendFaultHarness;
use crate::scheduler::QueuedIngressMode;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

mod provider_turn;
mod suspend;

use suspend::{FinishedSuspend, SuspendingTurn};

/// Suspend resolutions resume a turn only after the generated workload has
/// drained, so they are scheduled past every workload boundary. The generator's
/// boundary times are far below this.
const SUSPEND_RESOLUTION_BASE_AT: u64 = 1_000_000;

pub(super) struct GeneratedRuntimeWorld {
    clock: Arc<SimClock>,
    sessions: BTreeMap<String, GeneratedRuntimeSession>,
    queued_inputs: BTreeMap<String, String>,
    backend_faults: GeneratedBackendFaultHarness,
    provider_mutations: SimProviderMutationHarness,
    /// The workload's seed: every engine of the world derives its own from
    /// it.
    seed: u64,
    /// The world's own engine, under the workload's seed: work lands in
    /// its store. No core runs on it.
    engine: crate::backend::SimEngine,
    /// Each session's engine, by session alias. Each session, with its own
    /// provider and tools, runs on its own engine, so the core built over
    /// it serves that session alone.
    session_engines: BTreeMap<String, crate::backend::SimEngine>,
    durable_writes: CheckpointWriteCollector,
    runtime_boundaries: RuntimeBoundaryHarness,
    suspending_turns: BTreeMap<String, SuspendingTurn>,
    /// Boundaries the host has discovered but the simulated schedule has not
    /// reached yet.
    ///
    /// A provider turn and a parked suspend turn both run as spawned tasks, so
    /// *when* the host observes one as finished (or as having registered its
    /// await key) depends on real task-poll progress, not on the simulated
    /// schedule. Scheduling the matching boundary straight from that discovery
    /// let poll speed decide how many boundaries were pending when an *earlier*
    /// boundary was delivered: the discovered boundary carries an `at` in the
    /// future, so delivery order never moved, but the `scheduler.pending_before`
    /// recorded for every delivery in between shifted by one. That is the
    /// FIG-3053 timing case: host load changes when a future completion
    /// appears in the queue, even though its virtual delivery time is later.
    ///
    /// Staging here and admitting on a logical condition
    /// (`flush_staged_admissions`) keeps host poll speed from deciding when a
    /// discovered boundary enters the scheduler. The surrounding concurrent
    /// interleaving can still differ between runs of the same seed.
    staged_admissions: BTreeMap<String, BoundaryEvent>,
    suspends_spawned: u64,
    /// Resolved suspend sessions, kept for the durable-content oracle.
    finished_suspends: Vec<FinishedSuspend>,
    /// What the world's own tools and host record for the global invariants.
    recorder: crate::invariants::HistoryRecorder,
    /// When set, the driver admits at most one live provider turn at a time
    /// (see `RuntimeCompletionState::serialize_provider_turns`). Enabled for the
    /// cross-backend durable re-run; left off for the concurrent search run.
    pub(super) serialize_provider_turns: bool,
}

struct GeneratedRuntimeSession {
    core: lash::LashCore,
    /// The session's own engine: its core's node runs every turn of it.
    engine: crate::backend::SimEngine,
    /// The engine's session factory, for reading the session back.
    reopen: Arc<dyn lash_core::DeploymentStore>,
    /// The session as the host reaches it durably: its sends, reads and
    /// pending-input cancels.
    durable: lash::DurableSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_schedule: ScriptedTransportSchedule,
    provider_scripts: Vec<ProviderWireScript>,
    provider_kind: String,
    active_provider_turns: BTreeMap<String, ActiveProviderTurn>,
    finished_provider_turns: BTreeMap<String, Value>,
    /// Every modeled provider turn that has started: a turn input may
    /// address (ADR 0101 §5.1), running or ended.
    started_provider_turns: BTreeSet<String>,
    /// The session's actor, held from a queued ingress until its paired
    /// cancellation (or the next modeled provider turn): the engine runs an
    /// input as soon as its actor drains it, and each modeled provider turn
    /// owns exactly one scripted exchange, so an input the model queues must
    /// stay pending on the held actor until the model withdraws it or a
    /// provider turn's run takes it with its own input.
    hold: Option<crate::session_hold::SessionHold>,
    /// Every activity the engine delivered for the session's turns: its
    /// model call records are the session's recorded attempts.
    activities: Arc<Mutex<Vec<lash::TurnActivity>>>,
}

struct ActiveProviderTurn {
    completion_event: BoundaryEvent,
    handle: tokio::task::JoinHandle<Result<Value, FixedScriptRunnerError>>,
    final_ready_at: u64,
    logical_ms_at_start: u64,
}

const SCHEDULE_TICK_MS: u64 = 40_000;

impl GeneratedRuntimeWorld {
    /// A world over a fresh engine of its own.
    pub(super) async fn new(seed: u64) -> Result<Self, FixedScriptRunnerError> {
        let engine = crate::backend::SimEngine::new(seed).await?;
        Ok(Self {
            clock: SimClock::new(),
            sessions: BTreeMap::new(),
            queued_inputs: BTreeMap::new(),
            backend_faults: GeneratedBackendFaultHarness::default(),
            provider_mutations: SimProviderMutationHarness::default(),
            runtime_boundaries: RuntimeBoundaryHarness::new(seed),
            seed,
            engine,
            session_engines: BTreeMap::new(),
            durable_writes: CheckpointWriteCollector::default(),
            suspending_turns: BTreeMap::new(),
            staged_admissions: BTreeMap::new(),
            suspends_spawned: 0,
            finished_suspends: Vec::new(),
            recorder: crate::invariants::HistoryRecorder::default(),
            serialize_provider_turns: false,
        })
    }

    /// A fresh engine for `alias`'s session, under a seed derived from the
    /// workload's and the alias, so one workload builds the same engines on
    /// every run: its backend (session factory under the world's commit
    /// observer) and its own session factory for reading back.
    async fn session_engine(
        &mut self,
        alias: &str,
    ) -> Result<
        (
            crate::backend::SimEngine,
            lash::Backend,
            Arc<dyn lash_core::DeploymentStore>,
        ),
        FixedScriptRunnerError,
    > {
        let seed = alias
            .bytes()
            .fold(self.seed ^ 0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
        let engine = crate::backend::SimEngine::new(seed).await?;
        let reopen = engine.backend().session_store_factory();
        let backend: lash::Backend = crate::backend::DecoratedBackend::over_engine(&engine)
            .observing(self.durable_writes.clone())
            .into();
        self.session_engines
            .insert(alias.to_string(), engine.clone());
        Ok((engine, backend, reopen))
    }

    /// The run's history for the global invariants: the delivered boundaries,
    /// the effects they ran with their counted executions, what the world's
    /// tools and host recorded, and every engine's final store.
    ///
    /// The history ends as a stopped deployment's: the world first stops
    /// every core it built ([`Self::stop_cores`]), so the stores are never
    /// read inside a recovery pass. No pass runs after, so a cleanup no pass
    /// delivered stays due for the next deployment's.
    pub(super) async fn global_history(
        &mut self,
        scenario: &str,
        events: &[crate::scheduler::DeliveredBoundary],
    ) -> Result<crate::invariants::History, String> {
        use crate::invariants::Fact;
        let mut history = crate::invariants::History::new(scenario, self.seed);
        // A cancellation names the queued-ingress boundary it withdraws.
        let mut queued_inputs = BTreeMap::new();
        for event in events {
            let input = event
                .observed
                .get("input_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    let target = event.payload.get("target").and_then(Value::as_str)?;
                    queued_inputs.get(target).cloned()
                });
            if let Some(input) = &input {
                queued_inputs.insert(event.boundary_id.clone(), input.clone());
            }
            history.push(Fact::Boundary {
                boundary_id: event.boundary_id.clone(),
                actor: event.actor_alias.clone(),
                kind: event.kind.to_string(),
                label: event.label.clone(),
                input,
            });
            if matches!(
                event.kind,
                BoundaryKind::Tool | BoundaryKind::ExecCode | BoundaryKind::DurableEffect
            ) && let Some(executions) = event
                .observed
                .get("execution_count")
                .and_then(Value::as_u64)
            {
                // A tool or exec-code boundary's body runs once on a live
                // run; a durable effect's first run stops only after the
                // engine recorded its outcome. No run dies inside the window.
                history.push(Fact::EffectRan {
                    effect: event.boundary_id.clone(),
                    executions: usize::try_from(executions).unwrap_or(usize::MAX),
                    unrecorded_attempts: 0,
                });
            }
        }
        history.extend_from(&self.recorder);
        self.stop_cores().await?;
        history.now_ms = Some(self.engine.backend().clock().timestamp_ms());
        history
            .capture_store_with_transcripts("world", self.engine.stores())
            .await?;
        for (alias, engine) in &self.session_engines {
            history
                .capture_store_with_transcripts(alias.clone(), engine.stores())
                .await?;
        }
        for (label, stores) in self.runtime_boundaries.stores() {
            history
                .capture_store_with_transcripts(label, stores)
                .await?;
        }
        Ok(history)
    }

    /// Shut every session's and suspend's core down, then wait out the
    /// artifact-cleanup pass a core still had in flight: shutdown stops a
    /// core's later passes, not the one it is running, and that pass ends
    /// within one attempt budget of its claims (ADR 0109 §1.8). A row still
    /// claimed past that is left for `obligations-settled-or-stalled` to
    /// judge.
    async fn stop_cores(&mut self) -> Result<(), String> {
        let cores = self
            .sessions
            .values()
            .map(|session| &session.core)
            .chain(self.finished_suspends.iter().map(FinishedSuspend::core))
            .chain(self.suspending_turns.values().map(SuspendingTurn::core));
        for core in cores {
            core.shutdown().await.map_err(|err| err.to_string())?;
        }
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(
                lash_core::runtime::obligations::relay::RelayPolicy::DEFAULT_ATTEMPT_BUDGET_MS,
            );
        let stores = std::iter::once(&self.engine)
            .chain(self.session_engines.values())
            .map(crate::backend::SimEngine::stores)
            .collect::<Vec<_>>();
        loop {
            let mut claimed = false;
            for stores in &stores {
                let snapshot = crate::invariants::StoreSnapshot::read("stop-cores", stores)?;
                claimed |= snapshot.obligations.iter().any(|row| {
                    row.table == crate::invariants::RELAY_ONLY_OBLIGATIONS
                        && row.state.as_deref() == Some("claimed")
                });
            }
            if !claimed || tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    pub(super) fn checkpoint_write_events(&self) -> Vec<CheckpointWriteEvent> {
        self.durable_writes.events()
    }

    pub(super) fn checkpoint_write_collector(&self) -> CheckpointWriteCollector {
        self.durable_writes.clone()
    }

    /// Emitted, delivered and reopened content for every runtime and suspend
    /// session, each read back through its own engine's storage.
    pub(super) async fn content_evidence(
        &self,
    ) -> Result<Vec<crate::content_oracle::SessionContent>, FixedScriptRunnerError> {
        let mut sessions = Vec::new();
        for (alias, session) in &self.sessions {
            let activities = session.activities.lock_recover().clone();
            sessions.push(
                session_content(
                    alias,
                    session.transport.as_ref(),
                    &session.provider_scripts,
                    Vec::new(),
                    &activities,
                    session.reopen.as_ref(),
                )
                .await?,
            );
        }
        for suspend in &self.finished_suspends {
            sessions.push(suspend.content().await?);
        }
        Ok(sessions)
    }

    pub(super) async fn advance_time_for_boundary(&self, event: &BoundaryEvent) {
        let schedule_time = if event.kind == BoundaryKind::ProviderEvent {
            self.clock.logical_ms().saturating_add(SCHEDULE_TICK_MS)
        } else {
            event.at
        };
        self.clock.advance_to(schedule_time).await;
    }

    pub(super) fn pending_suspend_turn_count(&self) -> usize {
        self.suspending_turns
            .values()
            .filter(|turn| !turn.resolution_scheduled())
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
            BoundaryKind::ContractExecution => Ok(event.payload.clone()),
            BoundaryKind::Ingress => {
                if event.payload.get("suspend_kind").is_some() {
                    self.open_suspending_session(event).await
                } else {
                    self.open_runtime_session(event).await
                }
            }
            BoundaryKind::QueuedIngress => self.queue_turn_input(event).await,
            BoundaryKind::Provider => self.finish_provider_turn(event),
            BoundaryKind::ProviderEvent => self.release_provider_event(event),
            BoundaryKind::Observer => self.observe_session(event).await,
            BoundaryKind::Cancellation => self.cancel_queued_input(event).await,
            BoundaryKind::BackendFailure => self.backend_faults.inject(event).await,
            BoundaryKind::ProviderMutation => self.provider_mutations.reject(event).await,
            BoundaryKind::DurableEffect | BoundaryKind::Tool | BoundaryKind::ExecCode => self
                .runtime_boundaries
                .deliver(event)
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string())),
        }
    }

    async fn open_runtime_session(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let provider_turns = scripted_turns_from_ingress(&event.payload).map_err(|err| {
            FixedScriptRunnerError::Assertion(format!(
                "ingress boundary `{}` {err}",
                event.boundary_id
            ))
        })?;
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
        let scripts = runtime_scripts_for_turns(provider_kind, &provider_turns)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let provider_scripts = scripts.clone();
        let (engine, backend, reopen) = self.session_engine(&event.actor_alias).await?;
        let provider_schedule = ScriptedTransportSchedule::new();
        let (core, transport, provider_kind, model) =
            runtime_core_for_scripts(scripts, backend, Some(provider_schedule.clone()))?;
        let session = crate::open_created_session(
            model,
            &core,
            SessionId::fixture(event.actor_alias.clone()),
        )
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
                core,
                engine,
                reopen,
                durable: session.durable(),
                transport,
                provider_schedule,
                provider_scripts,
                provider_kind,
                active_provider_turns: BTreeMap::new(),
                finished_provider_turns: BTreeMap::new(),
                started_provider_turns: BTreeSet::new(),
                hold: None,
                activities: Arc::new(Mutex::new(Vec::new())),
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
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
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
        if runtime_session.hold.is_none() {
            runtime_session.hold = Some(
                runtime_session
                    .engine
                    .hold_session(&SessionId::fixture(event.actor_alias.clone()))?,
            );
        }
        let mut send = runtime_session
            .durable
            .send(lash::TurnInput::text(text.to_string()))
            .id(lash_core::TurnId::fixture(source_key));
        let observed_active_turn_id = event
            .payload
            .get("active_turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let planned_mode = event
            .queued_ingress_mode()
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let active_turn_id = observed_active_turn_id
            .as_deref()
            .unwrap_or(&event.boundary_id);
        // A host addresses a turn that is running or has ended (ADR 0101
        // §5.1). A modeled provider turn is its own run, so its boundary id
        // is the turn's id; one that has not started yet, as serialized
        // provider turns can leave it, has nothing to address, and the input
        // is next-turn input.
        let ingress_mode = if planned_mode == QueuedIngressMode::ActiveTurn
            && !runtime_session
                .started_provider_turns
                .contains(active_turn_id)
        {
            QueuedIngressMode::NextTurn
        } else {
            planned_mode
        };
        if ingress_mode == QueuedIngressMode::ActiveTurn {
            send = send.ingress(lash_core::TurnInputIngress::active_turn(
                lash_core::TurnId::fixture(active_turn_id),
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ));
        }
        let acceptance = send
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
            .receipt()
            .clone();
        self.queued_inputs
            .insert(event.boundary_id.clone(), acceptance.input_id.to_string());
        let input_state = lash_core::TurnInputState::open(acceptance.ingress.clone());
        Ok(json!({
            "session": event.actor_alias,
            "queued_ingress": true,
            "source_key": source_key,
            "input_id": acceptance.input_id,
            "input_state": input_state.as_str(),
            "ingress_mode": ingress_mode.as_str(),
            "active_turn_id": observed_active_turn_id,
        }))
    }

    fn stage_admission(&mut self, event: BoundaryEvent) {
        self.staged_admissions
            .insert(event.boundary_id.clone(), event);
    }

    /// The earliest simulated time still owed to the scheduler by work the host
    /// has not admitted yet: a live provider turn's completion, a staged
    /// boundary, or a suspend turn's resume.
    ///
    /// Every one of those times is fixed before the work starts, and each piece
    /// of work contributes its time from the moment it is created until the
    /// moment it is admitted — whether the host has noticed it finishing yet or
    /// not. That is what makes this a property of the simulation rather than of
    /// task-poll progress, and it is why `flush_staged_admissions` can use it to
    /// decide admission.
    fn min_unadmitted_at(&self) -> Option<u64> {
        self.sessions
            .values()
            .flat_map(|session| session.active_provider_turns.values())
            .map(|active| active.final_ready_at)
            .chain(self.staged_admissions.values().map(|event| event.at))
            .chain(
                self.suspending_turns
                    .values()
                    .map(SuspendingTurn::resolution_at),
            )
            .min()
    }

    /// Admit every staged boundary the simulated schedule has reached.
    ///
    /// A staged boundary is "reached" when two things hold: nothing pending is
    /// scheduled strictly before it, and no unadmitted work is owed to an
    /// earlier time. The second half is the one that matters — without it, a
    /// scheduler that drains transiently while provider turns are still live
    /// would admit a far-future boundary (a suspend resume) the instant the host
    /// happened to discover it, which is exactly the host timing this is meant
    /// to keep out. With both, admission reads only times the simulation fixed
    /// in advance, so it — and every `pending_before` the scheduler records —
    /// lands at the same point on every run of a seed.
    ///
    /// This cannot stall: whatever owns the earliest unadmitted time is either
    /// already staged (admitted right here), a live provider turn (the driver's
    /// delivery barrier spins on `schedule_finished_provider_turns` until it
    /// lands), or a suspend turn yet to park (the driver spins on
    /// `schedule_parked_suspend_resolutions` while any remains).
    fn flush_staged_admissions(&mut self, scheduler: &mut BoundaryScheduler) {
        loop {
            let next_pending_at = scheduler.min_pending_at();
            let earliest_unadmitted = self.min_unadmitted_at();
            let Some(ready_id) = self
                .staged_admissions
                .iter()
                .find(|(_, event)| {
                    next_pending_at.is_none_or(|at| at >= event.at)
                        && earliest_unadmitted.is_none_or(|at| at >= event.at)
                })
                .map(|(id, _)| id.clone())
            else {
                return;
            };
            if let Some(event) = self.staged_admissions.remove(&ready_id) {
                scheduler.schedule(event);
            }
        }
    }

    /// The driver asserts this before it treats an empty scheduler as the end of the run, so a
    /// future interleaving of suspend and workload times cannot silently drop a staged
    /// boundary.
    pub(super) fn staged_admissions_is_empty(&self) -> bool {
        self.staged_admissions.is_empty()
    }

    async fn observe_session(
        &self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
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
        // An observer reads the session the store committed, as a host that
        // reconnects does.
        let read_view = runtime_session
            .durable
            .read()
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "observer boundary `{}` found no committed session `{}`",
                    event.boundary_id, event.actor_alias
                ))
            })?;
        let graph_node_count = read_view.session_graph().nodes.len();
        let transcript_message_count = read_view.messages().len();
        let graph_non_empty = graph_node_count > 0;
        let observer_ok = *read_view.session_id() == event.actor_alias
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
        let runtime_session = self.sessions.get_mut(&event.actor_alias).ok_or_else(|| {
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
        let receipts = runtime_session
            .durable
            .cancel_pending_turn_inputs([lash_core::PendingTurnInputCancelTarget::input_id(
                input_id,
            )])
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        // The model's queue-then-withdraw pair is over: the engine may work
        // the session again.
        drop(runtime_session.hold.take());
        let [receipt] = receipts.as_slice() else {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "cancellation boundary `{}` answered {} receipts for one target",
                event.boundary_id,
                receipts.len()
            )));
        };
        let (cancelled, cancel_outcome) = match &receipt.outcome {
            lash::PendingTurnInputCancelOutcome::Cancelled(_) => (true, "cancelled"),
            lash::PendingTurnInputCancelOutcome::AlreadyAdmitted { .. } => {
                (false, "already_admitted")
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
}

/// Emitted, committed and reopened content for one session whose provider ran
/// `scripts` in order over `transport`, with the call records the engine
/// delivered in `activities`.
pub(super) async fn session_content(
    session: &str,
    transport: &ScriptedLlmHttpTransport,
    scripts: &[ProviderWireScript],
    emitted_tool_results: Vec<crate::content_oracle::ToolResultContent>,
    activities: &[lash::TurnActivity],
    reopen: &dyn lash_core::DeploymentStore,
) -> Result<crate::content_oracle::SessionContent, FixedScriptRunnerError> {
    let exchanged = transport.exchanges()?.len();
    let emitted_attempts = scripts
        .get(..exchanged)
        .ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "`{session}` recorded {exchanged} provider exchanges for {} scripts",
                scripts.len()
            ))
        })?
        .iter()
        .map(crate::content_oracle::emitted_attempt)
        .collect::<Result<Vec<_>, _>>()
        .map_err(FixedScriptRunnerError::Assertion)?;
    Ok(crate::content_oracle::SessionContent {
        session: session.to_string(),
        emitted_attempts,
        emitted_tool_results,
        recorded_usage: crate::content_oracle::recorded_usage(activities)
            .map_err(FixedScriptRunnerError::Assertion)?,
        reopened: crate::content_oracle::reopen_session(reopen, session)
            .await
            .map_err(FixedScriptRunnerError::Assertion)?,
    })
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
