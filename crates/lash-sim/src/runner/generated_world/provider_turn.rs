//! A modeled provider turn: sent to its session, released through its
//! scripted transport one scheduled wire event at a time, and harvested once
//! the engine settled it.

use super::*;

impl GeneratedRuntimeWorld {
    pub(in crate::runner) async fn start_provider_turn(
        &mut self,
        event: BoundaryEvent,
        completion_event: BoundaryEvent,
        scheduler: &mut BoundaryScheduler,
        queued_next_turn_boundaries: &[String],
    ) -> Result<(), FixedScriptRunnerError> {
        let expected_admissions = queued_next_turn_boundaries
            .iter()
            .map(|boundary| {
                self.queued_inputs.get(boundary).cloned().ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "queued input boundary `{boundary}` has no runtime input id"
                    ))
                })
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
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
        for (event_index, wire_event) in script.timeline().iter().enumerate() {
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

        let session = runtime_session.durable.clone();
        let transport = Arc::clone(&runtime_session.transport);
        let provider_kind = runtime_session.provider_kind.clone();
        let activities = Arc::clone(&runtime_session.activities);
        let task_event = event.clone();
        // A queued input the model still holds pending is admitted with this
        // turn's input: the hold ends once this input is accepted, and the
        // run that drains the session takes both.
        let hold = runtime_session.hold.take();
        let mut handle = tokio::spawn(async move {
            run_provider_turn_task(
                session,
                transport,
                provider_kind,
                activities,
                task_event,
                hold,
            )
            .await
        });
        tokio::select! {
            ready = async {
                runtime_session.provider_schedule.wait_until_blocked(exchange_index, 0).await;
                // The same model rows that admission will take must now be
                // visible as admitted in the runtime's pending-input view. No
                // boundary is delivered during this wait and the first wire
                // gate is closed, so these rows cannot have been cancelled or
                // completed. An empty admission needs no store read.
                while !expected_admissions.is_empty() {
                    let pending = runtime_session.durable.pending_turn_inputs().await
                        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
                    if expected_admissions.iter().all(|input_id| {
                        pending.iter().any(|read| {
                            &read.input.input_id == input_id
                                && matches!(read.status, lash::PendingTurnInputReadStatus::Admitted { .. })
                        })
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                Ok::<(), FixedScriptRunnerError>(())
            } => { ready?; }
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
        runtime_session
            .started_provider_turns
            .insert(event.boundary_id.clone());
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

    pub(super) fn release_provider_event(
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

    pub(super) fn finish_provider_turn(
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

    pub(in crate::runner) async fn schedule_finished_provider_turns(
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
                // `min_unadmitted_at` orders provider completions against
                // suspend resumes by time alone, so the two ranges must stay
                // disjoint: a completion at or past the suspend base would let a
                // resume be admitted while a workload boundary is still owed.
                debug_assert!(
                    final_ready_at < SUSPEND_RESOLUTION_BASE_AT,
                    "provider completion at {final_ready_at} reached the suspend-resume range"
                );
                self.stage_admission(completion_event);
            }
        }
        self.flush_staged_admissions(scheduler);
        Ok(())
    }

    pub(in crate::runner) fn active_provider_turn_count(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.active_provider_turns.len())
            .sum()
    }

    /// The earliest completion time (`final_ready_at`) across all live provider
    /// turns, or `None` when none is live. This is the delivery
    /// barrier: the driver holds back any boundary scheduled at or after this time
    /// until the turn finishes and its completion lands in the scheduler, so the
    /// completion is always delivered at its own `at` ahead of later boundaries —
    /// making the delivery order independent of how long the store takes to
    /// commit the turn.
    pub(in crate::runner) fn min_active_final_ready_at(&self) -> Option<u64> {
        self.sessions
            .values()
            .flat_map(|session| session.active_provider_turns.values())
            .map(|active| active.final_ready_at)
            .min()
    }
}

pub(in crate::runner) fn provider_release_boundary(
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

/// The activity of a session's turns, kept for its recorded attempts.
struct SessionActivity(Arc<Mutex<Vec<lash::TurnActivity>>>);

#[async_trait::async_trait]
impl lash::TurnActivitySink for SessionActivity {
    async fn emit(&self, activity: lash::TurnActivity) {
        self.0.lock_recover().push(activity);
    }
}

async fn run_provider_turn_task(
    session: lash::DurableSession,
    transport: Arc<ScriptedLlmHttpTransport>,
    provider_kind: String,
    activities: Arc<Mutex<Vec<lash::TurnActivity>>>,
    event: BoundaryEvent,
    hold: Option<crate::session_hold::SessionHold>,
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
    let prompt = format!("Run generated provider turn {}.", event.boundary_id);
    let accepted = session
        .send(lash::TurnInput::text(prompt))
        .id(lash_core::TurnId::fixture(event.boundary_id.clone()))
        .await;
    // Once this turn's input is accepted, the run that drains the session
    // takes it with every input the hold kept pending.
    drop(hold);
    let runtime_error = |err: lash::EmbedError| {
        FixedScriptRunnerError::Runtime(format!(
            "runtime turn `{}` error: {err}",
            event.boundary_id
        ))
    };
    let output = crate::backend::settle_handle(
        accepted.map_err(runtime_error)?,
        Arc::new(SessionActivity(activities)),
    )
    .await
    .map_err(runtime_error)?;
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
        &SessionId::fixture(event.actor_alias.clone()),
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
            "graph_acyclic": graph_invariant.cycle_node_ids.is_empty(),
            "single_active_agent_frame": agent_frame_invariant.active_frame_ids.len() == 1,
            "usage_monotonic": usage_invariant.usage_events_monotonic,
            "transcript_contains_provider_output": read_view.messages().iter().any(|message| {
                message.parts.iter().any(|part| part.content().contains(expected_text))
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
