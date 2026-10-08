//! Suspend sessions: a real generated turn that parks mid-flight on a
//! deferring tool's completion key and resumes only when the boundary
//! scheduler delivers the matching completion.

use super::*;

/// A real generated turn that parks mid-flight on a tool/durable/exec await key
/// and is resumed only when the boundary scheduler delivers the matching
/// completion. This generalizes the fixed `prove_pending_tool_completion_through_turn`
/// proof into the live, interleaved generated search.
pub(super) struct SuspendingTurn {
    core: lash::LashCore,
    handle: tokio::task::JoinHandle<Result<lash::TurnOutput, FixedScriptRunnerError>>,
    events: Arc<RuntimeProofRecordingEvents>,
    key_slot: Arc<tokio::sync::Mutex<Option<lash_core::PinnedKey>>>,
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
    /// The simulated time this turn's resume boundary is scheduled for, fixed
    /// when the turn is spawned. Spawning happens inside a boundary delivery, so
    /// this is a function of the workload; deriving it instead from the driver
    /// pass that later notices the parked await key would make it a function of
    /// how fast the host polled the turn (see `staged_admissions`).
    resolution_at: u64,
    transport: Arc<ScriptedLlmHttpTransport>,
    scripts: Vec<ProviderWireScript>,
    reopen: Arc<dyn lash_core::DeploymentStore>,
}

impl SuspendingTurn {
    pub(super) fn core(&self) -> &lash::LashCore {
        &self.core
    }

    pub(super) fn resolution_scheduled(&self) -> bool {
        self.resolution_scheduled
    }

    pub(super) fn resolution_at(&self) -> u64 {
        self.resolution_at
    }
}

/// What the durable-content oracle needs from a suspend session after its
/// resumed turn finished.
pub(super) struct FinishedSuspend {
    core: lash::LashCore,
    session: String,
    transport: Arc<ScriptedLlmHttpTransport>,
    scripts: Vec<ProviderWireScript>,
    tool_result: crate::content_oracle::ToolResultContent,
    activities: Vec<lash::TurnActivity>,
    reopen: Arc<dyn lash_core::DeploymentStore>,
}

impl FinishedSuspend {
    pub(super) fn core(&self) -> &lash::LashCore {
        &self.core
    }

    pub(super) async fn content(
        &self,
    ) -> Result<crate::content_oracle::SessionContent, FixedScriptRunnerError> {
        session_content(
            &self.session,
            self.transport.as_ref(),
            &self.scripts,
            vec![self.tool_result.clone()],
            &self.activities,
            self.reopen.as_ref(),
        )
        .await
    }
}

impl GeneratedRuntimeWorld {
    /// The turn calls a sim tool that registers its await key and returns
    /// `ToolOutcome::pending`, so the turn future parks mid-flight and cannot finish until the
    /// scheduler later delivers the matching completion boundary.
    /// The observed masquerades as a normal ingress for the abstract store; suspend evidence
    /// lives in a normalized-away field so cross-backend replay stays green.
    pub(super) async fn open_suspending_session(
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
            "payload": event.payload.get("tool_output_text").cloned().unwrap_or(Value::Null),
        });

        let key_slot = Arc::new(tokio::sync::Mutex::new(None));
        let events = Arc::new(RuntimeProofRecordingEvents::default());
        // Route the parked turn through the real openai-compatible provider wire
        // transport (not a TestProvider), so both the tool-call exchange that
        // suspends the turn and the post-resume exchange exercise real provider
        // wire parsing.
        let suspend_scripts = suspend_roundtrip_scripts(&tool_name)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts(
            suspend_scripts.clone(),
        )?);
        let (_engine, backend, reopen) = self.session_engine(&session_alias).await?;
        let (provider_handle, model, _provider_kind) =
            runtime_provider_components(OPENAI_COMPATIBLE, &transport)
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let observer = crate::invariants::ToolObserver::new(self.recorder.clone());
        let core = lash::LashCore::standard_builder(backend)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(world_queued_work_batching())
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .live_replay_store(world_live_replay_store())
            .serve_test_llm_profile(provider_handle, model.clone())
            .tools(Arc::new(SuspendToolProvider::new(
                tool_name.clone(),
                Arc::clone(&key_slot),
                observer,
            )) as Arc<dyn lash_core::ToolProvider>)
            .build(crate::sim_process_owner())
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let session = crate::open_created_session(
            model.wire_model.clone(),
            &core,
            SessionId::fixture(session_alias.clone()),
        )
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let prompt = format!("await {suspend_kind_label} completion");
        // The turn's input is accepted inside this boundary's delivery, so
        // the order in which suspend sessions are sent is the boundaries'
        // own; only the wait for the engine's answer runs as a task.
        let accepted = session
            .durable()
            .send(lash::TurnInput::text(prompt))
            .id(lash_core::TurnId::fixture(format!(
                "{session_alias}:suspend-turn"
            )))
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let turn_events: Arc<dyn lash::TurnActivitySink> = events.clone();
        let handle = tokio::spawn(async move {
            crate::backend::settle_handle(accepted, turn_events)
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
        });
        let resolution_at = SUSPEND_RESOLUTION_BASE_AT + self.suspends_spawned;
        self.suspends_spawned += 1;
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
                resolution_at,
                transport,
                scripts: suspend_scripts,
                reopen,
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

    /// Once a turn has registered its await key (it parked on the tool) and is still in
    /// flight, stage the matching completion boundary — mirroring how finished provider turns
    /// stage their completion.
    /// The completion is the only thing that can resume the parked turn, and it is delivered
    /// after the generated workload has drained, so it is scheduled past every workload
    /// boundary.
    ///
    /// Its `at` is `resolution_at`, fixed from the spawn-order counter when the
    /// delivery that spawned the turn ran — not from how many driver passes had
    /// gone by when the host noticed the await key. Which pass notices is
    /// decided by task-poll progress, so a counter advanced by the driver loop
    /// would put host timing into recorded simulator evidence (see
    /// `staged_admissions`). Resolutions are independent of one another, so
    /// ordering them by spawn order loses nothing.
    pub(in crate::runner) async fn schedule_parked_suspend_resolutions(
        &mut self,
        scheduler: &mut BoundaryScheduler,
    ) -> Result<(), FixedScriptRunnerError> {
        tokio::task::yield_now().await;
        let mut staged = Vec::new();
        for (session_alias, turn) in self.suspending_turns.iter_mut() {
            if turn.resolution_scheduled {
                continue;
            }
            let key_present = turn.key_slot.lock().await.is_some();
            if !key_present {
                continue;
            }
            let ready_at = turn.resolution_at;
            // The await key exists, so the tool parked the turn. Record that the
            // turn suspended before any completion was delivered.
            let suspended =
                !turn.handle.is_finished() && turn.events.tool_completed_count().await == 0;
            turn.suspended_before_completion = Some(suspended);
            turn.completed_before_resolution = turn.events.tool_completed_count().await;
            let boundary_id = format!("{session_alias}:suspend-resume:001");
            let label = format!("suspend.{}.resume", boundary_kind_label(turn.suspend_kind));
            staged.push(BoundaryEvent::new(
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
        for event in staged {
            self.stage_admission(event);
        }
        self.flush_staged_admissions(scheduler);
        Ok(())
    }

    /// Resolve a parked suspend turn via `core.completions().resolve(...)` when
    /// the scheduler delivers its completion boundary, then await the resumed
    /// turn to completion. The observed masquerades as the matching runtime
    /// boundary (tool/exec/durable) for the abstract store, with suspend/resume
    /// evidence in a normalized-away field.
    pub(super) async fn resolve_suspended_turn(
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
        self.recorder
            .record(crate::invariants::Fact::CompletionResolved {
                key: crate::invariants::completion_key_label(&key),
                session: event.actor_alias.clone(),
                result_digest: crate::invariants::result_digest(
                    &crate::content_oracle::ToolResultContent::from_tool_value(
                        crate::runtime_providers::SUSPEND_TOOL_CALL_ID,
                        &turn.tool_name,
                        &resolution,
                    )
                    .content,
                ),
            });
        let accepted = turn
            .core
            .completions()
            .resolve(key.as_str(), lash_core::Resolution::Ok(resolution.clone()))
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let output = turn.handle.await.map_err(|err| {
            FixedScriptRunnerError::Runtime(format!(
                "suspend turn `{}` task failed to join: {err}",
                event.actor_alias
            ))
        })??;
        let result = &output.result;
        let completed_after = turn.events.tool_completed_count().await;
        let activities = turn.events.snapshot().await;
        let assistant_message = result.assistant_message().unwrap_or_default().to_string();
        let read_view = result.state.read_view();
        let graph_invariant = runtime_graph_invariant_facts(&result.state.session_graph);
        let usage_invariant = runtime_usage_invariant_facts(result, &activities);
        let resumed_after_completion = completed_after > completed_before
            && matches!(
                &result.outcome,
                lash_core::facade_support::TurnOutcome::Finished(
                    lash_core::facade_support::TurnFinish::AssistantMessage { .. }
                )
            );
        let resolve_accepted = matches!(accepted, lash_core::ResolveAnswer::Resolved);
        let observed = json!({
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
            "graph_node_count": result.state.session_graph.nodes.len(),
            "transcript_message_count": read_view.messages().len(),
            "runtime_invariant_facts": {
                "graph": graph_invariant,
                "usage": usage_invariant,
            },
        });
        self.finished_suspends.push(FinishedSuspend {
            core: turn.core,
            session: event.actor_alias.clone(),
            transport: Arc::clone(&turn.transport),
            scripts: turn.scripts.clone(),
            tool_result: crate::content_oracle::ToolResultContent::from_tool_value(
                crate::runtime_providers::SUSPEND_TOOL_CALL_ID,
                &turn.tool_name,
                &resolution,
            ),
            activities,
            reopen: turn.reopen,
        });
        Ok(observed)
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

/// A sim tool that registers its await key in a shared slot the generated world
/// can read, then returns `ToolOutcome::pending` so the calling turn parks until
/// the scheduler resolves the key. Generalizes `PendingToolProvider` for the
/// generated suspend sessions (Tool / DurableEffect / ExecCode).
struct SuspendToolProvider {
    tool_name: String,
    key_slot: Arc<tokio::sync::Mutex<Option<lash_core::PinnedKey>>>,
    observer: crate::invariants::ToolObserver,
}

impl SuspendToolProvider {
    fn new(
        tool_name: String,
        key_slot: Arc<tokio::sync::Mutex<Option<lash_core::PinnedKey>>>,
        observer: crate::invariants::ToolObserver,
    ) -> Self {
        Self {
            tool_name,
            key_slot,
            observer,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "this module declares the tool or payload schema and admission checks its invariant"
    )]
    fn definition(&self) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            format!("tool:{}", self.tool_name),
            self.tool_name.clone(),
            "Await an externally-resolved completion.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_declaration(lash_core::ToolDeclaration::deferring())
        .with_park(lash_core::ParkBound::Within(
            std::time::Duration::from_secs(120),
        ))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SuspendToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.tool_name).then(|| Arc::new(self.definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() != self.tool_name {
            return lash_core::ToolOutcome::err_fmt(format_args!("unknown tool {}", call.name()))
                .into();
        }
        let observed = self.observer.executed(call.context);
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        self.observer.registered(observed, &key);
        *self.key_slot.lock().await = Some(key);
        lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into()
    }
}
