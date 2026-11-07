//! Unpublished source holdings and generation-qualified local storage.
use super::*;
use crate::arena::Key;
use lash_kernel_doc::heap::{HostResource, PageValue};
use lash_kernel_doc::{
    NativeError,
    arena::{Body, ShortText},
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub struct LeaseValue {
    pub(super) repr: Repr,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Repr {
    Scalar(Value),
    Object(LeaseObject, Option<Value>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseObject {
    pub(super) lease: u64,
    pub(super) slot: u32,
    pub(super) generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Existing,
    List,
    Map,
    Set,
    Record,
    Tuple,
    Cell,
    Closure,
    Text,
    Bytes,
    /// A fresh immutable body a native built, with no lease constructor.
    Leaf(Leaf),
}
/// The immutable kinds only a native builds into a lease, by adoption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Leaf {
    BigInt,
    Timestamp,
    Function,
    /// Its kind, message and data, in that order, as a value body.
    Error,
}
impl Kind {
    fn editable(self) -> bool {
        matches!(
            self,
            Self::List | Self::Map | Self::Set | Self::Record | Self::Cell
        )
    }
    fn mutable(self) -> bool {
        matches!(
            self,
            Self::List | Self::Map | Self::Set | Self::Record | Self::Cell | Self::Closure
        )
    }
    pub(super) fn naming(self, handle: ArenaHandle) -> Value {
        match self {
            Self::List => Value::List(handle),
            Self::Map => Value::Map(handle),
            Self::Set => Value::Set(handle),
            Self::Record => Value::Record(handle),
            Self::Tuple => Value::Tuple(handle),
            Self::Cell => Value::ObjectRef(handle),
            Self::Closure => Value::Closure(handle),
            Self::Text => Value::LongText(handle),
            Self::Bytes => Value::Bytes(handle),
            Self::Leaf(Leaf::BigInt) => Value::BigInt(handle),
            Self::Leaf(Leaf::Timestamp) => Value::BigTimestamp(handle),
            Self::Leaf(Leaf::Function) => Value::Function(handle),
            Self::Leaf(Leaf::Error) => Value::Error(handle),
            Self::Existing => unreachable!("existing value keeps its kind"),
        }
    }
}
#[derive(Clone, Debug)]
pub enum LeaseMember {
    List(u64),
    Table(LeaseValue),
    Field(Arc<str>),
    Variable,
}
pub struct LeaseAllocation<'a> {
    body: Allocation<'a>,
}
enum Allocation<'a> {
    List(&'a [LeaseValue]),
    Map(&'a [(LeaseValue, LeaseValue)]),
    Set(&'a [LeaseValue]),
    Record(&'a [(LeaseValue, LeaseValue)]),
    RecordFields {
        fields: &'a [Arc<str>],
        values: &'a [LeaseValue],
    },
    Tuple(&'a [LeaseValue]),
    Cell(&'a LeaseValue),
    Text(&'a str),
    Bytes(&'a [u8]),
    BigInt(&'a lash_kernel_doc::Integer),
    Function(lash_kernel_value::names::CallableId),
    Closure {
        identity: Arc<lash_kernel_doc::arena::ClosureIdentity>,
        captures: &'a [LeaseValue],
    },
}
impl<'a> LeaseAllocation<'a> {
    pub fn closure(
        identity: Arc<lash_kernel_doc::arena::ClosureIdentity>,
        captures: &'a [LeaseValue],
    ) -> Self {
        Self {
            body: Allocation::Closure { identity, captures },
        }
    }

    pub fn list(values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::List(values),
        }
    }
    pub fn map(entries: &'a [(LeaseValue, LeaseValue)]) -> Self {
        Self {
            body: Allocation::Map(entries),
        }
    }
    pub fn set(values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::Set(values),
        }
    }
    pub fn record(entries: &'a [(LeaseValue, LeaseValue)]) -> Self {
        Self {
            body: Allocation::Record(entries),
        }
    }
    /// Static record spellings belong to the record allocation, without an
    /// intermediate text occurrence or an extra logical holding.
    pub fn record_fields(fields: &'a [Arc<str>], values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::RecordFields { fields, values },
        }
    }
    pub fn tuple(values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::Tuple(values),
        }
    }
    pub fn cell(value: &'a LeaseValue) -> Self {
        Self {
            body: Allocation::Cell(value),
        }
    }
    pub fn text(value: &'a str) -> Self {
        Self {
            body: Allocation::Text(value),
        }
    }
    pub fn bytes(value: &'a [u8]) -> Self {
        Self {
            body: Allocation::Bytes(value),
        }
    }
    /// An integer outside the inline word, as the arena holds it.
    pub fn big_int(value: &'a lash_kernel_doc::Integer) -> Self {
        Self {
            body: Allocation::BigInt(value),
        }
    }
    pub fn function(callable: lash_kernel_value::names::CallableId) -> Self {
        Self {
            body: Allocation::Function(callable),
        }
    }
}
pub enum LeaseOp<'a> {
    /// One source root transfer. The bool selects zero-byte capture pricing.
    Transfer {
        holds: &'a [(LeaseValue, bool)],
        releases: &'a [(LeaseValue, bool)],
    },
    /// The same source transfer at a canonical activation Enter or Leave.
    /// Header prices are fixed here, never supplied as a byte allowance.
    FrameTransfer {
        holds: &'a [(LeaseValue, bool)],
        releases: &'a [(LeaseValue, bool)],
        entering: bool,
    },
    Allocate(LeaseAllocation<'a>),
    /// Allocation and the source occurrences moved into its members form one
    /// canonical phase. Tuple and variable construction have no gross-output
    /// prefix between allocation and these moves.
    AllocateMove {
        allocation: LeaseAllocation<'a>,
        releases: &'a [LeaseValue],
    },
    /// The source transfer of a native call's step, accepting the graph its
    /// answer adopted ([`StateLease::adopt`]) in the same phase.
    Adopt {
        holds: &'a [(LeaseValue, bool)],
        releases: &'a [(LeaseValue, bool)],
    },
    Member {
        object: LeaseObject,
        member: LeaseMember,
        next: Option<LeaseValue>,
    },
    Grow {
        object: LeaseObject,
        additional: u64,
    },
    HoldCapture {
        cell: LeaseObject,
        count: u64,
    },
    ReleaseCapture {
        cell: LeaseObject,
        count: u64,
    },
    Hold {
        value: LeaseValue,
        count: u64,
    },
    Release {
        value: LeaseValue,
        count: u64,
    },
}
#[derive(Debug, thiserror::Error)]
pub enum LeaseFault {
    #[error("operation requires a state boundary")]
    BoundaryRequired,
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error(transparent)]
    Native(#[from] NativeError),
}
/// Canonical state selectors, rather than caller supplied byte allowances.
pub struct LeaseMeasure {
    selector: Measure,
}
enum Measure {
    Members,
    Payload,
    Released,
    Depth,
}
impl LeaseMeasure {
    pub const MEMBERS: Self = Self {
        selector: Measure::Members,
    };
    pub const PAYLOAD: Self = Self {
        selector: Measure::Payload,
    };
    pub const RELEASED: Self = Self {
        selector: Measure::Released,
    };
    pub const DEPTH: Self = Self {
        selector: Measure::Depth,
    };
}
#[derive(Default)]
pub struct LeaseFacts {
    delta: i128,
    peak: u64,
    members: u64,
    payload: u64,
    released: u64,
    depth: u32,
}
impl LeaseFacts {
    pub fn delta(&self) -> i128 {
        self.delta
    }
    pub fn peak(&self) -> u64 {
        self.peak
    }
    pub fn measure(&self, selector: &LeaseMeasure) -> Option<u64> {
        Some(match selector.selector {
            Measure::Members => self.members,
            Measure::Payload => self.payload,
            Measure::Released => self.released,
            Measure::Depth => u64::from(self.depth),
        })
    }
}
pub struct LeaseStep<'a, 's> {
    lease: &'a mut StateLease<'s>,
    candidate: Candidate<'a>,
    facts: LeaseFacts,
}
pub struct ReadyLeaseStep<'a, 's> {
    step: LeaseStep<'a, 's>,
    body: Option<LocalBody>,
    result: Option<LeaseValue>,
    list: Vec<(u64, Option<Entry>, Option<Entry>)>,
}
struct Candidate<'a> {
    op: LeaseOp<'a>,
    member: Option<MemberPlan>,
    allocation: Option<(Kind, u64, u64)>,
    result: Option<LeaseValue>,
}
struct MemberPlan {
    address: Address,
    old: Option<Entry>,
    next: Option<Entry>,
    structural: bool,
    body_bytes: u64,
}
#[derive(Clone)]
pub(super) enum Spelling {
    Value(LeaseValue),
    Field(Arc<str>),
}
#[derive(Clone)]
pub(super) struct Entry {
    pub(super) key: Spelling,
    pub(super) value: LeaseValue,
    pub(super) sequence: u64,
}
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum Address {
    List(u64),
    Table(LeaseKey),
    Variable,
}
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum LeaseKey {
    Canonical(Key),
    Local(LeaseObject),
    Composite(Vec<LeaseKey>),
}
pub(super) struct OverlayMember {
    pub(super) original: Option<Entry>,
    pub(super) current: Option<Entry>,
}
pub(super) enum LocalBody {
    Values(Vec<LeaseValue>),
    Table(LocalTable),
    Cell(LeaseValue),
    Text(String),
    Bytes(Vec<u8>),
    Closure(LocalClosure),
    /// A big integer's, timestamp's or function's body.
    Leaf(LeafBody),
}
pub(super) enum LeafBody {
    Integer(lash_kernel_doc::Integer),
    Function(lash_kernel_value::names::CallableId),
}
pub(super) struct LocalTable {
    pub(super) entries: Vec<Entry>,
    index: std::collections::HashMap<LeaseKey, usize, foldhash::fast::RandomState>,
    pub(super) next: u64,
}
#[derive(Default)]
struct TableInputs {
    index: std::collections::HashMap<LeaseKey, usize, foldhash::fast::RandomState>,
    // Source positions only: first spelling and last value. No target body or
    // logical holding is allocated while deriving the candidate facts.
    rows: Vec<(usize, usize)>,
}
pub(super) struct LocalClosure {
    pub(super) identity: Arc<lash_kernel_doc::arena::ClosureIdentity>,
    pub(super) captures: Vec<LeaseValue>,
}
enum LocalCallable {
    Function(lash_kernel_value::names::CallableId),
    Closure(LocalClosure),
}
pub struct LeaseCallable<'a> {
    identity: &'a lash_kernel_value::names::CallableId,
    closure: Option<&'a lash_kernel_doc::arena::ClosureIdentity>,
    captures: &'a [LeaseValue],
}
impl LeaseCallable<'_> {
    pub fn identity(&self) -> &lash_kernel_value::names::CallableId {
        self.identity
    }
    pub fn closure(&self) -> Option<&lash_kernel_doc::arena::ClosureIdentity> {
        self.closure
    }
    pub fn captures(&self) -> &[LeaseValue] {
        self.captures
    }
}
pub(super) struct Existing {
    pub(super) value: Option<Value>,
    pub(super) entity: Option<EntityId>,
    edges: Vec<(LeaseObject, u64)>,
    pub(super) overlays: overlay::OverlayMap,
    pub(super) len: u64,
    pub(super) next: u64,
    callable: Option<LocalCallable>,
    pub(super) cold: bool,
}
pub(super) struct ObjectSlot {
    pub(super) generation: u64,
    pub(super) ordinal: Option<u64>,
    pub(super) kind: Kind,
    pub(super) body: Option<LocalBody>,
    pub(super) existing: Option<Existing>,
    pub(super) bytes: u64,
    pub(super) incoming: u64,
    pub(super) live: bool,
    pub(super) content: u64,
    pub(super) structural: u64,
    draft_epoch: u64,
    draft: i128,
    retiring: bool,
    measured: u64,
    pub(super) depth: u32,
}
enum Binding {
    Edges(u32, Option<EntityId>, Value),
    Callable(u32, ArenaHandle),
}
/// The vector is a reusable slab. A free slot always receives a new generation;
/// its monotonic source ordinal is independent of physical reuse.
pub(super) struct LocalStore {
    pub(super) id: u64,
    pub(super) slots: Vec<ObjectSlot>,
    free: Vec<u32>,
    existing: BTreeMap<(u32, u32), u32>,
    entities: BTreeMap<EntityId, u32>,
    bindings: Vec<Binding>,
    table_inputs: TableInputs,
    touched: Vec<u32>,
    zero: Vec<u32>,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) release_visits: u64,
    #[cfg(any(test, feature = "perf-witness"))]
    binding_depth: u64,
    #[cfg(any(test, feature = "perf-witness"))]
    pub(super) binding_peak: u64,
    epoch: u64,
    measure_epoch: u64,
    measure_pending: Vec<LeaseValue>,
    measure_pages: Vec<(PageValue, bool)>,
    pub(super) ordinal: u64,
    pub(super) version: u64,
    pub(super) frozen: bool,
    pub(super) delta: i128,
    pub(super) peak: i128,
    /// The slots an adoption added, in batch order, until its phase is
    /// accepted: no other phase is accepted while one stands.
    pub(super) adopted: Option<Vec<u32>>,
}
fn exhausted() -> HeapFault {
    HeapFault::Exhausted {
        resource: HostResource::StagingBytes,
    }
}
pub(super) fn capacity<T>(values: &mut Vec<T>, additional: usize) -> Result<(), HeapFault> {
    values.try_reserve(additional).map_err(|_| exhausted())
}
fn count(value: usize) -> Result<u64, HeapFault> {
    u64::try_from(value).map_err(|_| invalid("lease length overflow"))
}
fn plus(a: u64, b: u64) -> Result<u64, HeapFault> {
    a.checked_add(b)
        .ok_or_else(|| invalid("lease holding overflow"))
}
impl LocalStore {
    pub(super) fn new() -> Result<Self, HeapFault> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| invalid("lease identity overflow"))?;
        Ok(Self {
            id,
            slots: Vec::new(),
            free: Vec::new(),
            existing: BTreeMap::new(),
            entities: BTreeMap::new(),
            bindings: Vec::new(),
            table_inputs: TableInputs::default(),
            touched: Vec::new(),
            zero: Vec::new(),
            #[cfg(any(test, feature = "perf-witness"))]
            release_visits: 0,
            #[cfg(any(test, feature = "perf-witness"))]
            binding_depth: 0,
            #[cfg(any(test, feature = "perf-witness"))]
            binding_peak: 0,
            epoch: 0,
            measure_epoch: 0,
            measure_pending: Vec::new(),
            measure_pages: Vec::new(),
            ordinal: 0,
            version: 0,
            frozen: false,
            delta: 0,
            peak: 0,
            adopted: None,
        })
    }
    pub(super) fn object(&self, slot: u32) -> LeaseObject {
        LeaseObject {
            lease: self.id,
            slot,
            generation: self.slots[slot as usize].generation,
        }
    }
    pub(super) fn index(&self, object: LeaseObject) -> Result<usize, HeapFault> {
        if object.lease != self.id
            || !self
                .slots
                .get(object.slot as usize)
                .is_some_and(|slot| slot.live && slot.generation == object.generation)
        {
            return Err(invalid("foreign, released or stale lease object"));
        }
        Ok(object.slot as usize)
    }
    fn value_index(&self, value: LeaseValue) -> Result<Option<usize>, HeapFault> {
        match value.repr {
            Repr::Scalar(_) => Ok(None),
            Repr::Object(object, _) => self.index(object).map(Some),
        }
    }
    fn begin(&mut self) -> Result<(), HeapFault> {
        if self.frozen {
            return Err(invalid("lease graph is frozen"));
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| invalid("lease phase overflow"))?;
        self.touched.clear();
        self.zero.clear();
        self.table_inputs.index.clear();
        self.table_inputs.rows.clear();
        #[cfg(any(test, feature = "perf-witness"))]
        {
            self.release_visits = 0;
        }
        Ok(())
    }
    fn change(&mut self, object: LeaseObject, delta: i128) -> Result<(), HeapFault> {
        let index = self.index(object)?;
        let slot = &mut self.slots[index];
        if slot.draft_epoch != self.epoch {
            capacity(&mut self.touched, 1)?;
            slot.draft_epoch = self.epoch;
            slot.draft = i128::from(slot.incoming);
            slot.retiring = false;
            self.touched.push(object.slot);
        }
        slot.draft = slot
            .draft
            .checked_add(delta)
            .ok_or_else(|| invalid("lease multiplicity overflow"))?;
        if slot.draft == 0 && !slot.retiring {
            capacity(&mut self.zero, 1)?;
            self.zero.push(object.slot);
        }
        Ok(())
    }
    fn empty_slot(&self) -> Result<LeaseObject, HeapFault> {
        let slot = self
            .free
            .last()
            .copied()
            .unwrap_or(u32::try_from(self.slots.len()).map_err(|_| exhausted())?);
        let generation = match self.slots.get(slot as usize) {
            Some(old) => old
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("lease generation overflow"))?,
            None => 1,
        };
        Ok(LeaseObject {
            lease: self.id,
            slot,
            generation,
        })
    }
}

mod accept;
mod adopt;
pub use adopt::NativeBatch;
mod allocation;
mod bindings;
mod overlay;
mod phase;
mod view;

impl LocalBody {
    fn clear(&mut self) {
        match self {
            Self::Values(values) => values.clear(),
            Self::Closure(closure) => closure.captures.clear(),
            Self::Table(table) => {
                table.entries.clear();
                table.index.clear();
                table.next = 0;
            }
            Self::Text(text) => text.clear(),
            Self::Bytes(bytes) => bytes.clear(),
            Self::Leaf(_) => {}
            Self::Cell(value) => {
                *value = LeaseValue {
                    repr: Repr::Scalar(Value::Absent),
                }
            }
        }
    }
}
pub(super) fn local_native(error: NativeError) -> LeaseFault {
    match error {
        NativeError::Retry(_) => LeaseFault::BoundaryRequired,
        error => LeaseFault::Native(error),
    }
}

impl ObjectSlot {
    pub(super) fn existing(&self) -> &Existing {
        let Some(existing) = &self.existing else {
            unreachable!("bound entry slot")
        };
        existing
    }
    pub(super) fn existing_mut(&mut self) -> &mut Existing {
        let Some(existing) = &mut self.existing else {
            unreachable!("bound entry slot")
        };
        existing
    }
}
