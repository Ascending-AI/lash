use super::*;

pub const SCHEDULER_OWNED_RUNTIME_COMPLETION_KINDS: &[BoundaryKind] = &[
    BoundaryKind::Provider,
    BoundaryKind::Cancellation,
    BoundaryKind::BackendFailure,
    BoundaryKind::ProviderMutation,
    BoundaryKind::Tool,
    BoundaryKind::ExecCode,
    BoundaryKind::DurableEffect,
    BoundaryKind::Worker,
    BoundaryKind::ProcessWake,
    BoundaryKind::Observer,
];

#[derive(Default)]
pub(super) struct RuntimeCompletionState {
    pub(super) opened_sessions: BTreeSet<String>,
    pub(super) queued_boundaries: BTreeSet<String>,
    pub(super) provider_completions_by_session: BTreeMap<String, usize>,
    pub(super) active_provider_turns_by_session: BTreeMap<String, usize>,
    pub(super) durable_first_completions: BTreeSet<String>,
    /// When set, at most ONE live provider turn is admitted across ALL sessions:
    /// a provider turn becomes ready only when no provider turn is in flight
    /// anywhere. This is enabled ONLY for the cross-backend durable re-run
    /// (SQLite/Postgres), where preserved cross-session concurrency would let the
    /// async store's mid-op await points interleave differently from the
    /// synchronous in-memory reference and change a timing-sensitive
    /// `next_turn` `claim_and_run_pending` lease race — drifting the exchange
    /// count. Durable-state equivalence is well-posed under serial execution. The
    /// model-store generated SEARCH lane leaves this OFF and keeps full preserved
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
            BoundaryKind::DurableEffect => {
                if event
                    .observed
                    .get("replayed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return;
                }
                if let Some(durable_key) = durable_key(event) {
                    self.durable_first_completions.insert(durable_key);
                }
            }
            BoundaryKind::ProviderEvent
            | BoundaryKind::Tool
            | BoundaryKind::ExecCode
            | BoundaryKind::ProcessWake
            | BoundaryKind::ProcessLifecycle
            | BoundaryKind::Worker
            | BoundaryKind::Observer
            | BoundaryKind::Cancellation
            | BoundaryKind::Trigger
            | BoundaryKind::BackendFailure
            | BoundaryKind::ProviderMutation
            | BoundaryKind::LeaseTime => {}
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

    fn durable_completed(&self, durable_key: &str) -> bool {
        self.durable_first_completions.contains(durable_key)
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
    match kind {
        BoundaryKind::Provider
        | BoundaryKind::Cancellation
        | BoundaryKind::BackendFailure
        | BoundaryKind::ProviderMutation
        | BoundaryKind::Tool
        | BoundaryKind::ExecCode
        | BoundaryKind::DurableEffect
        | BoundaryKind::Worker
        | BoundaryKind::ProcessWake
        | BoundaryKind::Observer => true,
        BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::ProcessLifecycle
        | BoundaryKind::Trigger
        | BoundaryKind::LeaseTime => false,
    }
}

pub(super) async fn register_ready_runtime_completions(
    queue: &mut RuntimeCompletionQueue,
    state: &mut RuntimeCompletionState,
    scheduler: &mut BoundaryScheduler,
    registered_after: &crate::scheduler::DeliveredBoundary,
    world: &mut GeneratedRuntimeWorld,
) -> Result<(), FixedScriptRunnerError> {
    let ready = queue.take_ready(|event| runtime_completion_ready(event, state));
    for event in ready {
        if !runtime_completion_ready(&event, state) {
            queue.defer(event);
            continue;
        }
        let family = runtime_completion_family(&event);
        let units = runtime_completion_units(&event)?;
        if event.kind == BoundaryKind::Provider {
            let turn_event = event.clone();
            let actor_alias = event.actor_alias.clone();
            let (_pending, completion_event) =
                queue.register_pending_event(event, registered_after, family, units);
            world
                .start_provider_turn(turn_event, completion_event, scheduler)
                .await?;
            state.provider_started(&actor_alias);
        } else {
            queue.register(scheduler, event, registered_after, family, units);
        }
    }
    Ok(())
}

pub(super) fn runtime_completion_ready(
    event: &BoundaryEvent,
    state: &RuntimeCompletionState,
) -> bool {
    match event.kind {
        BoundaryKind::Provider => {
            state.next_provider_turn_ready(event)
                // Cross-backend durable re-run only: admit a provider turn only
                // when none is live anywhere, so live turns never overlap and the
                // backend's async-vs-sync store timing cannot change committed
                // outcomes. The generated SEARCH lane leaves this off.
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
        BoundaryKind::Worker => {
            let session = completion_session_alias(event);
            state.session_opened(&session) && !state.provider_active(&session)
        }
        BoundaryKind::Cancellation => event
            .payload
            .get("target")
            .and_then(Value::as_str)
            .is_some_and(|target| state.queued_boundary_exists(target)),
        BoundaryKind::Tool | BoundaryKind::ExecCode => {
            state.provider_completed(&event.actor_alias)
                && !state.provider_active(&event.actor_alias)
        }
        BoundaryKind::DurableEffect => {
            if state.provider_active(&event.actor_alias) {
                return false;
            }
            if event.label.contains("replay") {
                durable_key_from_event(event)
                    .is_some_and(|durable_key| state.durable_completed(&durable_key))
            } else {
                state.session_opened(&event.actor_alias)
            }
        }
        BoundaryKind::ProcessWake => {
            let session = completion_session_alias(event);
            state.session_opened(&session) && !state.provider_active(&session)
        }
        BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::ProcessLifecycle
        | BoundaryKind::Trigger
        | BoundaryKind::LeaseTime => false,
    }
}

fn completion_session_alias(event: &BoundaryEvent) -> String {
    event
        .payload
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or(&event.actor_alias)
        .to_string()
}

pub(super) fn runtime_completion_family(event: &BoundaryEvent) -> &'static str {
    match event.kind {
        BoundaryKind::Provider => "provider_turn_completion",
        BoundaryKind::Cancellation => "queued_input_cancellation",
        BoundaryKind::BackendFailure => "backend_retry_or_failure",
        BoundaryKind::ProviderMutation => "provider_script_mutation",
        BoundaryKind::Tool => "tool_return",
        BoundaryKind::ExecCode => "exec_result",
        BoundaryKind::DurableEffect => "durable_effect_completion",
        BoundaryKind::Worker => "worker_lease_completion",
        BoundaryKind::ProcessWake => "process_wake",
        BoundaryKind::Observer => "observer_snapshot",
        BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::ProcessLifecycle
        | BoundaryKind::Trigger
        | BoundaryKind::LeaseTime => "runtime_completion",
    }
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
        let text = event
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let script = runtime_script_for_text(provider_kind, text)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        return Ok(script
            .timeline
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
        BoundaryKind::DurableEffect => {
            if event.label.contains("replay") {
                "runtime:durable_effect_replay"
            } else {
                "runtime:durable_effect_local_completion"
            }
        }
        BoundaryKind::Worker => "runtime:worker_stale_completion",
        BoundaryKind::ProcessWake => "runtime:process_wake_delivery",
        BoundaryKind::Observer => "runtime:observer_snapshot",
        BoundaryKind::Provider
        | BoundaryKind::Ingress
        | BoundaryKind::QueuedIngress
        | BoundaryKind::ProviderEvent
        | BoundaryKind::ProcessLifecycle
        | BoundaryKind::Trigger
        | BoundaryKind::LeaseTime => "runtime:completion",
    };
    Ok(vec![RuntimeCompletionUnit::new(unit, event.at)])
}

fn durable_key(event: &crate::scheduler::DeliveredBoundary) -> Option<String> {
    event
        .observed
        .get("durable_key")
        .or_else(|| event.payload.get("durable_key"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn durable_key_from_event(event: &BoundaryEvent) -> Option<String> {
    event
        .payload
        .get("durable_key")
        .and_then(Value::as_str)
        .map(str::to_string)
}
