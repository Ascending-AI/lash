//! The fake pool: fake workers checked out of a bounded number of slots.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_vm_protocol::{ExecutionLease, FrameCodec, FrameEpoch, OwnerEpoch, VmOwner};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::worker::{FakeWorker, Fault};
use crate::transport::{CheckoutRefusal, WorkerCheckout, WorkerSlots};

/// What a fake pool did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub checkouts: usize,
    pub releases: usize,
    pub discards: usize,
    /// Runs a worker started: a `Start` it accepted.
    pub starts: usize,
    /// Workers that died, killed or crashed.
    pub deaths: usize,
    /// Workers that reached a step that computes forever.
    pub hung: usize,
    /// The most workers checked out at once.
    pub max_active: usize,
}

/// What the pool's workers share with it.
#[derive(Default)]
pub(super) struct PoolShared {
    starts: AtomicUsize,
    deaths: AtomicUsize,
    hung: AtomicUsize,
}

impl PoolShared {
    pub(super) fn record_start(&self) {
        self.starts.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn record_hang(&self) {
        self.hung.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn record_death(&self) {
        self.deaths.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Ledger {
    stats: PoolStats,
    active: usize,
    permits: BTreeMap<u64, OwnedSemaphorePermit>,
}

/// Fake workers checked out of `slots` slots, each checkout waiting at most
/// `checkout_wait`. Faults planned with [`Self::plan`] strike the next
/// checkouts in order.
pub struct FakeWorkerPool {
    codec: FrameCodec,
    slots: Arc<Semaphore>,
    checkout_wait: Duration,
    faults: Mutex<VecDeque<Option<Fault>>>,
    next_lease: AtomicU64,
    epochs: Mutex<(OwnerEpoch, FrameEpoch)>,
    ledger: Mutex<Ledger>,
    shared: Arc<PoolShared>,
}

impl FakeWorkerPool {
    pub fn new(codec: FrameCodec, slots: usize, checkout_wait: Duration) -> Self {
        Self {
            codec,
            slots: Arc::new(Semaphore::new(slots)),
            checkout_wait,
            faults: Mutex::new(VecDeque::new()),
            // Leases start past zero, so an earlier lease always exists.
            next_lease: AtomicU64::new(1),
            epochs: Mutex::new((OwnerEpoch(0), FrameEpoch(0))),
            ledger: Mutex::new(Ledger::default()),
            shared: Arc::new(PoolShared::default()),
        }
    }

    /// Plans the fault of the next checkout that has none planned yet;
    /// `None` plans a healthy one.
    pub fn plan(&self, fault: Option<Fault>) {
        self.faults
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(fault);
    }

    /// The epochs the pool's next workers run under.
    pub fn set_epochs(&self, owner_epoch: OwnerEpoch, frame_epoch: FrameEpoch) {
        *self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (owner_epoch, frame_epoch);
    }

    pub fn stats(&self) -> PoolStats {
        let mut stats = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stats
            .clone();
        stats.starts = self.shared.starts.load(Ordering::SeqCst);
        stats.deaths = self.shared.deaths.load(Ordering::SeqCst);
        stats.hung = self.shared.hung.load(Ordering::SeqCst);
        stats
    }

    fn give_back(&self, checkout: WorkerCheckout, discarded: bool) {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger.permits.remove(&checkout.lease.0).is_some() {
            ledger.active -= 1;
            if discarded {
                ledger.stats.discards += 1;
            } else {
                ledger.stats.releases += 1;
            }
        }
    }
}

#[async_trait::async_trait]
impl WorkerSlots for FakeWorkerPool {
    async fn checkout(
        &self,
        _owner: &VmOwner,
        _start: &lash_vm_protocol::Start,
    ) -> Result<WorkerCheckout, CheckoutRefusal> {
        let permit =
            tokio::time::timeout(self.checkout_wait, Arc::clone(&self.slots).acquire_owned())
                .await
                .map_err(|_| CheckoutRefusal::TimedOut {
                    waited: self.checkout_wait,
                })?
                .map_err(|_| CheckoutRefusal::Closed)?;
        let lease = ExecutionLease(self.next_lease.fetch_add(1, Ordering::SeqCst));
        let fault = self
            .faults
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .flatten();
        let (owner_epoch, frame_epoch) = *self
            .epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let mut ledger = self
                .ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ledger.permits.insert(lease.0, permit);
            ledger.active += 1;
            ledger.stats.checkouts += 1;
            ledger.stats.max_active = ledger.stats.max_active.max(ledger.active);
        }
        Ok(WorkerCheckout {
            lease,
            transport: Box::new(FakeWorker::new(
                self.codec.clone(),
                lease,
                owner_epoch,
                frame_epoch,
                fault,
                Arc::clone(&self.shared),
            )),
        })
    }

    async fn release(&self, checkout: WorkerCheckout) -> Result<(), CheckoutRefusal> {
        self.give_back(checkout, false);
        Ok(())
    }

    async fn discard(&self, mut checkout: WorkerCheckout) -> Result<(), CheckoutRefusal> {
        checkout.transport.kill().await;
        self.give_back(checkout, true);
        Ok(())
    }
}
