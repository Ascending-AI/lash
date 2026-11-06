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

mod call_paths;
mod departures;
mod heap;
mod joins;
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
pub(crate) use recovery::{GraphRecovery, RecoveryMap, Role};
pub(crate) use resources::{IndexedAmount, ResourceInput};

fn resource_pair(lib: &super::Lib, charged: bool) -> Option<doc::scalar::Pair> {
    if lib.specialization.native.immediate_resources {
        super::numeric_pair(lib, charged)
    } else {
        None
    }
}

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
    roles: Box<[(Role, Reg<LeaseValue>)]>,
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
    pub(crate) fn lib(&self) -> LibId {
        self.lib
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
    /// A binding's read; a cell binding (a capture, or a binding a closure
    /// shares) reads its cell's value.
    Local {
        slot: Role,
        cell: bool,
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
    moves: Box<[Reg<LeaseValue>]>,
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
    AllocateMove,
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
    action: Option<ActionBoundary>,
    origin: Option<KirOrigin>,
    next: Option<Pc>,
}
/// A delivering action's Enter. The step evaluates its atoms and stops after
/// its own acceptance; the machine performs the action its origin names, and
/// the frame resumes at the successor Commit with the result as the task's
/// Incoming. Without one, the boundary has no lowering and the machine faults.
#[derive(Clone)]
pub(crate) struct ActionBoundary {
    /// The statement action's atoms, evaluated into their own slots.
    pub(crate) atoms: Box<[AtomLoad]>,
    /// The action's arguments, in order: its atoms, or the operands of a
    /// nested action, live from this step to its Commit.
    pub(crate) arguments: Box<[Reg<LeaseValue>]>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AtomLoad {
    pub(crate) dst: Reg<LeaseValue>,
    pub(crate) src: AtomSource,
}
#[derive(Clone, Debug)]
pub(crate) enum AtomSource {
    /// A visible name's role.
    Role(Reg<LeaseValue>),
    Constant(Value),
    /// A text literal too long to stand inline, allocated when it loads.
    Text(Arc<str>),
}
impl PartialEq for AtomSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Role(a), Self::Role(b)) => a == b,
            (Self::Constant(a), Self::Constant(b)) => a.same(b),
            (Self::Text(a), Self::Text(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for AtomSource {}
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
    pub(crate) fn action(&self) -> Option<&ActionBoundary> {
        self.action.as_ref()
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
    /// An uncaught Throw's Commit: no try of the frame encloses it. The
    /// step moves the operand into the frame's departure and is accepted;
    /// the region then stops, and the machine carries the raise out of the
    /// frame from that role.
    Raise {
        dst: Reg<LeaseValue>,
        src: Reg<LeaseValue>,
    },
    /// An uncaught ReturnVia's Commit: no try of the frame encloses it.
    /// The step is accepted with its operand still held at its role, as an
    /// action's atoms are; the region then stops, and the machine returns
    /// the value out of the frame through the statement's policy.
    ReturnVia {
        value: Reg<LeaseValue>,
    },
    Boundary {
        spec: BoundarySpec,
    },
    /// A statement that hands its operand to the machine: the step is
    /// accepted and billed in the region, which then stops with the value
    /// as the task's Incoming for the machine to print, finish or fail with.
    /// A finish or fail departs the program: its value moves into the
    /// departure role and the region has no successor.
    Effect {
        kind: EffectKind,
        value: Reg<LeaseValue>,
        departure: Option<Reg<LeaseValue>>,
        next: Option<Pc>,
    },
    Exit {
        prefix: PrefixId,
    },
}

/// What the machine does with an [`RegionOp::Effect`]'s value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EffectKind {
    Print,
    Finish,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoopDeparture {
    Origin(Point),
    End { exits: Option<u32> },
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
    /// A statement call's atoms, loaded at its Enter and held by the caller
    /// until its Commit; a nested call's arguments are its operands.
    pub(crate) atoms: Box<[AtomLoad]>,
    pub(crate) arguments: Box<[Reg<LeaseValue>]>,
    pub(crate) result: Reg<LeaseValue>,
}

pub(crate) fn compile_region(exe: &Executable, code: CodeId) -> Result<RegionDraft, Refused> {
    lower::compile(exe, code)
}
impl Executable {
    /// `code`'s verified region program: lowered, its calls planned and the
    /// whole verified once, then shared by every lease of this executable.
    pub(crate) fn region(&self, code: CodeId) -> Result<Arc<RegionCode>, Refused> {
        if let Some(known) = self.regions.lock().map_err(|_| Refused::Source)?.get(&code) {
            return known.clone();
        }
        let verified = compile_region(self, code).and_then(|draft| {
            let calls =
                crate::compile::room::region::calls::region_isa::compile_calls(self, &draft)?;
            verify_region(self, code, draft, &calls)
        });
        self.regions
            .lock()
            .map_err(|_| Refused::Source)?
            .insert(code, verified.clone());
        verified
    }

    /// Records that a run reached `target` through `caller`'s dynamic call
    /// `site`; false when it was already recorded.
    pub(crate) fn reach_dynamic_target(&self, caller: CodeId, site: u32, target: CodeId) -> bool {
        self.dynamic_targets
            .lock()
            .is_ok_and(|mut reached| reached.insert((caller, site, target)))
    }

    /// The targets runs have reached through `caller`'s dynamic call `site`.
    pub(crate) fn dynamic_targets(&self, caller: CodeId, site: u32) -> Vec<CodeId> {
        self.dynamic_targets.lock().map_or_else(
            |_| Vec::new(),
            |reached| {
                reached
                    .range((caller, site, CodeId(0))..=(caller, site, CodeId(u32::MAX)))
                    .map(|&(_, _, target)| target)
                    .collect()
            },
        )
    }
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
    /// The slots of the delivering action at `pc`'s arguments, in order,
    /// and its Commit's pc.
    pub(crate) fn action_arguments(&self, pc: Pc) -> Option<(Box<[Slot]>, Pc)> {
        let RegionOp::Boundary { spec } = self.op(pc)? else {
            return None;
        };
        let slots = spec
            .action()?
            .arguments
            .iter()
            .map(|reg| {
                self.draft
                    .roles
                    .iter()
                    .find_map(|(role, actual)| (actual == reg).then_some(role.slot()))
                    .flatten()
            })
            .collect::<Option<_>>()?;
        Some((slots, spec.next()?))
    }
    /// The library a native call at `pc` calls and the source slots of its
    /// arguments, for the machine to answer it when its checked kernel
    /// cannot in place.
    /// The native a step calls and the value registers of its operands: a
    /// native call, or a scalar kernel specialized from one, whose guard's
    /// inputs are the operands it unpacked.
    pub(crate) fn native_operands(
        &self,
        pc: Pc,
    ) -> Option<(LibId, FunctionId, &[Reg<LeaseValue>])> {
        match self.op(pc)? {
            RegionOp::Checked { kernel, args, .. } => match kernel.kind() {
                CheckedKind::Native { lib, function }
                | CheckedKind::IntTruth { lib, function, .. }
                | CheckedKind::FloatBinary { lib, function, .. } => Some((*lib, *function, args)),
                _ => None,
            },
            RegionOp::IntBinary { kernel, .. } => match self.step(pc)?.guard.as_ref()?.kind() {
                CheckedKind::UnpackPair { inputs, .. } => {
                    Some((kernel.lib(), kernel.function(), inputs))
                }
                _ => None,
            },
            _ => None,
        }
    }
    pub(crate) fn native_arguments(&self, pc: Pc) -> Option<(LibId, Box<[Slot]>)> {
        let (lib, _, args) = self.native_operands(pc)?;
        let slots = args
            .iter()
            .map(|reg| {
                self.draft
                    .roles
                    .iter()
                    .find_map(|(role, actual)| (actual == reg).then_some(role.slot()))
                    .flatten()
            })
            .collect::<Option<_>>()?;
        Some((lib, slots))
    }
    /// The source slot holding the value of the effect at `pc`, for the
    /// machine to copy out before the region takes its step.
    pub(crate) fn effect_argument(&self, pc: Pc) -> Option<Slot> {
        let RegionOp::Effect { value, .. } = self.op(pc)? else {
            return None;
        };
        self.draft
            .roles
            .iter()
            .find_map(|(role, actual)| (actual == value).then_some(role.slot()))
            .flatten()
    }
    /// The source slot holding the value of the ReturnVia at `pc`, which
    /// the machine returns through the policy once the step is accepted.
    pub(crate) fn return_via_argument(&self, pc: Pc) -> Option<Slot> {
        let RegionOp::ReturnVia { value } = self.op(pc)? else {
            return None;
        };
        self.draft
            .roles
            .iter()
            .find_map(|(role, actual)| (actual == value).then_some(role.slot()))
            .flatten()
    }
    /// The loop of the statement at `site`.
    pub(crate) fn loop_at(&self, site: &Site) -> Option<u32> {
        let index = self
            .draft
            .loops
            .iter()
            .position(|info| info.site == *site)?;
        u32::try_from(index).ok()
    }
    /// The source slot holding what the plain `for` of loop `index` walks:
    /// the iterable its Enter stage takes, held at the loop's cursor role.
    pub(crate) fn loop_cursor_slot(&self, index: u32) -> Option<Slot> {
        let pc = self.stage(LoopStage {
            info: index,
            phase: LoopPhase::Enter,
        })?;
        let RegionOp::Checked { kernel, .. } = self.op(pc)? else {
            return None;
        };
        let CheckedKind::LoopControl {
            iterable: Some(iterable),
            ..
        } = kernel.kind()
        else {
            return None;
        };
        self.draft
            .roles
            .iter()
            .find_map(|(role, actual)| (actual == iterable).then_some(role.slot()))
            .flatten()
    }
    /// The atoms a statement call loads at its Enter.
    pub(crate) fn call_atoms(&self, site: CallId) -> Option<&[AtomLoad]> {
        Some(&self.draft.calls.get(site.offset())?.atoms)
    }
    /// Register holding the completed callee result until caller Restore.
    pub(crate) fn call_incoming(&self, site: CallId) -> Option<Reg<LeaseValue>> {
        self.draft.calls.get(site.offset())?;
        Reg::new(self.draft.roles.len()).ok()
    }
    /// Return is followed by its authenticated scope and defining-frame Leaves.
    pub(crate) fn return_successor(&self, pc: Pc) -> Option<Pc> {
        departures::successor(&self.draft, pc)
    }
    /// A Break/Continue route row: its origin, or its final Catch together
    /// with the loop a Break leaves. Return routes are not loop departures.
    pub(crate) fn loop_departure(&self, pc: Pc) -> Option<LoopDeparture> {
        let route = self
            .draft
            .departures
            .iter()
            .find(|route| route.origin == pc || route.leaves.last() == Some(&pc))?;
        let origin = &self.draft.steps[route.origin.offset()].at;
        // A raising event's route is carried as a Throw's.
        if route.origin.offset() >= self.draft.source.room.events.len() {
            return Some(if route.origin == pc {
                LoopDeparture::Origin(origin.clone())
            } else {
                LoopDeparture::End { exits: None }
            });
        }
        let breaks = match lower::node_at(&self.draft.source, origin)? {
            doc::Node::Stmt(doc::Stmt::Break) => true,
            doc::Node::Stmt(doc::Stmt::Continue) => false,
            // A raise is carried like a loop departure; its value stays
            // in the departure role until its Catch takes it.
            doc::Node::Stmt(doc::Stmt::Throw { .. }) if route.origin == pc => {
                return Some(LoopDeparture::Origin(origin.clone()));
            }
            doc::Node::Stmt(doc::Stmt::Throw { .. }) => {
                return Some(LoopDeparture::End { exits: None });
            }
            _ => return None,
        };
        if route.origin == pc {
            return Some(LoopDeparture::Origin(origin.clone()));
        }
        let at = &self.draft.steps[pc.offset()].at;
        let index = self
            .draft
            .loops
            .iter()
            .position(|info| info.site == at.site)?;
        Some(LoopDeparture::End {
            exits: breaks.then_some(u32::try_from(index).ok()?),
        })
    }
    /// Whether a carried departure origin is a Break, Continue or Throw.
    pub(crate) fn loop_origin(&self, origin: &Point) -> bool {
        self.draft.departures.iter().any(|route| {
            self.draft.steps[route.origin.offset()].at == *origin
                && !matches!(
                    self.draft.ops[route.origin.offset()],
                    RegionOp::Return { .. }
                )
        })
    }
    /// Select a departure-specific Leave when reopening a carried return.
    pub(crate) fn departure_at(&self, origin: &Point, at: &Point) -> Option<Pc> {
        let route = self
            .draft
            .departures
            .iter()
            .find(|route| self.draft.steps[route.origin.offset()].at == *origin)?;
        std::iter::once(route.origin)
            .chain(route.leaves.iter().copied())
            .find(|pc| self.draft.steps[pc.offset()].at == *at)
    }
    /// Whether a raise of the event at `at` has a route of its own to the
    /// try of the frame that takes it.
    pub(crate) fn raise_route(&self, at: &Point) -> bool {
        self.raise_origins()
            .any(|origin| self.draft.steps[origin.offset()].at == *at)
    }
    /// The rows a raise re-enters the region at: one per raising event with
    /// a route, entered only from a continuation, never by a branch.
    pub(crate) fn raise_origins(&self) -> impl Iterator<Item = Pc> + '_ {
        let events = self.draft.source.room.events.len();
        self.draft
            .departures
            .iter()
            .map(|route| route.origin)
            .filter(move |origin| origin.offset() >= events)
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
    pub(crate) fn roles(&self) -> &[(Role, Reg<LeaseValue>)] {
        &self.draft.roles
    }
    fn prefix(step: &Step, progress: u32) -> Option<PrefixId> {
        match progress {
            0 => Some(step.before),
            1 => Some(step.pending),
            2 => Some(step.after),
            _ => None,
        }
    }
    /// The row a continuation stands at. A conditional Commit has one row
    /// per arm, and they differ in the arm operand they hold: the row is the
    /// one whose recovery holds every saved role.
    pub(crate) fn source_row(
        &self,
        point: &Point,
        progress: u32,
        departure: Option<&Point>,
        held: &[Slot],
    ) -> Option<Pc> {
        if let Some(pc) = departure.and_then(|origin| self.departure_at(origin, point)) {
            return Some(pc);
        }
        let rows: Vec<Pc> = (0..self.draft.steps.len())
            .filter(|index| self.ordinary(*index) && self.draft.steps[*index].at == *point)
            .filter_map(|index| u32::try_from(index).ok().map(Pc))
            .collect();
        if rows.len() < 2 {
            return rows.first().copied();
        }
        let mut matching = rows.into_iter().filter(|pc| {
            self.step(*pc)
                .and_then(|step| Self::prefix(step, progress))
                .and_then(|prefix| self.recovery(prefix))
                .is_some_and(|map| {
                    held.iter()
                        .all(|slot| map.roles.iter().any(|(actual, _)| actual == slot))
                })
        });
        let pc = matching.next()?;
        matching.next().is_none().then_some(pc)
    }
    /// Rebuild a recovery entry from its semantic phase and acceptance state.
    pub(crate) fn source_recovery(
        &self,
        point: &Point,
        progress: u32,
        departure: Option<&Point>,
        held: &[Slot],
    ) -> Option<PrefixId> {
        let pc = self.source_row(point, progress, departure, held)?;
        Self::prefix(self.step(pc)?, progress)
    }
    /// The row a continuation resumes at: the reached row itself before it
    /// is accepted, otherwise the reached row's successor at `unpaid`.
    pub(crate) fn source_next(
        &self,
        point: &Point,
        progress: u32,
        departure: Option<&Point>,
        held: &[Slot],
        unpaid: &Point,
    ) -> Option<Pc> {
        let reached = self.source_row(point, progress, departure, held)?;
        if progress < 2 {
            return Some(reached);
        }
        if let Some(pc) = departure.and_then(|origin| self.departure_at(origin, unpaid)) {
            return Some(pc);
        }
        let rows = self
            .draft
            .steps
            .iter()
            .enumerate()
            .filter(|(index, step)| self.ordinary(*index) && step.at == *unpaid)
            .count();
        if rows < 2 {
            return self.at(unpaid);
        }
        let mut next = recovery::successors(&self.draft, reached.offset())
            .into_iter()
            .filter(|pc| self.draft.steps[pc.offset()].at == *unpaid);
        let pc = next.next()?;
        next.next().is_none().then_some(pc)
    }
    pub(crate) fn source_progress(&self, prefix: PrefixId) -> Option<u32> {
        self.draft.steps.iter().find_map(|step| {
            if step.before == prefix {
                Some(0)
            } else if step.pending == prefix {
                Some(1)
            } else if step.after == prefix {
                Some(2)
            } else {
                None
            }
        })
    }
    /// The saved next phase must be an actual edge of the reached source event.
    pub(crate) fn source_successor(
        &self,
        point: &Point,
        progress: u32,
        departure: Option<&Point>,
        held: &[Slot],
        unpaid: &Point,
    ) -> bool {
        if let Some(origin) = departure
            && !self.draft.departures.iter().any(|route| {
                self.draft.steps[route.origin.offset()].at == *origin
                    && (origin == point
                        || route
                            .leaves
                            .iter()
                            .any(|pc| self.draft.steps[pc.offset()].at == *point))
            })
        {
            return false;
        }
        let Some(pc) = self.source_row(point, progress, departure, held) else {
            return false;
        };
        match progress {
            0 | 1 => point == unpaid,
            2 => recovery::successors(&self.draft, pc.offset())
                .iter()
                .any(|next| self.draft.steps[next.offset()].at == *unpaid),
            _ => false,
        }
    }
    /// A reachable row a continuation names by its point alone: a raise
    /// origin is named only through its departure.
    fn ordinary(&self, index: usize) -> bool {
        self.draft.steps[index].reachable
            && (index < self.draft.source.room.events.len()
                || !self.raise_origins().any(|origin| origin.offset() == index))
    }
    pub(crate) fn at(&self, point: &Point) -> Option<Pc> {
        (0..self.draft.steps.len())
            .position(|index| self.ordinary(index) && self.draft.steps[index].at == *point)
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
                moves: Box::new([]),
            },
            args: Box::new([]),
            dst: None,
            next: Pc(0),
        }]);
        Ok(draft)
    }
}
