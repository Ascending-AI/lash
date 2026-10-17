//! A heap natives run on outside any run: the helper-release build probes
//! every native on one and fingerprints what it observes (FIG-6688). Its
//! operations are the paged heap's own, so a probe sees the normal forms,
//! identities and memory refusals a run's native call would.

use std::ops::{Deref, DerefMut};

use lash_kernel_doc::heap::{EntityId, FieldName, HeapFault, LogicalBytes, NativeHeap, Value};

use super::census_impl::Census;
use super::tree::Tree;
use super::{
    AllocationCensus as _, EditEpoch, HeapWriteError, PageSource, PagedHeap, SemanticHeap,
};

/// What a fresh probe heap may hold before [`ProbeHeap::hold`] bounds it.
const BUILD_ROOM: LogicalBytes = LogicalBytes(1 << 40);

/// A heap a probe builds its inputs on and calls one native over.
pub struct ProbeHeap(PagedHeap);

impl ProbeHeap {
    /// An empty heap whose pages are sealed into `source`.
    ///
    /// # Errors
    ///
    /// The heap's faults installing its census.
    pub fn fresh(source: Box<dyn PageSource>) -> Result<Self, HeapFault> {
        let mut heap = PagedHeap::fresh(source, EditEpoch(1));
        let census = Census::restore(heap.certificate(), BUILD_ROOM)
            .map_err(|fault| HeapFault::Malformed(fault.to_string()))?;
        heap.set_census(Box::new(census))?;
        Ok(Self(heap))
    }

    /// Writes element `index` of `list`; `index == len` appends.
    ///
    /// # Errors
    ///
    /// The heap's refusals.
    pub fn list_write(
        &mut self,
        list: EntityId,
        index: u64,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        self.0.list_write(list, index, value)
    }

    /// Adds `member` to `set`.
    ///
    /// # Errors
    ///
    /// The heap's refusals.
    pub fn set_insert(&mut self, set: EntityId, member: Value) -> Result<(), HeapWriteError> {
        self.0.set_write(set, member, true)
    }

    /// Writes `value` under `key` in `map`.
    ///
    /// # Errors
    ///
    /// The heap's refusals.
    pub fn map_write(
        &mut self,
        map: EntityId,
        key: Value,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        self.0.map_write(map, key, value)
    }

    /// Writes field `field` of `record`.
    ///
    /// # Errors
    ///
    /// The heap's refusals.
    pub fn record_write(
        &mut self,
        record: EntityId,
        field: &str,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        self.0.record_write(record, FieldName::Static(field), value)
    }

    /// `value` in the heap's canonical form.
    ///
    /// # Errors
    ///
    /// The heap's refusals.
    pub fn canonical_value(&mut self, value: Value) -> Result<Value, HeapWriteError> {
        self.0.canonical_value(value)
    }

    /// Holds `args` as the probed call's arguments, and bounds the heap to
    /// what it holds now and `room` logical bytes more.
    ///
    /// # Errors
    ///
    /// The heap's refusals, and a room past the logical byte range.
    pub fn hold(&mut self, args: &[Value], room: u64) -> Result<(), HeapWriteError> {
        self.0
            .set_execution_roots(Tree::new(false), Tree::new(false), args, &[], 0, &[])?;
        let certificate = self.0.certificate();
        let bound = certificate
            .upper
            .0
            .checked_add(room)
            .ok_or_else(|| HeapFault::Malformed("a probe's memory room overflows".into()))?;
        let census = Census::restore(certificate, LogicalBytes(bound))
            .map_err(|fault| HeapFault::Malformed(fault.to_string()))?;
        self.0.set_census(Box::new(census))?;
        Ok(())
    }
}

impl Deref for ProbeHeap {
    type Target = dyn NativeHeap;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ProbeHeap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
