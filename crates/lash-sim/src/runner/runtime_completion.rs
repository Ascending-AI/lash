use super::*;

pub const SCHEDULER_OWNED_RUNTIME_COMPLETION_KINDS: &[BoundaryKind] = &[
    BoundaryKind::Provider,
    BoundaryKind::Cancellation,
    BoundaryKind::BackendFailure,
    BoundaryKind::ProviderMutation,
    BoundaryKind::Tool,
    BoundaryKind::ExecCode,
    BoundaryKind::DurableEffect,
    BoundaryKind::Observer,
];

#[derive(Default)]
pub(super) struct RuntimeCompletionState {
    pub(super) opened_sessions: BTreeSet<String>,
    pub(super) queued_boundaries: BTreeSet<String>,
    pub(super) provider_completions_by_session: BTreeMap<String, usize>,
    pub(super) active_provider_turns_by_session: BTreeMap<String, usize>,
    /// When set, at most ONE live provider turn is admitted across ALL sessions,
    /// and a boundary that runs an effect in its own handler (tool, exec-code,
    /// durable effect) waits until none is live. This is the SERIAL lane's
    /// discipline: the server double runs one attempt at a time, and a live
    /// turn parked on a scripted provider gate holds that turn, so a second
    /// handler admitted beside it could only start by preempting it. The
    /// generated SEARCH lane leaves this OFF and keeps full preserved
    /// concurrency (and its interleaving oracle) for concurrency fuzzing.
    pub(super) serialize_provider_turns: bool,
}

impl RuntimeCompletionState {
    pub(super) fn provider_started(&mut self, actor_alias: &str) {
        *self
            .active_provider_turns_by_session
            .entry(actor_alias.to_string())
            .or_default() += 1;
    }

    pub(super) fn observe(&mut self, event: &crate::scheduler::DeliveredBoundary) {
        match event.kind {
            BoundaryKind::Ingress => {
                self.opened_sessions.insert(event.actor_alias.clone());
            }
            BoundaryKind::QueuedIngress => {
                self.queued_boundaries.insert(event.boundary_id.clone());
            }
            BoundaryKind::Provider => {
                *self
                    .provider_completions_by_session
                    .entry(event.actor_alias.clone())
                    .or_default() += 1;
                if let Some(active) = self
                    .active_provider_turns_by_session
                    .get_mut(&event.actor_alias)
                {
                    *active = active.saturating_sub(1);
                }
            }
            BoundaryKind::ProviderEvent
            | BoundaryKind::Tool
            | BoundaryKind::ExecCode
            | BoundaryKind::DurableEffect
            | BoundaryKind::Observer
            | BoundaryKind::Cancellation
            | BoundaryKind::Trigger
            | BoundaryKind::BackendFailure
            | BoundaryKind::ProviderMutation => {}
        }
    }

    fn session_opened(&self, actor_alias: &str) -> bool {
        self.opened_sessions.contains(actor_alias)
    }

    fn provider_completed(&self, actor_alias: &str) -> bool {
        self.provider_completed_count(actor_alias) > 0
    }

    fn provider_completed_count(&self, actor_alias: &str) -> usize {
        self.provider_completions_by_session
            .get(actor_alias)
            .copied()
            .unwrap_or(0)
    }

    fn provider_active(&self, actor_alias: &str) -> bool {
        self.active_provider_turns_by_session
            .get(actor_alias)
            .copied()
            .unwrap_or(0)
            > 0
    }

    fn any_provider_active(&self) -> bool {
        self.active_provider_turns_by_session
            .values()
            .any(|&count| count > 0)
    }

    fn next_provider_turn_ready(&self, event: &BoundaryEvent) -> bool {
        if !self.session_opened(&event.actor_alias) || self.provider_active(&event.actor_alias) {
            return false;
        }
        let completed = self
            .provider_completions_by_session
            .get(&event.actor_alias)
            .copied()
            .unwrap_or(0);
        let Some(turn_index) = event.payload.get("turn_index").and_then(Value::as_u64) else {
            return true;
        };
        turn_index as usize == completed.saturating_add(1)
    }

    fn queued_boundary_exists(&self, boundary_id: &str) -> bool {
        self.queued_boundaries.contains(boundary_id)
    }

    /// Whether a boundary that runs its own handler may start now: never
    /// beside a live provider turn of its session, and under the serial
    /// discipline never beside any live provider turn.
    fn handler_boundary_ready(&self, actor_alias: &str) -> bool {
        !self.provider_active(actor_alias)
            && (!self.serialize_provider_turns || !self.any_provider_active())
    }
}

pub(super) fn split_runtime_completion_boundaries(
    boundaries: Vec<BoundaryEvent>,
) -> (Vec<BoundaryEvent>, RuntimeCompletionQueue) {
    let mut initial = Vec::new();
    let mut completions = Vec::new();
    for boundary in boundaries {
        if is_scheduler_owned_runtime_completion(boundary.kind) {
            completions.push(boundary);
        } else {
            initial.push(boundary);
        }
    }
    (initial, RuntimeCompletionQueue::new(completions))
}

pub(crate) fn is_scheduler_owned_runtime_completion(kind: BoundaryKind) -> bool {
    runtime_completion_family(kind).is_some()
}

pub(super) async fn register_ready_runtime_completions(
    queue: &mut RuntimeCompletionQueue,
    state: &mut RuntimeCompletionState,
    scheduler: &mut BoundaryScheduler,
    registered_after: &crate::scheduler::DeliveredBoundary,
    world: &mut GeneratedRuntimeWorld,
    store: &ModelStore,
) -> Result<Vec<Value>, FixedScriptRunnerError> {
    let mut admissions = Vec::new();
    let ready = queue.take_ready(|event| runtime_completion_ready(event, state));
    for event in ready {
        if !runtime_completion_ready(&event, state) {
            queue.defer(event);
            continue;
        }
        let Some(family) = runtime_completion_family(event.kind) else {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "queued runtime completion `{}` has no completion family for {:?}",
                event.boundary_id, event.kind
            )));
        };
        let units = runtime_completion_units(&event)?;
        if event.kind == BoundaryKind::Provider {
            let turn_event = event.clone();
            let actor_alias = event.actor_alias.clone();
            let provider_boundary = event.boundary_id.clone();
            let (_pending, completion_event) =
                queue.register_pending_event(event, registered_after, family, units);
            world
                .start_provider_turn(
                    turn_event,
                    completion_event,
                    scheduler,
                    &store.queued_next_turn_boundaries(&actor_alias),
                )
                .await?;
            state.provider_started(&actor_alias);
            admissions
                .push(json!({"session": actor_alias, "provider_boundary": provider_boundary}));
        } else {
            queue.register(scheduler, event, registered_after, family, units);
        }
    }
    Ok(admissions)
}

pub(super) fn runtime_completion_ready(
    event: &BoundaryEvent,
    state: &RuntimeCompletionState,
) -> bool {
    match event.kind {
        BoundaryKind::Provider => {
            state.next_provider_turn_ready(event)
                // Serial lane only: admit a provider turn only when none is
                // live anywhere, so live turns never overlap. The generated
                // SEARCH lane leaves this off.
                && (!state.serialize_provider_turns || !state.any_provider_active())
        }
        BoundaryKind::Observer => {
            if !state.session_opened(&event.actor_alias) {
                return false;
            }
            let expected_turn_index = event
                .payload
                .get("turn_index")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            state.provider_completed_count(&event.actor_alias) >= expected_turn_index
        }
        BoundaryKind::BackendFailure | BoundaryKind::ProviderMutation => {
            state.session_opened(&event.actor_alias) && !state.provider_active(&event.actor_alias)
        }
        BoundaryKind::Cancellation => event
            .payload
            .get("target")
            .and_then(Value::as_str)
            .is_some_and(|target| state.queued_boundary_exists(target)),
        BoundaryKind::Tool | BoundaryKind::ExecCode => {
            state.provider_completed(&event.actor_alias)
                && state.handler_boundary_ready(&event.actor_alias)
        }
        BoundaryKind::DurableEffect => {
            state.session_opened(&event.actor_alias)
                && state.handler_boundary_ready(&event.actor_alias)
        }
        BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::Trigger => false,
    }
}

pub(super) fn runtime_completion_family(kind: BoundaryKind) -> Option<RuntimeCompletionFamily> {
    Some(match kind {
        BoundaryKind::Provider => RuntimeCompletionFamily::ProviderTurnCompletion,
        BoundaryKind::Cancellation => RuntimeCompletionFamily::QueuedInputCancellation,
        BoundaryKind::BackendFailure => RuntimeCompletionFamily::BackendRetryOrFailure,
        BoundaryKind::ProviderMutation => RuntimeCompletionFamily::ProviderScriptMutation,
        BoundaryKind::Tool => RuntimeCompletionFamily::ToolReturn,
        BoundaryKind::ExecCode => RuntimeCompletionFamily::ExecResult,
        BoundaryKind::DurableEffect => RuntimeCompletionFamily::DurableEffectCompletion,
        BoundaryKind::Observer => RuntimeCompletionFamily::ObserverSnapshot,
        BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::Trigger => return None,
    })
}

pub(super) fn runtime_completion_units(
    event: &BoundaryEvent,
) -> Result<Vec<RuntimeCompletionUnit>, FixedScriptRunnerError> {
    if event.kind == BoundaryKind::Provider {
        let provider_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "provider runtime completion `{}` missing provider_kind",
                    event.boundary_id
                ))
            })?;
        let turn = scripted_turn_from_provider_boundary(&event.payload).map_err(|err| {
            FixedScriptRunnerError::Assertion(format!(
                "provider runtime completion `{}` {err}",
                event.boundary_id
            ))
        })?;
        let script = runtime_script_for_turn(provider_kind, &turn)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        return Ok(script
            .timeline()
            .iter()
            .enumerate()
            .map(|(index, wire_event)| {
                RuntimeCompletionUnit::new(
                    format!("provider:{}:{index:02}", wire_event.event_name()),
                    wire_event.at(),
                )
            })
            .collect());
    }

    let unit = match event.kind {
        BoundaryKind::Cancellation => "runtime:cancel_pending_turn_input",
        BoundaryKind::BackendFailure => {
            if event
                .payload
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "runtime:backend_retry_attempt"
            } else {
                "runtime:backend_terminal_failure"
            }
        }
        BoundaryKind::ProviderMutation => "provider:mutated_script_parser_rejection",
        BoundaryKind::Tool => "runtime:tool_attempt_return",
        BoundaryKind::ExecCode => "runtime:exec_code_result",
        BoundaryKind::DurableEffect => "runtime:durable_effect_crash_redrive",
        BoundaryKind::Observer => "runtime:observer_snapshot",
        BoundaryKind::Provider
        | BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::Trigger => "runtime:completion",
    };
    Ok(vec![RuntimeCompletionUnit::new(unit, event.at)])
}
