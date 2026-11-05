//! Ephemeral region ISA. Source and canonical definitions seal every table.
#![allow(dead_code, reason = "region execution is integrated by lane C")]

use super::room::region::calls::region_isa::CallPlan;
use super::room::{Event, Phase, Point, Refused, Source};
use super::{CodeId, Executable, LibId, StmtId};
use lash_kernel_doc::{self as doc, Value};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{LeaseValue, Slot};
use lash_kernel_value::{FunctionId, sites::Site};
use std::{marker::PhantomData, sync::Arc};

mod departures;
mod heap;
#[cfg(test)]
mod laws;
mod lower;
pub(crate) use heap::HeapKind;
mod recovery;
mod resources;
mod specialize;
mod stages;
mod verify;
use super::room::charges::region_isa::CanonicalClose;
pub(crate) use super::room::charges::region_isa::LoopInfo;
pub(crate) use recovery::{GraphRecovery, RecoveryMap};
pub(crate) use resources::{IndexedAmount, ResourceInput};

#[derive(Clone)]
pub(crate) struct RegionDraft {
    executable: u64,
    code: CodeId,
    source: Arc<Source>,
    entry: Pc,
    ops: Box<[RegionOp]>,
    generic_ops: Box<[RegionOp]>,
    steps: Box<[Step]>,
    generic_steps: Box<[Step]>,
    recovery: Box<[RecoveryMap]>,
    roles: Box<[(Slot, Reg<LeaseValue>)]>,
    registers: Box<[RegisterKind]>,
    generic_registers: Box<[RegisterKind]>,
    packing: Box<[Option<CheckedKernel>]>,
    calls: Box<[CallSite]>,
    call_sources: Box<[(StmtId, u32)]>,
    loops: Box<[LoopInfo]>,
    departures: Box<[departures::Departure]>,
}

pub(crate) struct RegionCode {
    draft: Arc<RegionDraft>,
    calls: Arc<[CallPlan]>,
    generic: bool,
}

pub(crate) struct Reg<T> {
    index: u32,
    kind: PhantomData<fn() -> T>,
}
impl<T> std::fmt::Debug for Reg<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Reg").field(&self.index).finish()
    }
}
impl<T> Copy for Reg<T> {}
impl<T> Clone for Reg<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> PartialEq for Reg<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}
impl<T> Eq for Reg<T> {}
impl<T> Reg<T> {
    fn new(index: usize) -> Result<Self, Refused> {
        Ok(Self {
            index: u32::try_from(index).map_err(|_| Refused::Facts)?,
            kind: PhantomData,
        })
    }
    pub(crate) fn index(&self) -> u32 {
        self.index
    }
    pub(crate) fn offset(self) -> usize {
        self.index as usize
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pc(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrefixId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallId(u32);
impl Pc {
    pub(crate) fn offset(self) -> usize {
        self.0 as usize
    }
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}
impl PrefixId {
    pub(crate) fn offset(self) -> usize {
        self.0 as usize
    }
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}
impl CallId {
    #[cfg(test)]
    pub(crate) fn fixture(index: u32) -> Self {
        Self(index)
    }
    pub(crate) fn index(self) -> u32 {
        self.0
    }
    pub(crate) fn offset(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisterKind {
    Value,
    Int,
    Float,
    Bool,
}

/// The installed native's own integer kernel and signature policy. There is
/// no caller-supplied answer and no successful operation is executed twice.
#[derive(Clone)]
pub(crate) struct IntKernel {
    function: FunctionId,
    lib: LibId,
    op: doc::scalar::ScalarOp,
    units: [u64; 16],
    depth: u32,
}
impl IntKernel {
    pub(crate) fn function(&self) -> FunctionId {
        self.function
    }
    pub(crate) fn operation(&self) -> doc::scalar::ScalarOp {
        self.op
    }
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }
    #[inline]
    pub(crate) fn apply(&self, lhs: i64, rhs: i64) -> Option<i64> {
        doc::scalar::int_number(self.op, lhs, rhs)
    }
    pub(crate) fn charge(&self, lhs: i64, rhs: i64, result: i64) -> u64 {
        let mask =
            usize::from(lhs != 0) | (usize::from(rhs != 0) << 1) | (usize::from(result != 0) << 3);
        self.units[mask]
    }
}

#[derive(Clone)]
pub(crate) struct CheckedKernel {
    kind: CheckedKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckedKind {
    Phase,
    LoopControl {
        stage: LoopStage,
        iterable: Option<Reg<LeaseValue>>,
    },
    Local {
        slot: Slot,
    },
    Native {
        function: FunctionId,
        lib: LibId,
    },
    FloatBinary {
        function: FunctionId,
        lib: LibId,
        operation: doc::scalar::ScalarOp,
        lhs: Reg<f64>,
        rhs: Reg<f64>,
        units: u64,
    },
    UnpackFloats {
        inputs: [Reg<LeaseValue>; 2],
        outputs: [Reg<f64>; 2],
    },
    PackFloat {
        input: Reg<f64>,
    },
    IntTruth {
        function: FunctionId,
        lib: LibId,
        operation: doc::scalar::ScalarOp,
        lhs: Reg<i64>,
        rhs: Reg<i64>,
        units: [u64; 16],
    },
    Truth {
        input: Reg<LeaseValue>,
        output: Reg<bool>,
    },
    Cursor {
        at: Site,
        exhausted: Pc,
    },
    LoopItem {
        at: Site,
    },
    UnpackPair {
        inputs: [Reg<LeaseValue>; 2],
        outputs: [Reg<i64>; 2],
    },
    PackBool {
        input: Reg<bool>,
    },
    UnpackInt {
        input: Reg<LeaseValue>,
        output: Reg<i64>,
    },
    PackInt {
        input: Reg<i64>,
    },
}
impl CheckedKernel {
    pub(crate) fn kind(&self) -> &CheckedKind {
        &self.kind
    }
    #[inline]
    pub(crate) fn float_number(&self, lhs: f64, rhs: f64) -> Option<f64> {
        let CheckedKind::FloatBinary { operation, .. } = self.kind else {
            return None;
        };
        doc::scalar::float_number(operation, lhs, rhs).map(|number| doc::Float::new(number).get())
    }
    #[inline]
    pub(crate) fn float_truth(&self, lhs: f64, rhs: f64) -> Option<bool> {
        let CheckedKind::FloatBinary { operation, .. } = self.kind else {
            return None;
        };
        doc::scalar::float_truth(operation, lhs, rhs)
    }
}

#[derive(Clone)]
pub(crate) struct HeapKernel {
    source: crate::compile::room::Point,
    phases: Box<[HeapPhase]>,
    kind: HeapKind,
}
#[derive(Clone)]
pub(crate) struct HeapPhase {
    pub(crate) source: Point,
    pub(crate) kind: HeapPhaseKind,
    pub(crate) peak: IndexedAmount,
    pub(crate) delta: IndexedAmount,
    pub(crate) reached: PrefixId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeapPhaseKind {
    Allocate,
    Member,
    Grow,
    Hold,
    Release,
}

#[derive(Clone)]
pub(crate) struct ReadSpec {
    at: Point,
    selector: Selector,
    retained: Option<Slot>,
    kind: RegisterKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selector {
    Field(Arc<str>),
    Index(Reg<LeaseValue>),
}
impl ReadSpec {
    pub(crate) fn point(&self) -> &Point {
        &self.at
    }
    pub(crate) fn selector(&self) -> &Selector {
        &self.selector
    }
    pub(crate) fn result_kind(&self) -> RegisterKind {
        self.kind
    }
    pub(crate) fn retained_owner(&self) -> Option<Slot> {
        self.retained
    }
}

/// A boundary is one canonical operation, never an evaluator for a subtree.
#[derive(Clone)]
pub(crate) struct BoundarySpec {
    at: Point,
    origin: Option<KirOrigin>,
    next: Option<Pc>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KirOrigin {
    pub(crate) statement: StmtId,
    pub(crate) pc: u32,
    pub(crate) op: Op,
}
impl BoundarySpec {
    pub(crate) fn point(&self) -> &Point {
        &self.at
    }
    pub(crate) fn origin(&self) -> Option<KirOrigin> {
        self.origin
    }
    pub(crate) fn next(&self) -> Option<Pc> {
        self.next
    }
}

#[derive(Clone)]
pub(crate) enum RegionOp {
    Constant {
        dst: Reg<LeaseValue>,
        value: Value,
        next: Pc,
    },
    Move {
        dst: Reg<LeaseValue>,
        src: Reg<LeaseValue>,
        next: Pc,
    },
    IntBinary {
        kernel: IntKernel,
        lhs: Reg<i64>,
        rhs: Reg<i64>,
        dst: Reg<i64>,
        next: Pc,
    },
    Read {
        spec: ReadSpec,
        base: Reg<LeaseValue>,
        dst: Reg<LeaseValue>,
        next: Pc,
    },
    Checked {
        kernel: CheckedKernel,
        args: Box<[Reg<LeaseValue>]>,
        dst: Reg<LeaseValue>,
        next: Pc,
    },
    Heap {
        kernel: HeapKernel,
        args: Box<[Reg<LeaseValue>]>,
        dst: Option<Reg<LeaseValue>>,
        next: Pc,
    },
    Branch {
        test: Reg<bool>,
        yes: Pc,
        no: Pc,
    },
    Call {
        site: CallId,
        next: Pc,
    },
    Return {
        value: Reg<LeaseValue>,
    },
    Boundary {
        spec: BoundarySpec,
    },
    Exit {
        prefix: PrefixId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopPhase {
    Enter,
    Test,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoopStage {
    pub(crate) info: u32,
    pub(crate) phase: LoopPhase,
}

/// Every semantic event has a before and an after map. Pending results are
/// retained separately until their charge/resource phase completes.
#[derive(Clone)]
pub(crate) struct Step {
    pub(crate) at: Point,
    pub(crate) event: Event,
    pub(crate) before: PrefixId,
    pub(crate) after: PrefixId,
    pub(crate) pending: PrefixId,
    pub(crate) peak: IndexedAmount,
    pub(crate) delta: IndexedAmount,
    pub(crate) charges: Box<[CanonicalClose]>,
    pub(crate) depth: u32,
    pub(crate) scalar_resources: bool,
    pub(crate) reachable: bool,
    pub(crate) stage: Option<LoopStage>,
    pub(crate) scratch: Box<[IndexedAmount]>,
    pub(crate) guard: Option<CheckedKernel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallSite {
    pub(crate) at: Point,
    pub(crate) origin: KirOrigin,
    pub(crate) arguments: Box<[Reg<LeaseValue>]>,
    pub(crate) result: Reg<LeaseValue>,
}

pub(crate) fn compile_region(exe: &Executable, code: CodeId) -> Result<RegionDraft, Refused> {
    lower::compile(exe, code)
}
pub(crate) fn verify_region(
    exe: &Executable,
    code: CodeId,
    draft: RegionDraft,
    calls: &[CallPlan],
) -> Result<Arc<RegionCode>, Refused> {
    verify::verify(exe, code, &draft, calls)?;
    Ok(Arc::new(RegionCode {
        draft: Arc::new(draft),
        calls: calls.to_vec().into(),
        generic: false,
    }))
}

impl RegionDraft {
    pub(crate) fn ops(&self) -> &[RegionOp] {
        &self.ops
    }
    pub(crate) fn call_sources(&self) -> &[(StmtId, u32)] {
        &self.call_sources
    }
    pub(crate) fn has_calls(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, RegionOp::Call { .. }))
    }
    pub(crate) fn call_sites(&self) -> &[CallSite] {
        &self.calls
    }
    pub(crate) fn code(&self) -> CodeId {
        self.code
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
}
impl RegionCode {
    pub(crate) fn as_generic(&self) -> Arc<Self> {
        Arc::new(Self {
            draft: Arc::clone(&self.draft),
            calls: Arc::clone(&self.calls),
            generic: true,
        })
    }
    pub(crate) fn packing(&self, reg: Reg<LeaseValue>) -> Option<&CheckedKernel> {
        if self.generic {
            None
        } else {
            self.draft.packing.get(reg.offset())?.as_ref()
        }
    }
    pub(crate) fn entry(&self) -> Pc {
        self.draft.entry
    }
    pub(crate) fn op(&self, pc: Pc) -> Option<&RegionOp> {
        if self.generic {
            self.draft.generic_ops.get(pc.offset())
        } else {
            self.draft.ops.get(pc.offset())
        }
    }
    /// Return is followed by its authenticated scope and defining-frame Leaves.
    pub(crate) fn return_successor(&self, pc: Pc) -> Option<Pc> {
        departures::successor(&self.draft, pc)
    }
    /// Select a departure-specific Leave when reopening a carried return.
    pub(crate) fn departure_at(&self, origin: &Point, at: &Point) -> Option<Pc> {
        let route = self
            .draft
            .departures
            .iter()
            .find(|route| self.draft.steps[route.origin.offset()].at == *origin)?;
        route
            .leaves
            .iter()
            .copied()
            .find(|pc| self.draft.steps[pc.offset()].at == *at)
    }
    pub(crate) fn recovery(&self, prefix: PrefixId) -> Option<&RecoveryMap> {
        self.draft.recovery.get(prefix.offset())
    }
    pub(crate) fn call(&self, site: CallId) -> Option<&CallPlan> {
        self.calls.get(site.offset())
    }
    pub(crate) fn step(&self, pc: Pc) -> Option<&Step> {
        if self.generic {
            self.draft.generic_steps.get(pc.offset())
        } else {
            self.draft.steps.get(pc.offset())
        }
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.draft.source
    }
    pub(crate) fn code(&self) -> CodeId {
        self.draft.code
    }
    pub(crate) fn executable(&self) -> u64 {
        self.draft.executable
    }
    pub(crate) fn registers(&self) -> &[RegisterKind] {
        if self.generic {
            &self.draft.generic_registers
        } else {
            &self.draft.registers
        }
    }
    pub(crate) fn loop_info(&self, index: u32) -> Option<&LoopInfo> {
        self.draft.loops.get(index as usize)
    }
    pub(crate) fn stage(&self, stage: LoopStage) -> Option<Pc> {
        self.draft
            .steps
            .iter()
            .position(|step| step.reachable && step.stage == Some(stage))
            .and_then(|index| u32::try_from(index).ok())
            .map(Pc)
    }
    pub(crate) fn roles(&self) -> &[(Slot, Reg<LeaseValue>)] {
        &self.draft.roles
    }
    pub(crate) fn at(&self, point: &Point) -> Option<Pc> {
        self.draft
            .steps
            .iter()
            .position(|step| step.reachable && step.at == *point)
            .and_then(|index| u32::try_from(index).ok())
            .map(Pc)
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod fixtures {
    use super::*;
    pub(crate) fn draft(exe: &Executable, code: CodeId) -> Result<RegionDraft, Refused> {
        compile_region(exe, code)
    }
    fn raw(exe: &Executable, code: CodeId) -> Result<RegionDraft, Refused> {
        if code.0 as usize >= exe.code_count() {
            return Err(Refused::Source);
        }
        Ok(RegionDraft {
            executable: exe.local(),
            code,
            source: Arc::clone(&exe.code(code).source_room),
            entry: Pc(0),
            ops: Box::new([]),
            generic_ops: Box::new([]),
            steps: Box::new([]),
            generic_steps: Box::new([]),
            recovery: Box::new([]),
            roles: Box::new([]),
            registers: Box::new([]),
            generic_registers: Box::new([]),
            packing: Box::new([]),
            calls: Box::new([]),
            call_sources: Box::new([]),
            loops: Box::new([]),
            departures: Box::new([]),
        })
    }
    /// An untrusted source call table; never a verified code constructor.
    pub(crate) fn calls(
        exe: &Executable,
        code: CodeId,
        sites: &[(StmtId, u32)],
    ) -> Result<RegionDraft, Refused> {
        let mut draft = raw(exe, code)?;
        let end = u32::try_from(sites.len()).map_err(|_| Refused::Path)?;
        draft.ops = (0..end)
            .map(|index| RegionOp::Call {
                site: CallId(index),
                next: Pc(index + 1),
            })
            .collect();
        draft.call_sources = sites.into();
        Ok(draft)
    }
    pub(crate) fn exit(
        exe: &Executable,
        code: CodeId,
        continuation: Point,
    ) -> Result<RegionDraft, Refused> {
        let mut draft = raw(exe, code)?;
        draft.ops = Box::new([RegionOp::Exit {
            prefix: PrefixId(0),
        }]);
        draft.recovery = Box::new([RecoveryMap {
            roles: Box::new([]),
            operands: Box::new([]),
            pending: None,
            graph: GraphRecovery::fixture(&continuation),
            continuation,
        }]);
        Ok(draft)
    }
    pub(crate) fn heap(
        exe: &Executable,
        code: CodeId,
        continuation: Point,
    ) -> Result<RegionDraft, Refused> {
        let mut draft = exit(exe, code, continuation.clone())?;
        draft.ops = Box::new([RegionOp::Heap {
            kernel: HeapKernel {
                source: continuation,
                phases: Box::new([]),
                kind: HeapKind::List,
            },
            args: Box::new([]),
            dst: None,
            next: Pc(0),
        }]);
        Ok(draft)
    }
}
