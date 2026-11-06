//! Affine state authority for a verified region's unpublished exit.
use super::*;
use crate::arena::{Arena, ArenaAuthority, PreparedBatch, PreparedGraphProof, RegionRegistration};
use crate::paged::heap_impl::PagedHeap;
use lash_kernel_doc::{Value, arena::ArenaHandle};
mod exit;
mod loading;
mod storage;
pub use exit::{LeaseGraphExit, LeaseProjection};
use std::cell::{Cell as Flag, RefMut};
use storage::LocalStore;
pub use storage::{
    LeaseAllocation, LeaseCallable, LeaseFacts, LeaseFault, LeaseMeasure, LeaseMember, LeaseObject,
    LeaseOp, LeaseStep, LeaseValue, NativeBatch, ReadyLeaseStep,
};

#[derive(Debug, Default)]
pub(super) struct Lineage {
    leased: Flag<bool>,
}
struct Exclusion {
    lineage: Rc<Lineage>,
    token: Rc<()>,
}
impl Drop for Exclusion {
    fn drop(&mut self) {
        self.lineage.leased.set(false);
    }
}
impl Ledger {
    pub(super) fn require_unleased(&self) -> Result<(), HeapFault> {
        if self.identity.leased.get() {
            Err(invalid("ledger is owned by a live region lease"))
        } else {
            Ok(())
        }
    }
}

/// Exclusive state authority. Uncommitted work is discarded on drop.
pub struct StateLease<'s> {
    heap: &'s mut PagedHeap,
    storage: LeaseStorage<'s>,
    entry: Stamp,
    dependencies: Dependencies,
    exclusion: Exclusion,
    reserved: Option<(TaskId, u64)>,
    local: LocalStore,
    loader: Option<Box<dyn LeaseLoader>>,
}
/// Canonical projection can decode an entry body without executing guest work.
/// The lease authenticates its unchanged source identity and member frontier.
pub trait LeaseLoader {
    fn load(
        &self,
        arena: &mut Arena,
        heap: &PagedHeap,
        handle: ArenaHandle,
    ) -> Result<(), HeapFault>;
}
enum LeaseStorage<'s> {
    Vacant,
    Arena(ArenaAuthority<'s>),
    Installation {
        registration: RegionRegistration<'s>,
        batch: PreparedBatch,
    },
}
/// Actual surviving state, supplied only by C's sealed materialization.
/// This payload alone proves no correspondence with source semantics.
pub struct StateExit {
    pub inputs: Box<[Owner]>,
    pub outputs: Box<[(Owner, PublicationValue)]>,
    pub graph: LeaseGraphExit,
    pub invocations: Option<InvocationRange>,
    pub peak: u64,
    pub delta: i128,
}
/// An authenticated exit with the ledger borrowed until commit or abandonment.
pub struct PreparedStateExit<'s> {
    ledger: RefMut<'s, Ledger>,
    next: State,
    revision: u64,
    registration: RegionRegistration<'s>,
    batch: Option<PreparedBatch>,
    members: Vec<crate::arena::RegionMemberBody>,
    revisions: Vec<crate::arena::RegionBodyMetadata>,
    _exclusion: Exclusion,
}
impl<'s> StateLease<'s> {
    pub fn acquire(heap: &'s mut PagedHeap, arena: &'s mut Arena) -> Result<Self, HeapFault> {
        Self::acquire_with(heap, ArenaAuthority::Borrowed(arena))
    }
    /// Own the machine's shared-arena borrow across run, materialization and
    /// publication. No other shared view may mutate or replace that arena.
    pub fn acquire_shared(
        heap: &'s mut PagedHeap,
        arena: &'s std::cell::RefCell<Arena>,
    ) -> Result<Self, HeapFault> {
        let arena = arena
            .try_borrow_mut()
            .map_err(|_| invalid("region arena is already borrowed"))?;
        Self::acquire_with(heap, ArenaAuthority::Shared(arena))
    }
    fn acquire_with(
        heap: &'s mut PagedHeap,
        authority: ArenaAuthority<'s>,
    ) -> Result<Self, HeapFault> {
        let arena = authority.read();
        if arena.registration_active() || arena.has_pending_source_changes() {
            return Err(invalid("region entry has unpublished arena changes"));
        }
        let ledger = heap.region_ledger()?;
        ledger.require_unleased()?;
        ledger
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("logical revision overflow"))?;
        let entry = ledger.stamp();
        let dependencies = Dependencies::capture(&ledger, arena, &[])
            .ok_or_else(|| invalid("region entry dependencies unavailable"))?;
        let local = LocalStore::new()?;
        let exclusion = Exclusion {
            lineage: Rc::clone(&ledger.identity),
            token: Rc::new(()),
        };
        exclusion.lineage.leased.set(true);
        drop(ledger);
        Ok(Self {
            heap,
            storage: LeaseStorage::Arena(authority),
            entry,
            dependencies,
            exclusion,
            reserved: None,
            local,
            loader: None,
        })
    }
    pub fn arena(&self) -> &Arena {
        match &self.storage {
            LeaseStorage::Vacant => unreachable!("consumed region storage"),
            LeaseStorage::Arena(authority) => authority.read(),
            LeaseStorage::Installation { registration, .. } => registration.read(),
        }
    }
    pub fn heap(&self) -> &PagedHeap {
        self.heap
    }
    /// Bind all entry reads, including discarded operands and callee/captures.
    /// The dependency stamp must already be this lease's original stamp.
    pub fn bind_dependencies(&mut self, dependencies: Dependencies) -> Result<(), HeapFault> {
        let ledger = self.heap.region_ledger()?;
        self.authenticate(&ledger)?;
        if !dependencies.is_current(&ledger, self.arena()) {
            return Err(invalid("stale or foreign region dependencies"));
        }
        self.dependencies.extend(dependencies)
    }
    pub fn reserve_invocations(
        &mut self,
        caller: &Invocation,
        count: u64,
    ) -> Result<InvocationRange, HeapFault> {
        let ledger = self.heap.region_ledger()?;
        self.authenticate(&ledger)?;
        let range = ledger.reserve_region_invocations(
            caller,
            count,
            Rc::clone(&self.exclusion.token),
            self.heap.pages(),
        )?;
        let Owner::Frame { task, .. } = caller.frame() else {
            unreachable!()
        };
        if self
            .reserved
            .is_some_and(|reserved| reserved != (task, count))
        {
            return Err(invalid(
                "a region invocation reservation cannot be replaced",
            ));
        }
        self.reserved = Some((task, count));
        Ok(range)
    }
    fn authenticate_entry(&self, ledger: &Ledger) -> Result<(), HeapFault> {
        if !ledger.is_current(&self.entry) || !self.exclusion.lineage.leased.get() {
            return Err(invalid("stale or foreign region entry"));
        }
        Ok(())
    }
    fn authenticate(&self, ledger: &Ledger) -> Result<(), HeapFault> {
        if !ledger.is_current(&self.entry)
            || !self.dependencies.is_current(ledger, self.arena())
            || !self.exclusion.lineage.leased.get()
        {
            return Err(invalid(&format!(
                "stale or foreign region lease: same_lineage={}, entry_revision={}, current_revision={}, dependencies_current={}, exclusion_live={}",
                ledger.owns_stamp(&self.entry),
                self.entry.revision,
                ledger.revision,
                self.dependencies.is_current(ledger, self.arena()),
                self.exclusion.lineage.leased.get()
            )));
        }
        Ok(())
    }
    pub fn prepare(
        self,
        exit: StateExit,
        memory_bound: u64,
    ) -> Result<PreparedStateExit<'s>, HeapFault> {
        {
            let ledger = self.heap.region_ledger()?;
            self.authenticate(&ledger)?;
        }
        let Self {
            heap,
            storage,
            entry,
            mut dependencies,
            exclusion,
            reserved,
            local,
            loader: _,
        } = self;
        let ledger = heap.region_ledger()?;
        if !ledger.is_current(&entry) {
            return Err(invalid("region entry changed"));
        }
        let revision = ledger
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("logical revision overflow"))?;
        exit.graph.authenticate(&local, &exclusion)?;
        let LeaseStorage::Installation {
            mut registration,
            batch,
        } = storage
        else {
            return Err(invalid("region exit requires its frozen graph packet"));
        };
        let projected =
            exit.graph
                .project(&local, &ledger, registration.arena(), heap.pages(), &exit)?;
        let (batch, members) = registration.prepare_local_members(
            batch,
            &projected.edits,
            projected.tables,
            projected.lists,
        )?;
        let revisions = projected.revisions;
        let batch = Some(batch);
        if !dependencies.is_current(&ledger, registration.arena()) {
            return Err(invalid(
                "region dependency changed during storage preparation",
            ));
        }
        let graph = batch.as_ref().and_then(PreparedBatch::graph_proof).cloned();
        if graph
            .as_ref()
            .is_some_and(|graph| !graph.is_current(registration.arena()))
        {
            return Err(invalid("stale region graph proof"));
        }
        let mut handles = Vec::new();
        let mut prepared = projected.prepared;
        if let Some(range) = exit.invocations {
            if !range.matches_region(&entry, &exclusion.token, reserved) {
                return Err(invalid("foreign or sibling region invocation range"));
            }
            prepared = range.install_region(prepared, &exit.outputs, heap.pages())?;
        } else {
            if exit.outputs.iter().any(|(owner,_)| matches!(owner, Owner::Frame{invocation,..} | Owner::Value{invocation,..} if *invocation >= ledger.state.next_invocation)) {
                return Err(invalid("region retained an activation without a reservation"));
            }
        }
        for entity in &prepared.projected {
            if let Some(handle) = registration.arena().logical_handle(*entity) {
                handles.push(handle);
            }
        }
        let captured = Dependencies::capture(&ledger, registration.arena(), &handles)
            .ok_or_else(|| invalid("region projected dependency unavailable"))?;
        dependencies.extend(captured)?;
        let after = prepared.balance()?.total()?;
        if i128::from(after) - i128::from(prepared.before) != exit.delta
            || exit.peak < prepared.peak
            || exit.peak > memory_bound
        {
            return Err(invalid(&format!(
                "region admission mismatch: before={}, after={after}, supplied_delta={}, supplied_peak={}, required_peak={}, memory_bound={memory_bound}",
                prepared.before, exit.delta, exit.peak, prepared.peak
            )));
        }
        if let Some(graph) = &graph {
            graph.reserve_for_publication(registration.arena())?;
        }
        registration.arena().reserve_region_members(
            &members,
            batch.as_ref().map_or(0, PreparedBatch::region_len),
            revisions.len(),
        )?;
        if !ledger.is_current(&entry)
            || !dependencies.is_current(&ledger, registration.arena())
            || batch
                .as_ref()
                .is_some_and(|batch| !batch.is_current(registration.arena()))
        {
            return Err(invalid("region exit changed during preparation"));
        }
        Ok(PreparedStateExit {
            ledger,
            next: prepared.next,
            revision,
            registration,
            batch,
            members,
            revisions,
            _exclusion: exclusion,
        })
    }
}
impl PreparedStateExit<'_> {
    /// C supplies only nonpanicking, non-reentrant swaps of already staged VM
    /// replacements. No source execution, allocation, or host code is allowed.
    pub fn commit(self, publish_vm: impl FnOnce()) -> Result<(), HeapFault> {
        let Self {
            mut ledger,
            next,
            revision,
            mut registration,
            batch,
            members,
            revisions,
            _exclusion,
        } = self;
        if let Some(batch) = batch {
            batch.publish_region(registration.arena());
        }
        for member in members {
            member.publish(registration.arena());
        }
        registration.publish_revisions(revisions);
        ledger.facts.get_mut().clear();
        ledger.state = next;
        ledger.revision = revision;
        registration.commit();
        publish_vm();
        drop(registration);
        Ok(())
    }
}
fn arena_fault(error: lash_kernel_doc::NativeError) -> HeapFault {
    match error {
        lash_kernel_doc::NativeError::Host(error) => error,
        error => invalid(&error.to_string()),
    }
}
#[cfg(any(test, feature = "perf-witness"))]
pub mod fixtures {
    use super::*;
    pub fn graph() -> LeaseGraphExit {
        super::exit::fixture()
    }
    pub struct StorageWork {
        pub slab_slots: usize,
        pub live_objects: usize,
        pub changed_members: usize,
        pub accepted_phases: u64,
        pub materialized: bool,
        pub release_visits: u64,
        pub binding_peak: u64,
    }
    pub fn storage_work(lease: &StateLease<'_>) -> StorageWork {
        StorageWork {
            slab_slots: lease.local.slots.len(),
            live_objects: lease.local.slots.iter().filter(|slot| slot.live).count(),
            changed_members: lease
                .local
                .slots
                .iter()
                .filter_map(|slot| slot.existing.as_ref())
                .map(|existing| existing.overlays.iter().count())
                .sum(),
            accepted_phases: lease.local.version,
            materialized: lease.local.frozen,
            release_visits: lease.local.release_visits,
            binding_peak: lease.local.binding_peak,
        }
    }

    pub fn exit(
        graph: LeaseGraphExit,
        inputs: impl Into<Box<[Owner]>>,
        outputs: impl Into<Box<[(Owner, PublicationValue)]>>,
        peak: u64,
        delta: i128,
    ) -> StateExit {
        StateExit {
            inputs: inputs.into(),
            outputs: outputs.into(),
            graph,
            invocations: None,
            peak,
            delta,
        }
    }
}
