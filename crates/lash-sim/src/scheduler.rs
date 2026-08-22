use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BOUNDARY_EVENT_SCHEMA: &str = "lash.sim.boundary-event.v1";
pub const PENDING_RUNTIME_BOUNDARY_SCHEMA: &str = "lash.sim.pending-runtime-boundary.v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryKind {
    Ingress,
    QueuedIngress,
    Provider,
    ProviderEvent,
    Tool,
    ExecCode,
    DurableEffect,
    ProcessWake,
    ProcessLifecycle,
    Worker,
    Observer,
    Cancellation,
    Trigger,
    BackendFailure,
    ProviderMutation,
    LeaseTime,
}

impl BoundaryKind {
    pub const fn name(&self) -> &'static str {
        match self {
            BoundaryKind::Ingress => "ingress",
            BoundaryKind::QueuedIngress => "queued_ingress",
            BoundaryKind::Provider => "provider",
            BoundaryKind::ProviderEvent => "provider_event",
            BoundaryKind::Tool => "tool",
            BoundaryKind::ExecCode => "exec_code",
            BoundaryKind::DurableEffect => "durable_effect",
            BoundaryKind::ProcessWake => "process_wake",
            BoundaryKind::ProcessLifecycle => "process_lifecycle",
            BoundaryKind::Worker => "worker",
            BoundaryKind::Observer => "observer",
            BoundaryKind::Cancellation => "cancellation",
            BoundaryKind::Trigger => "trigger",
            BoundaryKind::BackendFailure => "backend_failure",
            BoundaryKind::ProviderMutation => "provider_mutation",
            BoundaryKind::LeaseTime => "lease_time",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BoundaryEvent {
    pub boundary_id: String,
    pub actor_alias: String,
    pub kind: BoundaryKind,
    pub at: u64,
    pub label: String,
    #[serde(default)]
    pub payload: Value,
}

impl BoundaryEvent {
    pub fn new(
        boundary_id: impl Into<String>,
        actor_alias: impl Into<String>,
        kind: BoundaryKind,
        at: u64,
        label: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            boundary_id: boundary_id.into(),
            actor_alias: actor_alias.into(),
            kind,
            at,
            label: label.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeliveredBoundary {
    pub schema: String,
    pub sequence: usize,
    #[serde(default)]
    pub scheduler: SchedulerDeliveryEvidence,
    pub boundary_id: String,
    pub actor_alias: String,
    pub kind: BoundaryKind,
    pub at: u64,
    pub label: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub observed: Value,
}

impl DeliveredBoundary {
    fn from_event(
        sequence: usize,
        event: BoundaryEvent,
        observed: Value,
        scheduler: SchedulerDeliveryEvidence,
    ) -> Self {
        Self {
            schema: BOUNDARY_EVENT_SCHEMA.to_string(),
            sequence,
            scheduler,
            boundary_id: event.boundary_id,
            actor_alias: event.actor_alias,
            kind: event.kind,
            at: event.at,
            label: event.label,
            payload: event.payload,
            observed,
        }
    }

    pub fn as_event(&self) -> BoundaryEvent {
        BoundaryEvent {
            boundary_id: self.boundary_id.clone(),
            actor_alias: self.actor_alias.clone(),
            kind: self.kind,
            at: self.at,
            label: self.label.clone(),
            payload: self.payload.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SchedulerDeliveryEvidence {
    pub scheduler_controlled: bool,
    pub pending_before: usize,
    pub min_scheduled_at: u64,
    pub delivered_at: u64,
    pub candidate_count_at_tick: usize,
    pub selected_candidate_index: usize,
    pub seed_before: u64,
    pub seed_after: u64,
}

#[derive(Clone, Debug)]
pub struct BoundaryScheduler {
    pending: Vec<BoundaryEvent>,
    seed: u64,
    sequence: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeCompletionUnit {
    pub unit: String,
    pub at: u64,
}

impl RuntimeCompletionUnit {
    pub fn new(unit: impl Into<String>, at: u64) -> Self {
        Self {
            unit: unit.into(),
            at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingRuntimeBoundary {
    pub schema: String,
    pub pending_id: String,
    pub boundary_id: String,
    pub actor_alias: String,
    pub kind: BoundaryKind,
    pub completion_family: String,
    pub original_scheduled_at: u64,
    pub ready_at: u64,
    pub registered_after: String,
    pub registered_after_sequence: usize,
    pub completion_units: Vec<RuntimeCompletionUnit>,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeCompletionQueue {
    pending: Vec<BoundaryEvent>,
    registered_ids: BTreeSet<String>,
    completed_ids: BTreeSet<String>,
    registrations: Vec<PendingRuntimeBoundary>,
}

impl RuntimeCompletionQueue {
    pub fn new(events: impl IntoIterator<Item = BoundaryEvent>) -> Self {
        Self {
            pending: events.into_iter().collect(),
            registered_ids: BTreeSet::new(),
            completed_ids: BTreeSet::new(),
            registrations: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn registered_len(&self) -> usize {
        self.registered_ids.len()
    }

    pub fn completed_len(&self) -> usize {
        self.completed_ids.len()
    }

    pub fn registrations(&self) -> &[PendingRuntimeBoundary] {
        &self.registrations
    }

    pub fn pending_ids(&self) -> Vec<String> {
        self.pending
            .iter()
            .map(|event| event.boundary_id.clone())
            .collect()
    }

    pub fn mark_completed(&mut self, boundary_id: &str) {
        self.completed_ids.insert(boundary_id.to_string());
    }

    pub fn defer(&mut self, event: BoundaryEvent) {
        self.pending.push(event);
    }

    pub fn take_ready(&mut self, ready: impl Fn(&BoundaryEvent) -> bool) -> Vec<BoundaryEvent> {
        let mut selected = Vec::new();
        let mut remaining = Vec::with_capacity(self.pending.len());
        for event in self.pending.drain(..) {
            if ready(&event) {
                selected.push(event);
            } else {
                remaining.push(event);
            }
        }
        self.pending = remaining;
        selected
    }

    pub fn register(
        &mut self,
        scheduler: &mut BoundaryScheduler,
        event: BoundaryEvent,
        registered_after: &DeliveredBoundary,
        completion_family: impl Into<String>,
        completion_units: Vec<RuntimeCompletionUnit>,
    ) -> PendingRuntimeBoundary {
        let (pending, event) = self.register_pending_event(
            event,
            registered_after,
            completion_family,
            completion_units,
        );
        scheduler.schedule(event);
        pending
    }

    pub fn register_pending_event(
        &mut self,
        mut event: BoundaryEvent,
        registered_after: &DeliveredBoundary,
        completion_family: impl Into<String>,
        completion_units: Vec<RuntimeCompletionUnit>,
    ) -> (PendingRuntimeBoundary, BoundaryEvent) {
        let original_scheduled_at = event.at;
        let ready_at = original_scheduled_at.max(registered_after.at.saturating_add(1));
        event.at = ready_at;
        let pending = PendingRuntimeBoundary {
            schema: PENDING_RUNTIME_BOUNDARY_SCHEMA.to_string(),
            pending_id: format!("pending:{}", event.boundary_id),
            boundary_id: event.boundary_id.clone(),
            actor_alias: event.actor_alias.clone(),
            kind: event.kind,
            completion_family: completion_family.into(),
            original_scheduled_at,
            ready_at,
            registered_after: registered_after.boundary_id.clone(),
            registered_after_sequence: registered_after.sequence,
            completion_units,
        };
        let mut payload_object = event.payload.as_object().cloned().unwrap_or_default();
        payload_object.insert(
            "runtime_completion".to_string(),
            serde_json::to_value(&pending)
                .expect("pending runtime boundary evidence is serializable"),
        );
        event.payload = Value::Object(payload_object);
        self.registered_ids.insert(event.boundary_id.clone());
        self.registrations.push(pending.clone());
        (pending, event)
    }
}

impl BoundaryScheduler {
    pub fn new(seed: u64) -> Self {
        Self {
            pending: Vec::new(),
            seed,
            sequence: 0,
        }
    }

    pub fn with_events(seed: u64, events: impl IntoIterator<Item = BoundaryEvent>) -> Self {
        let mut scheduler = Self::new(seed);
        for event in events {
            scheduler.schedule(event);
        }
        scheduler
    }

    pub fn schedule(&mut self, event: BoundaryEvent) {
        self.pending.push(event);
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The scheduled time of the boundary `deliver_next` would select next (the
    /// minimum `at` across all pending boundaries). `None` when empty. Used by the
    /// serialized cross-backend driver as a delivery barrier: it must not deliver
    /// any boundary at or past a live turn's completion time until that
    /// completion has actually been scheduled, so the delivery order cannot drift
    /// with the backend store's async timing.
    pub fn min_pending_at(&self) -> Option<u64> {
        self.pending.iter().map(|event| event.at).min()
    }

    pub fn deliver_next(&mut self, observed: Value) -> Option<DeliveredBoundary> {
        let decision = self.next_decision()?;
        Some(self.deliver_index(decision, observed))
    }

    pub fn deliver_next_with(
        &mut self,
        observe: impl FnOnce(&BoundaryEvent) -> Value,
    ) -> Option<DeliveredBoundary> {
        let decision = self.next_decision()?;
        let observed = observe(&self.pending[decision.index]);
        Some(self.deliver_index(decision, observed))
    }

    pub fn deliver_boundary(
        &mut self,
        boundary_id: &str,
        observed: Value,
    ) -> Option<DeliveredBoundary> {
        let index = self
            .pending
            .iter()
            .position(|event| event.boundary_id == boundary_id)?;
        Some(self.deliver_direct(index, observed))
    }

    fn deliver_index(&mut self, decision: SchedulerDecision, observed: Value) -> DeliveredBoundary {
        let event = self.pending.remove(decision.index);
        let sequence = self.sequence;
        self.sequence += 1;
        let scheduler = SchedulerDeliveryEvidence {
            scheduler_controlled: true,
            pending_before: decision.pending_before,
            min_scheduled_at: decision.min_scheduled_at,
            delivered_at: event.at,
            candidate_count_at_tick: decision.candidate_count_at_tick,
            selected_candidate_index: decision.selected_candidate_index,
            seed_before: decision.seed_before,
            seed_after: decision.seed_after,
        };
        DeliveredBoundary::from_event(sequence, event, observed, scheduler)
    }

    fn deliver_direct(&mut self, index: usize, observed: Value) -> DeliveredBoundary {
        let pending_before = self.pending.len();
        let event = self.pending.remove(index);
        let sequence = self.sequence;
        self.sequence += 1;
        let scheduler = SchedulerDeliveryEvidence {
            scheduler_controlled: false,
            pending_before,
            min_scheduled_at: event.at,
            delivered_at: event.at,
            candidate_count_at_tick: 1,
            selected_candidate_index: 0,
            seed_before: self.seed,
            seed_after: self.seed,
        };
        DeliveredBoundary::from_event(sequence, event, observed, scheduler)
    }

    fn next_decision(&mut self) -> Option<SchedulerDecision> {
        let pending_before = self.pending.len();
        let seed_before = self.seed;
        let min_at = self.pending.iter().map(|event| event.at).min()?;
        let mut candidates = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, event)| event.at == min_at)
            .collect::<Vec<_>>();
        candidates.sort_by(|(_, left), (_, right)| {
            left.actor_alias
                .cmp(&right.actor_alias)
                .then_with(|| left.kind.name().cmp(right.kind.name()))
                .then_with(|| left.boundary_id.cmp(&right.boundary_id))
        });
        let candidate_count_at_tick = candidates.len();
        let selected_candidate_index = if candidate_count_at_tick == 1 {
            0
        } else {
            (next_seed(&mut self.seed) as usize) % candidate_count_at_tick
        };
        let index = candidates[selected_candidate_index].0;
        Some(SchedulerDecision {
            index,
            pending_before,
            min_scheduled_at: min_at,
            candidate_count_at_tick,
            selected_candidate_index,
            seed_before,
            seed_after: self.seed,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct SchedulerDecision {
    index: usize,
    pending_before: usize,
    min_scheduled_at: u64,
    candidate_count_at_tick: usize,
    selected_candidate_index: usize,
    seed_before: u64,
    seed_after: u64,
}

pub(crate) fn next_seed(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *seed
}

#[derive(Clone, Debug, Default)]
pub struct BoundaryDeliveryLog {
    events: VecDeque<DeliveredBoundary>,
}

impl BoundaryDeliveryLog {
    pub fn push(&mut self, event: DeliveredBoundary) {
        self.events.push_back(event);
    }

    pub fn into_vec(self) -> Vec<DeliveredBoundary> {
        self.events.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scheduler_delivers_seeded_boundaries_without_polling_futures() {
        let events = [
            BoundaryEvent::new(
                "ingress-a",
                "session-a",
                BoundaryKind::Ingress,
                9,
                "session.open",
                json!({}),
            ),
            BoundaryEvent::new(
                "provider-a",
                "session-a",
                BoundaryKind::Provider,
                10,
                "p",
                json!({}),
            ),
            BoundaryEvent::new(
                "tool-a",
                "session-a",
                BoundaryKind::Tool,
                10,
                "t",
                json!({}),
            ),
            BoundaryEvent::new(
                "lease-time-b",
                "session-b",
                BoundaryKind::LeaseTime,
                11,
                "lease.clock",
                json!({}),
            ),
        ];
        let mut first = BoundaryScheduler::with_events(7, events.clone());
        let mut second = BoundaryScheduler::with_events(7, events);

        let first_ids = drain_ids(&mut first);
        let second_ids = drain_ids(&mut second);

        assert_eq!(first_ids, second_ids);
        assert_eq!(first_ids.len(), 4);
        assert_eq!(first_ids.first().map(String::as_str), Some("ingress-a"));
        assert_eq!(first_ids.last().map(String::as_str), Some("lease-time-b"));
    }

    #[test]
    fn scheduler_records_delivery_decisions_as_trace_evidence() {
        let events = [
            BoundaryEvent::new("a", "session-a", BoundaryKind::Provider, 1, "p", json!({})),
            BoundaryEvent::new("b", "session-b", BoundaryKind::Provider, 1, "p", json!({})),
        ];
        let mut scheduler = BoundaryScheduler::with_events(11, events);

        let delivered = scheduler
            .deliver_next(json!({"ok": true}))
            .expect("delivered");

        assert!(delivered.scheduler.scheduler_controlled);
        assert_eq!(delivered.scheduler.pending_before, 2);
        assert_eq!(delivered.scheduler.min_scheduled_at, 1);
        assert_eq!(delivered.scheduler.delivered_at, 1);
        assert_eq!(delivered.scheduler.candidate_count_at_tick, 2);
    }

    #[test]
    fn runtime_completion_queue_registers_pending_boundary_evidence() {
        let mut scheduler = BoundaryScheduler::new(17);
        let registered_after = DeliveredBoundary {
            schema: BOUNDARY_EVENT_SCHEMA.to_string(),
            sequence: 3,
            scheduler: SchedulerDeliveryEvidence {
                scheduler_controlled: true,
                delivered_at: 10,
                ..SchedulerDeliveryEvidence::default()
            },
            boundary_id: "session-001:queued-ingress:001".to_string(),
            actor_alias: "session-001".to_string(),
            kind: BoundaryKind::QueuedIngress,
            at: 10,
            label: "queued-ingress.next-turn".to_string(),
            payload: json!({}),
            observed: json!({}),
        };
        let event = BoundaryEvent::new(
            "session-001:cancellation:001",
            "session-001",
            BoundaryKind::Cancellation,
            2,
            "queued-ingress.cancel",
            json!({"target": "session-001:queued-ingress:001"}),
        );
        let mut queue = RuntimeCompletionQueue::new([event]);
        assert_eq!(queue.registered_len(), 0);
        let ready = queue.take_ready(|event| event.kind == BoundaryKind::Cancellation);
        assert_eq!(ready.len(), 1);

        let pending = queue.register(
            &mut scheduler,
            ready.into_iter().next().expect("ready event"),
            &registered_after,
            "queued_input_cancellation",
            vec![RuntimeCompletionUnit::new(
                "runtime:cancel_pending_turn_input",
                2,
            )],
        );

        assert_eq!(pending.schema, PENDING_RUNTIME_BOUNDARY_SCHEMA);
        assert_eq!(pending.pending_id, "pending:session-001:cancellation:001");
        assert_eq!(pending.original_scheduled_at, 2);
        assert_eq!(pending.ready_at, 11);
        assert_eq!(pending.registered_after, "session-001:queued-ingress:001");
        assert_eq!(pending.registered_after_sequence, 3);
        assert_eq!(pending.completion_units.len(), 1);
        assert_eq!(queue.registered_len(), 1);
        assert_eq!(queue.registrations(), &[pending]);
        let delivered = scheduler
            .deliver_next(json!({"cancelled": true}))
            .expect("registered completion");
        assert_eq!(delivered.at, 11);
        assert!(delivered.payload.get("runtime_completion").is_some());
        assert_eq!(
            delivered
                .payload
                .pointer("/runtime_completion/completion_family")
                .and_then(Value::as_str),
            Some("queued_input_cancellation")
        );
        assert_eq!(
            delivered
                .payload
                .pointer("/runtime_completion/ready_at")
                .and_then(Value::as_u64),
            Some(11)
        );
    }

    #[test]
    fn runtime_completion_queue_registered_len_tracks_multiple_registrations() {
        let mut scheduler = BoundaryScheduler::new(23);
        let registered_after = DeliveredBoundary {
            schema: BOUNDARY_EVENT_SCHEMA.to_string(),
            sequence: 1,
            scheduler: SchedulerDeliveryEvidence {
                scheduler_controlled: true,
                delivered_at: 5,
                ..SchedulerDeliveryEvidence::default()
            },
            boundary_id: "session-001:provider:001".to_string(),
            actor_alias: "session-001".to_string(),
            kind: BoundaryKind::Provider,
            at: 5,
            label: "provider.chat.stream".to_string(),
            payload: json!({}),
            observed: json!({}),
        };
        let mut queue = RuntimeCompletionQueue::new([
            BoundaryEvent::new(
                "session-001:tool:001",
                "session-001",
                BoundaryKind::Tool,
                6,
                "tool.return",
                json!({}),
            ),
            BoundaryEvent::new(
                "session-001:exec:001",
                "session-001",
                BoundaryKind::ExecCode,
                7,
                "exec.result",
                json!({}),
            ),
        ]);
        assert_eq!(queue.registered_len(), 0);
        let ready = queue.take_ready(|_| true);
        for event in ready {
            queue.register(
                &mut scheduler,
                event,
                &registered_after,
                "runtime_completion",
                vec![RuntimeCompletionUnit::new("runtime:unit", 6)],
            );
        }

        assert_eq!(queue.registered_len(), 2);
        assert_eq!(queue.registrations().len(), 2);
        assert_eq!(drain_ids(&mut scheduler).len(), 2);
    }

    #[test]
    fn runtime_completion_queue_keeps_unready_boundaries_pending() {
        let events = [
            BoundaryEvent::new(
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                4,
                "provider.chat.stream",
                json!({"turn_index": 1}),
            ),
            BoundaryEvent::new(
                "session-001:tool:001",
                "session-001",
                BoundaryKind::Tool,
                5,
                "tool.return",
                json!({}),
            ),
        ];
        let mut queue = RuntimeCompletionQueue::new(events);

        let ready = queue.take_ready(|event| event.kind == BoundaryKind::Provider);

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].boundary_id, "session-001:provider:001");
        assert_eq!(queue.pending_ids(), vec!["session-001:tool:001"]);
        assert_eq!(queue.pending_len(), 1);
    }

    #[test]
    fn runtime_completion_queue_tracks_completed_boundary_ids_idempotently() {
        let mut queue = RuntimeCompletionQueue::new([
            BoundaryEvent::new(
                "session-001:provider:001",
                "session-001",
                BoundaryKind::Provider,
                4,
                "provider.chat.stream",
                json!({"turn_index": 1}),
            ),
            BoundaryEvent::new(
                "session-001:tool:001",
                "session-001",
                BoundaryKind::Tool,
                5,
                "tool.return",
                json!({}),
            ),
        ]);

        assert_eq!(queue.completed_len(), 0);
        queue.mark_completed("session-001:provider:001");
        assert_eq!(queue.completed_len(), 1);
        queue.mark_completed("session-001:provider:001");
        assert_eq!(
            queue.completed_len(),
            1,
            "completed IDs should be idempotent, not double-counted"
        );
        queue.mark_completed("session-001:tool:001");
        assert_eq!(queue.completed_len(), 2);
    }

    fn drain_ids(scheduler: &mut BoundaryScheduler) -> Vec<String> {
        let mut ids = Vec::new();
        while let Some(event) = scheduler.deliver_next(json!({"ok": true})) {
            ids.push(event.boundary_id);
        }
        ids
    }
}
