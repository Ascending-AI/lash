//! Allocation reservations for one node and its claimed owners.
//!
//! A reservation is acquired before allocation, moves with that allocation,
//! and releases its charge on drop, including cancelled preparation. Shared
//! identities deduplicate both node and owner charges. The registry holds no
//! owner or allocation strongly; a reader/body must carry its own reservation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use lash_protocol_types::sync::MutexExt as _;

use crate::{ActorKey, Epoch, OwnerRetentionSettings};

/// Whether bytes are acknowledged state or tentative work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryClass {
    Retained,
    Dirty,
}

/// Admission did not reserve any bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReserveError {
    /// Other retained or admitted work must drain before retrying.
    #[error("owner memory is under pressure")]
    Pressure,
    /// This single allocation cannot fit the host's policy; retrying on
    /// another owner cannot help.
    #[error("allocation exceeds owner memory capacity")]
    TooLarge,
}

/// Current unique allocation charges, including pending reservations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    pub retained: u64,
    pub dirty: u64,
}

impl MemoryUsage {
    pub fn total(self) -> u64 {
        self.retained + self.dirty
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AllocationId {
    Private(u64),
    Shared(usize),
}

#[derive(Default)]
struct Holds {
    retained: u64,
    dirty: u64,
}

struct Allocation {
    bytes: u64,
    holders: BTreeMap<u64, Holds>,
}

#[derive(Default)]
struct Ledger {
    next: u64,
    allocations: BTreeMap<AllocationId, Allocation>,
    owners: BTreeMap<u64, MemoryUsage>,
    registry: BTreeMap<(ActorKey, Epoch), Weak<Owner>>,
    node: MemoryUsage,
    pressure: Option<Pressure>,
}

struct Pressure {
    owner: u64,
    owner_total: u64,
    owner_dirty: u64,
    node_total: u64,
    node_dirty: u64,
}

impl Pressure {
    fn blocked(&self, ledger: &Ledger, policy: OwnerRetentionSettings) -> bool {
        let Some(owner) = ledger.owners.get(&self.owner) else {
            return false;
        };
        owner.total() > policy.max_owner_bytes.get() - self.owner_total
            || owner.dirty > policy.max_owner_dirty_bytes.get() - self.owner_dirty
            || ledger.node.total() > policy.max_node_bytes.get() - self.node_total
            || ledger.node.dirty > policy.max_node_dirty_bytes.get() - self.node_dirty
    }
}

struct Node {
    policy: OwnerRetentionSettings,
    ledger: Mutex<Ledger>,
}

/// One accounting domain per serving backend, shared by derived backends.
#[derive(Clone)]
pub struct NodeMemory(Arc<Node>);

struct Owner {
    node: Arc<Node>,
    id: u64,
}

impl Drop for Owner {
    fn drop(&mut self) {
        let mut ledger = self.node.ledger.lock_recover();
        ledger.owners.remove(&self.id);
        if ledger
            .pressure
            .as_ref()
            .is_some_and(|pressure| pressure.owner == self.id)
        {
            ledger.pressure = None;
        }
        ledger.registry.retain(|_, owner| owner.strong_count() != 0);
    }
}

impl NodeMemory {
    /// The caller supplies a validated host policy.
    pub fn new(policy: OwnerRetentionSettings) -> Self {
        Self(Arc::new(Node {
            policy,
            ledger: Mutex::default(),
        }))
    }

    /// Reusing a handle for the same claim shares its attribution. A new
    /// epoch always gets a separate owner; the registry cannot keep it alive.
    pub fn owner(&self, actor: &ActorKey, epoch: Epoch) -> OwnerMemory {
        let mut ledger = self.0.ledger.lock_recover();
        let key = (actor.clone(), epoch);
        if let Some(owner) = ledger.registry.get(&key).and_then(Weak::upgrade) {
            return OwnerMemory(owner);
        }
        ledger.registry.retain(|_, owner| owner.strong_count() != 0);
        ledger.next += 1;
        let id = ledger.next;
        let owner = Arc::new(Owner {
            node: Arc::clone(&self.0),
            id,
        });
        ledger.owners.insert(id, MemoryUsage::default());
        ledger.registry.insert(key, Arc::downgrade(&owner));
        OwnerMemory(owner)
    }

    pub fn policy(&self) -> OwnerRetentionSettings {
        self.0.policy
    }

    pub fn usage(&self) -> MemoryUsage {
        self.0.ledger.lock_recover().node
    }

    /// Pressure is level-triggered until some reservation releases capacity.
    /// Idle owners check it at their existing yield/quiet points.
    pub fn under_pressure(&self) -> bool {
        self.0.ledger.lock_recover().pressure.is_some()
    }
}

/// Attribution for one actor epoch. Clones share it, without cloning bytes.
#[derive(Clone)]
pub struct OwnerMemory(Arc<Owner>);

impl OwnerMemory {
    pub fn usage(&self) -> MemoryUsage {
        self.0
            .node
            .ledger
            .lock_recover()
            .owners
            .get(&self.0.id)
            .copied()
            .unwrap_or_default()
    }

    pub fn under_pressure(&self) -> bool {
        self.0.node.ledger.lock_recover().pressure.is_some()
    }

    pub fn policy(&self) -> OwnerRetentionSettings {
        self.0.node.policy
    }

    /// Claim capacity before allocating a new private working set.
    pub fn reserve(
        &self,
        bytes: u64,
        class: MemoryClass,
    ) -> Result<MemoryReservation, ReserveError> {
        let mut ledger = self.0.node.ledger.lock_recover();
        ledger.next += 1;
        let id = AllocationId::Private(ledger.next);
        self.reserve_in(&mut ledger, id, bytes, class)
    }

    /// Reserve a persistent allocation shared by drafts/readers. `identity`
    /// must name the same live allocation of exactly `bytes` capacity until
    /// every reservation for it is dropped. Keep the token with the allocation,
    /// including header/index capacity, and never use a content hash as identity.
    pub fn reserve_shared(
        &self,
        identity: usize,
        bytes: u64,
        class: MemoryClass,
    ) -> Result<MemoryReservation, ReserveError> {
        let mut ledger = self.0.node.ledger.lock_recover();
        self.reserve_in(&mut ledger, AllocationId::Shared(identity), bytes, class)
    }

    fn reserve_in(
        &self,
        ledger: &mut Ledger,
        id: AllocationId,
        bytes: u64,
        class: MemoryClass,
    ) -> Result<MemoryReservation, ReserveError> {
        let policy = self.0.node.policy;
        if bytes > policy.max_owner_bytes.get()
            || bytes > policy.max_node_bytes.get()
            || (class == MemoryClass::Dirty
                && (bytes > policy.max_owner_dirty_bytes.get()
                    || bytes > policy.max_node_dirty_bytes.get()))
        {
            return Err(ReserveError::TooLarge);
        }
        let allocation = ledger.allocations.get(&id);
        if allocation.is_some_and(|allocation| allocation.bytes != bytes) {
            return Err(ReserveError::TooLarge);
        }
        let holds = allocation.and_then(|allocation| allocation.holders.get(&self.0.id));
        let owner_new = holds.is_none();
        let owner_dirty = class == MemoryClass::Dirty && holds.is_none_or(|holds| holds.dirty == 0);
        let node_new = allocation.is_none();
        let node_dirty = class == MemoryClass::Dirty
            && allocation
                .is_none_or(|allocation| allocation.holders.values().all(|holds| holds.dirty == 0));
        let owner_usage = ledger.owners.get(&self.0.id).copied().unwrap_or_default();
        let exceeds = |current: u64, add: bool, limit: u64| add && current > limit - bytes;
        if exceeds(owner_usage.total(), owner_new, policy.max_owner_bytes.get())
            || exceeds(
                owner_usage.dirty,
                owner_dirty,
                policy.max_owner_dirty_bytes.get(),
            )
            || exceeds(ledger.node.total(), node_new, policy.max_node_bytes.get())
            || exceeds(
                ledger.node.dirty,
                node_dirty,
                policy.max_node_dirty_bytes.get(),
            )
        {
            ledger.pressure = Some(Pressure {
                owner: self.0.id,
                owner_total: if owner_new { bytes } else { 0 },
                owner_dirty: if owner_dirty { bytes } else { 0 },
                node_total: if node_new { bytes } else { 0 },
                node_dirty: if node_dirty { bytes } else { 0 },
            });
            return Err(ReserveError::Pressure);
        }
        let allocation = ledger.allocations.entry(id).or_insert_with(|| Allocation {
            bytes,
            holders: BTreeMap::new(),
        });
        let holds = allocation.holders.entry(self.0.id).or_default();
        match class {
            MemoryClass::Retained => holds.retained += 1,
            MemoryClass::Dirty => holds.dirty += 1,
        }
        // Promote existing clean charges when the first dirty holder arrives.
        let owner_usage = ledger.owners.entry(self.0.id).or_default();
        add_charge(owner_usage, bytes, owner_new, owner_dirty, class);
        add_charge(&mut ledger.node, bytes, node_new, node_dirty, class);
        Ok(MemoryReservation {
            owner: self.clone(),
            id,
            class,
        })
    }
}

fn add_charge(
    usage: &mut MemoryUsage,
    bytes: u64,
    new: bool,
    first_dirty: bool,
    class: MemoryClass,
) {
    if new {
        match class {
            MemoryClass::Retained => usage.retained += bytes,
            MemoryClass::Dirty => usage.dirty += bytes,
        }
    } else if first_dirty {
        usage.retained -= bytes;
        usage.dirty += bytes;
    }
}

/// The unique obligation to release one allocation hold. Never clone a
/// reservation; reserve the shared identity for each independent holder.
pub struct MemoryReservation {
    owner: OwnerMemory,
    id: AllocationId,
    class: MemoryClass,
}

impl MemoryReservation {
    /// Acknowledgement transfers the charge without changing total bytes.
    pub fn into_retained(mut self) -> Self {
        if self.class == MemoryClass::Dirty {
            self.remove(true);
            self.class = MemoryClass::Retained;
        }
        self
    }

    fn remove(&self, retain: bool) {
        let mut ledger = self.owner.0.node.ledger.lock_recover();
        let Some(allocation) = ledger.allocations.get_mut(&self.id) else {
            return;
        };
        let bytes = allocation.bytes;
        let Some(holds) = allocation.holders.get_mut(&self.owner.0.id) else {
            return;
        };
        match self.class {
            MemoryClass::Retained => holds.retained -= 1,
            MemoryClass::Dirty => holds.dirty -= 1,
        }
        if retain {
            holds.retained += 1;
        }
        let owner_empty = holds.retained == 0 && holds.dirty == 0;
        let owner_clean = self.class == MemoryClass::Dirty && holds.dirty == 0;
        if owner_empty {
            allocation.holders.remove(&self.owner.0.id);
        }
        let node_empty = allocation.holders.is_empty();
        let node_clean = self.class == MemoryClass::Dirty
            && allocation.holders.values().all(|holds| holds.dirty == 0);
        let owner_usage = ledger.owners.entry(self.owner.0.id).or_default();
        remove_charge(owner_usage, bytes, owner_empty, owner_clean, self.class);
        remove_charge(&mut ledger.node, bytes, node_empty, node_clean, self.class);
        if node_empty {
            ledger.allocations.remove(&self.id);
        }
        if ledger
            .pressure
            .as_ref()
            .is_some_and(|pressure| !pressure.blocked(&ledger, self.owner.0.node.policy))
        {
            ledger.pressure = None;
        }
    }
}

fn remove_charge(
    usage: &mut MemoryUsage,
    bytes: u64,
    empty: bool,
    last_dirty: bool,
    class: MemoryClass,
) {
    if empty {
        match class {
            MemoryClass::Retained => usage.retained -= bytes,
            MemoryClass::Dirty => usage.dirty -= bytes,
        }
    } else if last_dirty {
        usage.dirty -= bytes;
        usage.retained += bytes;
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.remove(false);
    }
}
