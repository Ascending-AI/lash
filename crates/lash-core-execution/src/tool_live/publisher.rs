use super::{
    policy::{TOOL_LIVE_COMPLETION_LINGER, TOOL_LIVE_ENVELOPE_BYTES, TOOL_LIVE_WRITE_TIMEOUT},
    *,
};
use std::{
    collections::VecDeque,
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::time::Instant;

#[derive(Clone)]
pub struct ToolLivePublisher {
    owner: Arc<Owner>,
    call: Arc<Call>,
}
struct Call {
    address: CallAddress,
    address_bytes: usize,
    /// Refused at admission: counted once, publishes nothing.
    refused: bool,
    ended: AtomicBool,
}
struct Slot {
    call: Arc<Call>,
    /// The latest progress and its charge, held by the call's reservation.
    progress: Option<(ToolLiveFact, usize)>,
    dirty: bool,
    logs: VecDeque<(ToolLiveFact, usize)>,
    omitted: [u64; CAUSES.len()],
    /// Sealed calls only: the fence or the slot's latest write.
    settled_at: Option<Instant>,
    in_flight: bool,
}
impl Slot {
    fn new(call: Arc<Call>) -> Self {
        Self {
            call,
            progress: None,
            dirty: false,
            logs: VecDeque::new(),
            omitted: [0; CAUSES.len()],
            settled_at: None,
            in_flight: false,
        }
    }
    /// Sealed with nothing left to write: its slot may be reused.
    fn drained(&self) -> bool {
        self.settled_at.is_some()
            && !self.dirty
            && !self.in_flight
            && self.logs.is_empty()
            && self.omitted.iter().all(|count| *count == 0)
    }
}
#[derive(Default)]
struct Pending {
    slots: Vec<Slot>,
    log_bytes: usize,
    log_count: usize,
    in_flight_records: usize,
    in_flight_bytes: usize,
    /// Owner-level `RoutesFull` counted and not yet written.
    refused: u64,
    refused_total: u64,
    failed_writes: u64,
    ordinal: u64,
    flushing: bool,
}
pub(super) struct Owner {
    address: ExecutionOwner,
    address_bytes: usize,
    epoch: String,
    transport: Option<Arc<dyn ToolLiveReplay>>,
    policy: ToolLivePolicy,
    pending: Mutex<Pending>,
}
const CAUSES: [ToolLiveOmission; 7] = [
    ToolLiveOmission::Coalesced,
    ToolLiveOmission::OwnerBudget,
    ToolLiveOmission::Oversized,
    ToolLiveOmission::AfterCompletion,
    ToolLiveOmission::StoreFailed,
    ToolLiveOmission::RoutesFull,
    ToolLiveOmission::NonFinite,
];
fn cause_index(cause: ToolLiveOmission) -> usize {
    CAUSES
        .iter()
        .position(|candidate| *candidate == cause)
        .unwrap_or_default()
}

enum Draft {
    Call(Arc<Call>, ToolLivePayload),
    Owner(u64),
}
/// What a failed write gives back to the publisher.
enum Restore {
    Progress(Arc<Call>),
    Log(Arc<Call>),
    Omitted(Arc<Call>, usize, u64),
    Refused(u64),
}

/// A point-in-time view of one owner's publisher on this node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLiveDiagnostics {
    pub policy: ToolLivePolicySnapshot,
    /// Admitted calls not yet past their completion fence.
    pub live_calls: usize,
    /// Sealed calls whose slot still lingers or still has work to write.
    pub sealed_calls: usize,
    /// `progress_bytes` held for every slot.
    pub reserved_bytes: usize,
    /// Progress values a write has not carried yet.
    pub pending_progress: usize,
    pub waiting_logs: usize,
    pub waiting_log_bytes: usize,
    pub in_flight_records: usize,
    pub in_flight_bytes: usize,
    /// Omissions counted and not yet written, call- and owner-scoped.
    pub pending_omissions: u64,
    /// Admissions and late publications that found no slot.
    pub refused_total: u64,
    /// Writes that failed or timed out. Their `StoreFailed` counts reach
    /// subscribers only with a later successful write.
    pub failed_writes: u64,
}

impl Owner {
    pub(super) fn new(
        address: ExecutionOwner,
        transport: Option<Arc<dyn ToolLiveReplay>>,
        policy: ToolLivePolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            address_bytes: serde_json::to_vec(&address).map_or(0, |bytes| bytes.len()),
            address,
            epoch: uuid::Uuid::new_v4().to_string(),
            transport,
            policy,
            pending: Mutex::default(),
        })
    }
    /// Admits a call with its final-gauge reservation, reusing a drained
    /// sealed slot when every slot is taken. A call that finds no slot is
    /// refused here, as an owner-level `RoutesFull`, and publishes nothing.
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
        let refused = !self.room(&mut pending);
        let call = Arc::new(Call {
            address_bytes: serde_json::to_vec(&address).map_or(0, |bytes| bytes.len()),
            address,
            refused,
            ended: AtomicBool::new(refused),
        });
        if !refused {
            pending.slots.push(Slot::new(call.clone()));
        }
        self.start(&mut pending);
        ToolLivePublisher {
            owner: self.clone(),
            call,
        }
    }
    /// Room for one more slot, or an owner-level refusal counted.
    fn room(&self, pending: &mut Pending) -> bool {
        if pending.slots.len() >= self.policy.calls {
            let oldest = pending
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.drained())
                .min_by_key(|(_, slot)| slot.settled_at)
                .map(|(index, _)| index);
            let Some(index) = oldest else {
                pending.refused = pending.refused.saturating_add(1);
                pending.refused_total = pending.refused_total.saturating_add(1);
                return false;
            };
            pending.slots.remove(index);
        }
        true
    }
    fn start(self: &Arc<Self>, pending: &mut Pending) {
        if pending.flushing || self.transport.is_none() {
            return;
        }
        pending.flushing = true;
        let owner = self.clone();
        crate::task::spawn(async move {
            owner.flush().await;
        });
    }
    /// Counts `cause` against the call. A sealed call whose slot already
    /// retired reopens an omission-only slot, so late publication on a
    /// retained handle is still attributed to it.
    fn count(self: &Arc<Self>, call: &Arc<Call>, cause: ToolLiveOmission) {
        if call.refused {
            return;
        }
        let mut pending = self.pending.lock_recover();
        if let Some(slot) = pending
            .slots
            .iter_mut()
            .find(|slot| Arc::ptr_eq(&slot.call, call))
        {
            let count = &mut slot.omitted[cause_index(cause)];
            *count = count.saturating_add(1);
            return;
        }
        if self.room(&mut pending) {
            let mut slot = Slot::new(call.clone());
            slot.omitted[cause_index(cause)] = 1;
            slot.settled_at = Some(Instant::now());
            pending.slots.push(slot);
        }
        self.start(&mut pending);
    }
    pub(super) fn diagnostics(&self) -> ToolLiveDiagnostics {
        let pending = self.pending.lock_recover();
        let sealed = pending
            .slots
            .iter()
            .filter(|slot| slot.call.ended.load(Ordering::Acquire))
            .count();
        ToolLiveDiagnostics {
            policy: self.policy.snapshot(),
            live_calls: pending.slots.len() - sealed,
            sealed_calls: sealed,
            reserved_bytes: pending.slots.len() * self.policy.progress_bytes,
            pending_progress: pending.slots.iter().filter(|slot| slot.dirty).count(),
            waiting_logs: pending.log_count,
            waiting_log_bytes: pending.log_bytes,
            in_flight_records: pending.in_flight_records,
            in_flight_bytes: pending.in_flight_bytes,
            pending_omissions: pending
                .slots
                .iter()
                .flat_map(|slot| slot.omitted.iter())
                .fold(pending.refused, |sum, count| sum.saturating_add(*count)),
            refused_total: pending.refused_total,
            failed_writes: pending.failed_writes,
        }
    }
    /// One write's records: every waiting final first, round robin across
    /// calls, then logs, then omission counts, within `events` records and
    /// the write budget. What does not fit waits for the next interval.
    fn batch(&self, pending: &mut Pending) -> (Vec<ToolLiveRecord>, Vec<Restore>) {
        let policy = self.policy;
        let budget = policy.write_bytes();
        let mut payloads: Vec<(Draft, Restore)> = Vec::new();
        let mut bytes = 0;
        let mut last = None;
        for (index, slot) in pending.slots.iter_mut().enumerate() {
            if payloads.len() >= policy.events {
                break;
            }
            if slot.dirty
                && let Some((fact, charge)) = &slot.progress
                && bytes + charge <= budget
            {
                bytes += charge;
                payloads.push((
                    Draft::Call(
                        slot.call.clone(),
                        ToolLivePayload::Fact { fact: fact.clone() },
                    ),
                    Restore::Progress(slot.call.clone()),
                ));
                slot.dirty = false;
                slot.in_flight = true;
                last = Some(index);
            }
        }
        let mut logs = 0;
        let (mut log_bytes, mut log_count) = (0, 0);
        for slot in &mut pending.slots {
            while payloads.len() < policy.events
                && logs < policy.logs
                && slot
                    .logs
                    .front()
                    .is_some_and(|(_, charge)| bytes + charge <= budget)
                && let Some((fact, charge)) = slot.logs.pop_front()
            {
                logs += 1;
                bytes += charge;
                log_bytes += charge;
                log_count += 1;
                slot.in_flight = true;
                payloads.push((
                    Draft::Call(slot.call.clone(), ToolLivePayload::Fact { fact }),
                    Restore::Log(slot.call.clone()),
                ));
            }
        }
        pending.log_bytes -= log_bytes;
        pending.log_count -= log_count;
        for slot in &mut pending.slots {
            let charge = slot.call.address_bytes + TOOL_LIVE_ENVELOPE_BYTES;
            for (index, count) in slot.omitted.iter_mut().enumerate() {
                if *count != 0 && payloads.len() < policy.events && bytes + charge <= budget {
                    bytes += charge;
                    let count = std::mem::take(count);
                    slot.in_flight = true;
                    payloads.push((
                        Draft::Call(
                            slot.call.clone(),
                            ToolLivePayload::Omitted {
                                cause: CAUSES[index],
                                count,
                            },
                        ),
                        Restore::Omitted(slot.call.clone(), index, count),
                    ));
                }
            }
        }
        let charge = self.address_bytes + TOOL_LIVE_ENVELOPE_BYTES;
        if pending.refused != 0 && payloads.len() < policy.events && bytes + charge <= budget {
            bytes += charge;
            let count = std::mem::take(&mut pending.refused);
            payloads.push((Draft::Owner(count), Restore::Refused(count)));
        }
        if let Some(last) = last {
            let len = pending.slots.len();
            pending.slots.rotate_left((last + 1) % len);
        }
        pending.in_flight_records = payloads.len();
        pending.in_flight_bytes = bytes;
        let mut records = Vec::with_capacity(payloads.len());
        let mut restores = Vec::with_capacity(payloads.len());
        for (draft, restore) in payloads {
            pending.ordinal += 1;
            let id = ToolLiveEmissionId {
                producer_epoch: self.epoch.clone(),
                ordinal: pending.ordinal,
            };
            records.push(match draft {
                Draft::Call(call, payload) => ToolLiveRecord::Call(ToolLiveEmission {
                    address: call.address.clone(),
                    id,
                    payload,
                }),
                Draft::Owner(count) => ToolLiveRecord::Owner(ToolLiveOwnerOmission {
                    owner: self.address.clone(),
                    id,
                    cause: ToolLiveOmission::RoutesFull,
                    count,
                }),
            });
            restores.push(restore);
        }
        (records, restores)
    }
    /// A failed write loses its logs (counted `StoreFailed`); progress is
    /// rewritten next interval and omission counts are counted again.
    fn restore(pending: &mut Pending, restores: Vec<Restore>) {
        pending.failed_writes = pending.failed_writes.saturating_add(1);
        for restore in restores {
            let (call, index, count) = match restore {
                Restore::Refused(count) => {
                    pending.refused = pending.refused.saturating_add(count);
                    continue;
                }
                Restore::Progress(call) => {
                    if let Some(slot) = pending
                        .slots
                        .iter_mut()
                        .find(|slot| Arc::ptr_eq(&slot.call, &call))
                    {
                        slot.dirty = slot.progress.is_some();
                    }
                    continue;
                }
                Restore::Log(call) => (call, cause_index(ToolLiveOmission::StoreFailed), 1),
                Restore::Omitted(call, index, count) => (call, index, count),
            };
            if let Some(slot) = pending
                .slots
                .iter_mut()
                .find(|slot| Arc::ptr_eq(&slot.call, &call))
            {
                slot.omitted[index] = slot.omitted[index].saturating_add(count);
            }
        }
    }
    async fn flush(self: Arc<Self>) {
        let Some(transport) = self.transport.clone() else {
            return;
        };
        let mut tick = tokio::time::interval(self.policy.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let (records, restores) = self.batch(&mut self.pending.lock_recover());
            if !records.is_empty() {
                let result = tokio::time::timeout(
                    TOOL_LIVE_WRITE_TIMEOUT,
                    transport.publish(&self.address, records),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    Self::restore(&mut self.pending.lock_recover(), restores);
                }
            }
            let mut pending = self.pending.lock_recover();
            pending.in_flight_records = 0;
            pending.in_flight_bytes = 0;
            let now = Instant::now();
            for slot in &mut pending.slots {
                if std::mem::take(&mut slot.in_flight) && slot.settled_at.is_some() {
                    slot.settled_at = Some(now);
                }
            }
            pending.slots.retain(|slot| {
                !(slot.drained()
                    && slot
                        .settled_at
                        .is_some_and(|at| now.duration_since(at) >= TOOL_LIVE_COMPLETION_LINGER))
            });
            if pending.slots.is_empty() && pending.refused == 0 {
                pending.flushing = false;
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
    /// store or an observer. Progress is held by the call's reservation, so
    /// a valid gauge is never refused for space; logs share the owner's log
    /// budget. Invalid facts are counted, never stored.
    pub fn publish(&self, fact: ToolLiveFact) {
        let policy = self.owner.policy;
        if self.call.refused {
            return;
        }
        if self.call.ended.load(Ordering::Acquire) {
            self.owner
                .count(&self.call, ToolLiveOmission::AfterCompletion);
            return;
        }
        let cause = match &fact {
            ToolLiveFact::Progress {
                progress, total, ..
            } if !progress.is_finite() || total.is_some_and(|total| !total.is_finite()) => {
                Some(ToolLiveOmission::NonFinite)
            }
            _ => None,
        };
        let charge = fact_size(&fact, policy.fact_bytes)
            .map(|bytes| bytes + self.call.address_bytes + TOOL_LIVE_ENVELOPE_BYTES);
        let progress = matches!(fact, ToolLiveFact::Progress { .. });
        let cause = cause.or(match charge {
            Some(charge) if !progress || charge <= policy.progress_bytes => None,
            _ => Some(ToolLiveOmission::Oversized),
        });
        let mut pending = self.owner.pending.lock_recover();
        let Some(index) = pending
            .slots
            .iter()
            .position(|slot| Arc::ptr_eq(&slot.call, &self.call))
        else {
            return;
        };
        let charge = charge.unwrap_or_default();
        let cause = cause.or_else(|| {
            (!progress
                && (pending.log_count >= policy.logs
                    || pending.log_bytes + charge > policy.log_bytes()))
            .then_some(ToolLiveOmission::OwnerBudget)
        });
        let slot = &mut pending.slots[index];
        if let Some(cause) = cause {
            let count = &mut slot.omitted[cause_index(cause)];
            *count = count.saturating_add(1);
            return;
        }
        if progress {
            if slot.dirty {
                let count = &mut slot.omitted[cause_index(ToolLiveOmission::Coalesced)];
                *count = count.saturating_add(1);
            }
            slot.progress = Some((fact, charge));
            slot.dirty = true;
        } else {
            slot.logs.push_back((fact, charge));
            pending.log_count += 1;
            pending.log_bytes += charge;
        }
    }
    pub fn omit(&self, cause: ToolLiveOmission) {
        self.owner.count(&self.call, cause);
    }
    /// This call's owner publisher on this node.
    pub fn diagnostics(&self) -> ToolLiveDiagnostics {
        self.owner.diagnostics()
    }
    /// Seal at the attempt's completion fence. The final progress is written
    /// again even when an earlier interval already carried it.
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
            slot.settled_at = Some(Instant::now());
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
    Some(counter.bytes)
}

#[cfg(test)]
#[path = "publisher_laws.rs"]
mod laws;
