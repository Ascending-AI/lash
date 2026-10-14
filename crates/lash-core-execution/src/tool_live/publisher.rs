use super::*;
use std::{
    collections::VecDeque,
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub struct ToolLivePublisher {
    owner: Arc<Owner>,
    call: Arc<Call>,
}
struct Call {
    address: CallAddress,
    ended: AtomicBool,
}
struct Slot {
    call: Arc<Call>,
    progress: Option<(ToolLiveFact, usize)>,
    dirty: bool,
    logs: VecDeque<(ToolLiveFact, usize)>,
    omitted: [u64; 6],
    ended_at: Option<tokio::time::Instant>,
}
#[derive(Default)]
struct Pending {
    slots: Vec<Slot>,
    bytes: usize,
    events: usize,
    in_flight_bytes: usize,
    in_flight_events: usize,
    logs: usize,
    ordinal: u64,
}
pub(super) struct Owner {
    address: ExecutionOwner,
    epoch: String,
    transport: Option<Arc<dyn ToolLiveReplay>>,
    policy: ToolLivePolicy,
    pending: Mutex<Pending>,
}
const CAUSES: [ToolLiveOmission; 6] = [
    ToolLiveOmission::Coalesced,
    ToolLiveOmission::OwnerBudget,
    ToolLiveOmission::Oversized,
    ToolLiveOmission::AfterCompletion,
    ToolLiveOmission::StoreFailed,
    ToolLiveOmission::RoutesFull,
];
impl Owner {
    pub(super) fn new(
        address: ExecutionOwner,
        transport: Option<Arc<dyn ToolLiveReplay>>,
        policy: ToolLivePolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            address,
            epoch: uuid::Uuid::new_v4().to_string(),
            transport,
            policy,
            pending: Mutex::default(),
        })
    }
    pub(super) fn bind(self: &Arc<Self>, address: CallAddress) -> ToolLivePublisher {
        let mut pending = self.pending.lock_recover();
        if let Some(slot) = pending
            .slots
            .iter()
            .find(|slot| slot.call.address == address && !slot.call.ended.load(Ordering::Acquire))
        {
            return ToolLivePublisher {
                owner: self.clone(),
                call: slot.call.clone(),
            };
        }
        let call = Arc::new(Call {
            address,
            ended: AtomicBool::new(false),
        });
        let first = pending.slots.is_empty();
        if pending.slots.len() < self.policy.calls {
            pending.slots.push(Slot {
                call: call.clone(),
                progress: None,
                dirty: false,
                logs: VecDeque::new(),
                omitted: [0; 6],
                ended_at: None,
            });
        } else {
            // Refuse new observation routes, never evict an admitted live call.
            call.ended.store(true, Ordering::Release);
            if let Some(slot) = pending.slots.first_mut() {
                slot.omitted[5] = slot.omitted[5].saturating_add(1);
            }
        }
        drop(pending);
        if first && self.transport.is_some() {
            let owner = self.clone();
            crate::task::spawn(async move {
                owner.flush().await;
            });
        }
        ToolLivePublisher {
            owner: self.clone(),
            call,
        }
    }
    async fn flush(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.policy.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let batch = {
                let mut pending = self.pending.lock_recover();
                pending.logs = 0;
                let mut payloads = Vec::new();
                let mut batch_bytes = 0;
                let mut emitted_logs = 0;
                let mut freed_bytes = 0;
                let mut freed_events = 0;
                for slot in &mut pending.slots {
                    if payloads.len() >= self.policy.events / 2 {
                        break;
                    }
                    if slot.dirty
                        && let Some((fact, bytes)) = &slot.progress
                        && batch_bytes + bytes <= self.policy.bytes / 2
                    {
                        batch_bytes += bytes;
                        payloads.push((
                            slot.call.address.clone(),
                            ToolLivePayload::Fact { fact: fact.clone() },
                        ));
                        slot.dirty = false;
                    }
                    while payloads.len() < self.policy.events / 2
                        && emitted_logs < self.policy.logs
                        && slot
                            .logs
                            .front()
                            .is_some_and(|(_, bytes)| batch_bytes + bytes <= self.policy.bytes / 2)
                        && let Some((fact, bytes)) = slot.logs.pop_front()
                    {
                        emitted_logs += 1;
                        batch_bytes += bytes;
                        freed_bytes += bytes;
                        freed_events += 1;
                        payloads.push((slot.call.address.clone(), ToolLivePayload::Fact { fact }));
                    }
                    let omission_bytes = serde_json::to_vec(&slot.call.address)
                        .map_or(512, |bytes| bytes.len() + 512);
                    for (index, count) in slot.omitted.iter_mut().enumerate() {
                        if *count != 0
                            && payloads.len() < self.policy.events / 2
                            && batch_bytes + omission_bytes <= self.policy.bytes / 2
                        {
                            batch_bytes += omission_bytes;
                            payloads.push((
                                slot.call.address.clone(),
                                ToolLivePayload::Omitted {
                                    cause: CAUSES[index],
                                    count: std::mem::take(count),
                                },
                            ));
                        }
                    }
                }
                if pending.slots.len() > 1 {
                    pending.slots.rotate_left(1);
                }
                pending.bytes -= freed_bytes;
                pending.events -= freed_events;
                let batch = payloads
                    .into_iter()
                    .map(|(address, payload)| {
                        pending.ordinal += 1;
                        ToolLiveEmission {
                            address,
                            id: ToolLiveEmissionId {
                                producer_epoch: self.epoch.clone(),
                                ordinal: pending.ordinal,
                            },
                            payload,
                        }
                    })
                    .collect::<Vec<_>>();
                pending.in_flight_events = batch.len();
                pending.in_flight_bytes = batch
                    .iter()
                    .map(|event| serde_json::to_vec(event).map_or(0, |bytes| bytes.len()))
                    .sum();
                batch
            };
            if !batch.is_empty()
                && let Some(transport) = &self.transport
            {
                let addresses = batch
                    .iter()
                    .map(|emission| {
                        (
                            emission.address.clone(),
                            match &emission.payload {
                                ToolLivePayload::Omitted { count, .. } => *count,
                                ToolLivePayload::Fact { .. } => 1,
                            },
                            matches!(
                                emission.payload,
                                ToolLivePayload::Fact {
                                    fact: ToolLiveFact::Progress { .. }
                                }
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    transport.publish(&self.address, batch),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    let mut pending = self.pending.lock_recover();
                    for (address, count, progress) in &addresses {
                        if let Some(slot) = pending
                            .slots
                            .iter_mut()
                            .find(|slot| &slot.call.address == address)
                        {
                            slot.omitted[4] = slot.omitted[4].saturating_add(*count);
                            if *progress {
                                slot.dirty = true;
                            }
                        }
                    }
                }
            }
            let mut pending = self.pending.lock_recover();
            pending.in_flight_events = 0;
            pending.in_flight_bytes = 0;
            let now = tokio::time::Instant::now();
            let mut freed = 0;
            let mut events = 0;
            pending.slots.retain(|slot| {
                let retire = slot
                    .ended_at
                    .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(2))
                    && !slot.dirty
                    && slot.logs.is_empty()
                    && slot.omitted.iter().all(|count| *count == 0);
                if retire && let Some((_, bytes)) = &slot.progress {
                    freed += bytes;
                    events += 1;
                }
                !retire
            });
            pending.bytes -= freed;
            pending.events -= events;
            if pending.slots.is_empty() {
                break;
            }
        }
    }
}
impl ToolLivePublisher {
    pub fn address(&self) -> &CallAddress {
        &self.call.address
    }
    /// Latest-wins progress; bounded synchronous admission never awaits a
    /// store or an observer. Logs share the owner's interval budget.
    pub fn publish(&self, fact: ToolLiveFact) {
        let policy = self.owner.policy;
        let mut pending = self.owner.pending.lock_recover();
        let Some(index) = pending
            .slots
            .iter()
            .position(|slot| Arc::ptr_eq(&slot.call, &self.call))
        else {
            return;
        };
        if self.call.ended.load(Ordering::Acquire) {
            pending.slots[index].omitted[3] = pending.slots[index].omitted[3].saturating_add(1);
            return;
        }
        let Some(bytes) = fact_size(&fact, policy.fact_bytes) else {
            pending.slots[index].omitted[2] = pending.slots[index].omitted[2].saturating_add(1);
            return;
        };
        let bytes = bytes + serde_json::to_vec(&self.call.address).map_or(0, |bytes| bytes.len());
        let progress = matches!(fact, ToolLiveFact::Progress { .. });
        let replaced = if progress {
            pending.slots[index]
                .progress
                .as_ref()
                .map_or(0, |(_, bytes)| *bytes)
        } else {
            0
        };
        let extra = usize::from(!progress || replaced == 0);
        if pending.bytes - replaced + bytes > policy.bytes / 2
            || pending.events + extra > policy.events / 2
            || (!progress && pending.logs >= policy.logs)
        {
            pending.slots[index].omitted[1] = pending.slots[index].omitted[1].saturating_add(1);
            return;
        }
        pending.bytes = pending.bytes - replaced + bytes;
        pending.events += extra;
        if progress {
            let slot = &mut pending.slots[index];
            if slot.dirty {
                slot.omitted[0] = slot.omitted[0].saturating_add(1);
            }
            slot.progress = Some((fact, bytes));
            slot.dirty = true;
        } else {
            pending.logs += 1;
            pending.slots[index].logs.push_back((fact, bytes));
        }
    }
    pub fn omit(&self, cause: ToolLiveOmission) {
        let mut pending = self.owner.pending.lock_recover();
        if let Some(slot) = pending
            .slots
            .iter_mut()
            .find(|slot| Arc::ptr_eq(&slot.call, &self.call))
            && let Some(index) = CAUSES.iter().position(|candidate| *candidate == cause)
        {
            slot.omitted[index] = slot.omitted[index].saturating_add(1);
        }
    }
    /// Seal at the attempt's completion fence. The final admitted progress is
    /// flushed even when that interval already emitted the call's progress.
    pub fn finish(&self) {
        if self.call.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut pending = self.owner.pending.lock_recover();
        if let Some(slot) = pending
            .slots
            .iter_mut()
            .find(|slot| Arc::ptr_eq(&slot.call, &self.call))
        {
            slot.dirty = slot.progress.is_some();
            slot.ended_at = Some(tokio::time::Instant::now());
        }
    }
    pub(crate) fn fence(&self) -> CompletionFence {
        CompletionFence(self.clone())
    }
}
pub(crate) struct CompletionFence(ToolLivePublisher);
impl Drop for CompletionFence {
    fn drop(&mut self) {
        self.0.finish();
    }
}

// Counting serialization stops at the fact limit without allocating a copy
// of an unbounded log body.
fn fact_size(fact: &ToolLiveFact, limit: usize) -> Option<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if data.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other("tool-live fact bound"));
            }
            self.bytes += data.len();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, fact).ok()?;
    Some(counter.bytes + 512)
}

#[cfg(test)]
#[path = "publisher_laws.rs"]
mod laws;
