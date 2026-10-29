//! Private register program for an admitted complete scalar/control body.
//! Source coordinates and event tables remain the authority at every exit.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_exec::ActIx;

use super::{ISLAND_SLOTS, SlotState, Slots};
use crate::compile::expr::{Close, Expr, Kind};
use crate::compile::islands::IslandPlan;
use crate::compile::{
    BlockId, CodeId, Entry, Executable, LibId, LibRun, ScalarTable, StmtId, Stream,
};

#[path = "program.rs"]
mod program;
pub(crate) use super::code::Op;
use program::Build;
pub(super) use program::{
    AccessShape, AccessSite, CallInputs, CollectionAppend, CollectionStep, Destination, Format,
    Merge, Operand, Placement, ReadSlot, Reduction, Update, Visit, WriteSite,
};
pub(crate) use program::{Instruction, Read, StatementEntry, StoreInputs};

/// Heap text construction and operations the source handler must answer.
#[derive(Clone, Copy)]
pub(crate) enum Deferred {
    Source(u32),
    Text {
        dst: u32,
        constant: lash_kernel_exec::ConstIx,
        miss: u32,
    },
}

/// A literal return's value and source-derived statement/close price.
pub(super) struct LiteralReturn {
    pub(super) value: Value,
    pub(super) units: u64,
    pub(super) statement: StmtId,
    pub(super) index: usize,
    pub(super) close: Option<Close>,
}

/// Semantic cleanup targets, indexed by source statement for cold recovery.
#[derive(Clone, Copy)]
pub(super) struct Cleanup {
    pub(super) id: StmtId,
    pub(super) finally: Option<usize>,
    pub(super) after: usize,
    /// One distinct pending-departure lifetime for this lexical try.
    pub(super) pending: usize,
}

pub(crate) struct Compiled {
    pub(super) literal: Option<LiteralReturn>,
    pub(super) layout: Layout,
    pub(super) blocks: Vec<Block>,
    pub(super) code: super::code::Code,
    pub(super) registers: usize,
    headers: Box<[(u32, usize)]>,
    actions: Box<[(u32, usize)]>,
    pub(super) cell_parameters: usize,
    pub(super) cleanups: bool,
    cleanup: Box<[Cleanup]>,
    pub(super) restarts: Box<[Restart]>,
    room: u32,
    observes: bool,
    #[allow(
        dead_code,
        reason = "phase-one source projection; consumed by ledger admission later"
    )]
    pub(crate) source_room: std::sync::Arc<crate::compile::room::Source>,
}

/// Private copies used only when a source region's charge close refuses.
#[derive(Clone, Copy)]
pub(crate) struct Restart {
    pub(super) statement: StmtId,
    pub(super) head: u32,
    pub(super) reg: u32,
    pub(super) saved: u32,
}

pub(super) struct Block {
    pub(super) id: BlockId,
    pub(super) declares: Box<[u32]>,
    /// Physical entries projected from this block's source statement order.
    pub(super) starts: Box<[usize]>,
    pub(super) end: usize,
    pub(super) after: usize,
}

/// A plan's frame layout, precomputed from its plan: how many arguments
/// its call takes, and the variable each argument and each captured value
/// is copied to as its frame is entered.
pub(super) struct Layout {
    pub(super) arity: usize,
    /// Its parameters are its first variables, in order: the arguments
    /// are copied as one run.
    leading: bool,
    params: Box<[u32]>,
    captures: Box<[u32]>,
    /// Its shared variables, the only ones a cell stands in.
    pub(super) shared: Box<[u32]>,
}

impl Layout {
    fn of(plan: &IslandPlan) -> Option<Self> {
        if plan.slots > ISLAND_SLOTS
            || plan
                .params
                .iter()
                .chain(plan.captures.iter())
                .any(|at| *at as usize >= plan.slots)
        {
            return None;
        }
        Some(Self {
            arity: plan.params.len(),
            leading: plan
                .params
                .iter()
                .enumerate()
                .all(|(index, at)| *at as usize == index),
            params: plan.params.clone(),
            captures: plan.captures.clone(),
            shared: (0..plan.slots)
                .filter(|at| plan.shared.get(*at).copied().unwrap_or(false))
                .map(|at| at as u32)
                .collect(),
        })
    }

    /// Lays out the fresh frame `slots`, every variable empty: `args`
    /// in its parameters, those past them absent, and the captured values
    /// from `captured` in its captured variables. `args` are at most its
    /// arity.
    #[inline(always)]
    pub(super) fn fill(
        &self,
        slots: &mut Slots,
        args: impl ExactSizeIterator<Item = Value>,
        captured: &[(Value, ArenaHandle)],
    ) {
        let given = args.len().min(self.arity);
        if self.leading {
            // Over `args`, whose length a call site fixes, so the copy
            // unrolls there rather than becoming a call of `memcpy`.
            for (slot, value) in slots[..given].iter_mut().zip(args) {
                *slot = SlotState::Value(value);
            }
            for slot in &mut slots[given..self.arity] {
                *slot = SlotState::Value(Value::Absent);
            }
        } else {
            let mut args = args;
            for at in &self.params {
                slots[*at as usize] = SlotState::Value(args.next().unwrap_or(Value::Absent));
            }
        }
        for (at, (_, cell)) in self.captures.iter().zip(captured) {
            slots[*at as usize] = SlotState::Cell(*cell);
        }
    }
}

/// The depth check an operator's call makes (`K-BND-002`).
#[derive(Clone, Copy)]
pub(crate) enum Depth {
    None,
    /// An operation's admission, then its template's frames beyond one.
    Operation(u32),
    /// A template's frames.
    Template(u32),
}

/// A compiled operator or helper call.
pub(crate) struct OperatorSite {
    pub(crate) checked: bool,
    pub(crate) lib: LibId,
    pub(super) close: Option<Close>,
    /// What its close charges.
    pub(super) fixed: u64,
    pub(super) pc: u32,
    pub(crate) reg: u32,
    pub(crate) counted: Option<usize>,
    pub(crate) act: Option<ActIx>,
    pub(crate) root: Option<StmtId>,
    /// A body-only helper exposes its frame to work observation.
    pub(super) body_when_observed: bool,
    pub(super) depth: Depth,
}

impl OperatorSite {
    /// Publication admits only scalar prepared entries. The immutable library
    /// keeps their implementation and price table; sites keep source roles.
    pub(super) fn entry<'e>(&self, exe: &'e Executable) -> &'e Entry {
        match &exe.lib(self.lib).specialization.prepared {
            Some((entry, _)) => entry,
            None => unreachable!("published scalar site has a canonical entry"),
        }
    }

    pub(super) fn table<'e>(&self, exe: &'e Executable) -> &'e ScalarTable {
        match exe
            .lib(self.lib)
            .specialization
            .prepared
            .as_ref()
            .and_then(|(_, fast)| fast.scalar())
        {
            Some(table) => table,
            None => unreachable!("published scalar site has a canonical table"),
        }
    }

    pub(super) fn pair(&self, exe: &Executable) -> Option<lash_kernel_doc::scalar::Pair> {
        exe.lib(self.lib).kernels(exe).pairs[usize::from(self.checked)]
    }

    pub(super) fn concat(&self, exe: &Executable) -> Option<crate::compile::TextCharge> {
        exe.lib(self.lib).kernels(exe).concat
    }
}

/// A source-certified member read and its formula's scalar prices.
#[derive(Clone, Copy)]
pub(crate) enum MemberAccess {
    Float(u64),
    Int { zero: u64, nonzero: u64 },
    Map(Option<crate::compile::LengthCharge>),
    Append { units: u64, values: u32 },
}

/// A prepared heap operation keeps the same semantic call coordinates.
pub(crate) struct HeapSite {
    pub(super) lib: LibId,
    pub(super) close: Option<Close>,
    pub(super) pc: u32,
    pub(super) reg: u32,
    pub(super) depth: Depth,
    pub(super) checked: bool,
    pub(super) counted: Option<usize>,
    pub(super) act: Option<ActIx>,
    pub(super) root: Option<StmtId>,
}

/// A compiled general native call.
pub(crate) struct NativeSite {
    pub(super) lib: LibId,
    pub(super) close: Option<Close>,
    pub(super) pc: u32,
    pub(super) reg: u32,
    /// The depth its prepared entry's template checks.
    pub(super) depth: Depth,
}

pub(crate) use crate::compile::Resident as Direct;

impl NativeSite {
    pub(super) fn direct(&self, exe: &Executable) -> Option<Direct> {
        exe.lib(self.lib).kernels(exe).resident
    }
}

/// A compiled test `same(x, ())` of a value against the empty tuple,
/// answered without building the tuple.
pub(crate) struct UnitSite {
    pub(super) lib: LibId,
    pub(super) close: Option<Close>,
    /// Where an exit before it resumes: where the tuple's construction
    /// resumes, the head of the region `close` closes when it has one.
    pub(super) miss: u32,
    pub(super) reg: u32,
}

/// A compiled island call.
pub(crate) struct CallSite {
    pub(super) checked: bool,
    pub(super) inputs: Option<Box<CallInputs>>,
    /// The source library call and root action, retained on materialization.
    pub(super) library: Option<LibId>,
    /// The call depth its library's prepared answer needs beyond the
    /// caller's, where its partial prepared paths may answer before
    /// entering the ordinary body ([`answer_depth`]).
    pub(super) answers: Option<u32>,
    pub(super) root: Option<StmtId>,
    pub(super) callee: Option<CodeId>,
    pub(super) close: Option<Close>,
    pub(super) pc: u32,
    pub(super) reg: u32,
    pub(super) act: Option<ActIx>,
    pub(super) occurrence: Option<usize>,
    pub(super) site: u32,
    /// The most the segment from its return charges; 0 where none follows.
    pub(super) after: u64,
}

/// A helper's certified member write with its source admission and formula.
pub(crate) struct MemberWrite {
    pub(super) element: Option<u64>,
    pub(super) guard: lash_kernel_state::arena::Key,
    pub(super) accepts: [bool; 2],
    pub(super) values: u32,
    pub(super) lookup: Option<lash_kernel_state::arena::Key>,
    pub(super) charge: MemberCharge,
    pub(super) before: u64,
    pub(super) occurrence: Option<usize>,
    pub(super) depth: u32,
}

pub(crate) enum MemberCharge {
    Known(u64),
    Text(crate::compile::LengthCharge),
}

/// Where library `lib`'s body call may answer immediately: an uncharged
/// body of its own parameters whose prepared entry is not statement
/// metered. The call depth its immediate paths need beyond the caller's.
pub(crate) fn answer_depth(exe: &Executable, lib: LibId) -> Option<u32> {
    let function = exe.lib(lib);
    let LibRun::Body(code) = function.run else {
        return None;
    };
    if function.shares_params || exe.code(code).charged {
        return None;
    }
    let (prepared, _) = function.specialization.prepared.as_ref()?;
    if matches!(prepared, Entry::Template { template, .. } if template.metered.is_some()) {
        return None;
    }
    Some(prepared.deepest(&|id| exe.lib(id), &|id| exe.library_lib(id)))
}

/// A whole typed scalar loop, compiled from its verified ordered facts.
pub(crate) struct RepeatSite {
    pub(super) run: super::code::repeat::Repeat,
    pub(super) iteration_max: u64,
}

impl Compiled {
    #[cfg(test)]
    pub(crate) fn verify_packets(
        &self,
        stream: &Stream,
    ) -> Result<(), crate::compile::islands::Refused> {
        self.code.verify(stream)
    }

    #[cfg(test)]
    pub(crate) fn corrupt_packet_close(&mut self) -> u32 {
        let close = self
            .code
            .sites
            .operator
            .iter_mut()
            .find_map(|site| site.site.close.as_mut())
            .expect("the verifier law has an operator close");
        close.total += 1;
        close.region
    }

    #[cfg(test)]
    pub(crate) fn corrupt_packet_role(&mut self, stream: &Stream) -> bool {
        self.code.corrupt_call_role(stream)
    }

    #[cfg(test)]
    pub(crate) fn verify_family_projections(
        &self,
        plan: &IslandPlan,
        exe: &Executable,
    ) -> Result<(), crate::compile::islands::Refused> {
        self.code.verify_families(plan, exe)
    }

    #[cfg(test)]
    pub(crate) fn corrupt_family_roles(&mut self) -> bool {
        if let Some(site) = self.code.sites.update.first_mut() {
            site.value.consumed = Box::default();
            return true;
        }
        if let Some(site) = self.code.sites.format.first_mut() {
            site.value.joined = Box::default();
            return true;
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn corrupt_repeat_input(&mut self) -> bool {
        for header in &mut self.code.sites.header {
            if let Some(repeat) = &mut header.repeat {
                repeat.run.update.operation.inputs[0] =
                    super::code::repeat::Input::Invariant(u32::MAX);
                return true;
            }
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn verify_room_projections(
        &self,
        exe: &Executable,
        owner: &IslandPlan,
    ) -> Result<usize, crate::compile::room::Refused> {
        let mut checked = 0;
        for header in &self.code.sites.header {
            if let Some(repeat) = &header.repeat {
                let plan = exe
                    .islands
                    .scalar
                    .iter()
                    .find(|p| p.stmt == header.id)
                    .ok_or(crate::compile::room::Refused::Source)?;
                if !repeat.run.verify(plan, exe, &owner.shared) {
                    return Err(crate::compile::room::Refused::Source);
                }
                repeat.run.source_room.verify()?;
                checked += 1;
            }
        }
        Ok(checked)
    }

    pub(crate) fn of(plan: &IslandPlan, exe: &Executable) -> Option<Self> {
        Self::of_mode(plan, exe, true)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn generic(plan: &IslandPlan, exe: &Executable) -> Option<Self> {
        Self::of_mode(plan, exe, false)
    }

    fn of_mode(plan: &IslandPlan, exe: &Executable, fused: bool) -> Option<Self> {
        let stream = plan.stream(exe);
        let mut measure = Build::new(exe, &plan.shared, stream, None, 0);
        measure.fused = fused;
        measure.charged = plan.charged;
        measure.block(&plan.body)?;
        let mut cx = Build::new(
            exe,
            &plan.shared,
            stream,
            Some(measure.segments),
            measure.registers as u32,
        );
        cx.fused = fused;
        cx.charged = plan.charged;
        cx.block(&plan.body)?;
        if fused {
            cx.access_edges();
            cx.numeric();
            cx.updates();
            cx.formats();
        }
        if fused && !plan.charged && stream.layout.0.is_multiple_of(2) {
            cx.merges(&plan.body);
        }
        if cx.registers as u64 * super::SLOT_BYTES > super::ISLAND_HOLD {
            return None;
        }
        let literal = if plan.charged {
            match cx.ops.first().map(|instruction| &instruction.op) {
                Some(Op::ReturnLiteral {
                    entry: Some(_),
                    value,
                    close,
                    id,
                    index,
                    ..
                }) => Some(LiteralReturn {
                    value: *value,
                    units: 1 + close.map_or(0, |close| u64::from(close.total)),
                    statement: *id,
                    index: *index,
                    close: *close,
                }),
                _ => None,
            }
        } else {
            None
        };
        let code = super::code::Code::lower(&mut cx.ops, &mut cx.blocks)?;
        code.verify(stream).ok()?;
        code.verify_families(plan, exe).ok()?;
        if (cx.registers + code.sites.try_.len()) as u64 * super::SLOT_BYTES > super::ISLAND_HOLD {
            return None;
        }
        // These indexes project the common inventory. Source coordinates
        // remain the identity; physical positions are only lookup results.
        let mut headers = Vec::new();
        let mut actions = Vec::new();
        let mut cleanup = Vec::new();
        let mut restarts = Vec::new();
        for (at, packet) in code.packets.iter().enumerate() {
            use super::code::Opcode;
            match packet.opcode {
                Opcode::Header => headers.push((code.sites.header[packet.x as usize].id.0, at)),
                Opcode::Try => {
                    let entry = &code.sites.try_[packet.x as usize];
                    cleanup.push(Cleanup {
                        id: entry.id,
                        finally: entry.finally,
                        after: entry.after,
                        pending: packet.x as usize,
                    });
                }
                Opcode::Keep => restarts.push(code.sites.keep[packet.x as usize].value),
                Opcode::Spawn | Opcode::Yield | Opcode::Join | Opcode::Wait | Opcode::Cancel => {
                    actions.push((packet.y, at + 1));
                }
                _ => {}
            }
        }
        headers.sort_unstable_by_key(|(id, _)| *id);
        actions.sort_unstable_by_key(|(pc, _)| *pc);
        cleanup.sort_unstable_by_key(|entry| entry.id.0);
        Some(Self {
            literal,
            source_room: std::sync::Arc::clone(&plan.source_room),
            layout: Layout::of(plan)?,
            blocks: cx.blocks,
            cleanups: cleanup.iter().any(|entry| entry.finally.is_some()),
            cleanup: cleanup.into(),
            restarts: restarts.into(),
            code,
            registers: cx.registers,
            headers: headers.into(),
            actions: actions.into(),
            cell_parameters: plan
                .params
                .iter()
                .filter(|at| plan.shared[**at as usize])
                .count(),
            room: cx.room,
            observes: cx.observes,
        })
    }

    pub(super) fn cleanup(&self, id: StmtId) -> Option<Cleanup> {
        self.cleanup
            .binary_search_by_key(&id.0, |entry| entry.id.0)
            .ok()
            .map(|at| self.cleanup[at])
    }

    #[inline(always)]
    pub(super) fn header(&self, id: StmtId) -> Option<usize> {
        self.headers
            .binary_search_by_key(&id.0, |(id, _)| *id)
            .ok()
            .map(|at| self.headers[at].1)
    }

    #[inline(always)]
    pub(super) fn action_after(&self, source: usize) -> Option<usize> {
        let pc = u32::try_from(source.checked_sub(1)?).ok()?;
        self.actions
            .binary_search_by_key(&pc, |(pc, _)| *pc)
            .ok()
            .map(|at| self.actions[at].1)
    }

    pub(super) fn pending_slots(&self) -> usize {
        self.cleanup.len()
    }

    /// Covers both the physical value and its source continuation role on exit.
    pub(super) fn register_bytes(&self) -> u64 {
        (self.registers + self.pending_slots()) as u64 * super::SLOT_BYTES
    }

    #[inline(always)]
    pub(super) fn admits(&self, running: u64, call_depth: u64, observed: bool) -> bool {
        !(observed && self.observes)
            && running
                .checked_add(u64::from(self.room))
                .is_some_and(|end| end <= call_depth)
    }
}

/// The answer of `pair`'s kernel on `x` and `y` when both are floats or
/// both word integers and it has a kernel for them, with the units operator
/// `site`'s table charges it where it certifies its sizes.
#[inline(always)]
pub(super) fn typed(
    site: &OperatorSite,
    pair: &lash_kernel_doc::scalar::Pair,
    x: Value,
    y: Value,
    charging: bool,
    exe: &Executable,
) -> Option<(Value, Option<u64>)> {
    use lash_kernel_doc::scalar;
    match (x, y) {
        (Value::Float(p), Value::Float(q)) => pair
            .floats
            .and_then(|op| scalar::floats(op, p, q))
            .map(|value| {
                (
                    value,
                    if charging {
                        site.table(exe).units(0)
                    } else {
                        Some(0)
                    },
                )
            }),
        (Value::Int(p), Value::Int(q)) => {
            pair.ints
                .and_then(|op| scalar::ints(op, p, q))
                .map(|value| {
                    (
                        value,
                        if charging {
                            site.table(exe).units(super::sizes(&[x, y], &value))
                        } else {
                            Some(0)
                        },
                    )
                })
        }
        _ => None,
    }
}

/// A native's certified text relation. Keep arena reads and variable-length
/// charging out of the numeric operator's instruction path.
#[inline(never)]
pub(crate) fn text_relation(
    site: &OperatorSite,
    x: Value,
    y: Value,
    exe: &Executable,
    arena: &lash_kernel_state::arena::Arena,
    charging: bool,
) -> Option<(Value, Option<u64>)> {
    let op = exe.lib(site.lib).kernels(exe).texts?;
    if !matches!(
        (x, y),
        (
            Value::Text(_) | Value::LongText(_),
            Value::Text(_) | Value::LongText(_)
        )
    ) {
        return None;
    }
    let signature = site.checked.then(|| &exe.lib(site.lib).header.signature);
    if signature.is_some_and(|signature| {
        !signature.params.iter().zip([x, y]).all(|(param, value)| {
            (param.optional && matches!(value, Value::Absent))
                || crate::compile::import_result_fits(&param.ty, value)
        })
    }) {
        return None;
    }
    let semantics = arena.semantics();
    let value =
        lash_kernel_doc::scalar::texts(op, semantics.text(&x).ok()?, semantics.text(&y).ok()?)?;
    if signature
        .is_some_and(|signature| !crate::compile::import_result_fits(&signature.result, value))
    {
        return None;
    }
    Some((
        value,
        if charging {
            super::cx::measured_formula(arena, &exe.lib(site.lib).charge, &[x, y], Some(&value))
        } else {
            Some(0)
        },
    ))
}

/// An immediate unary answer uses the same prepared entry and scalar
/// table as the general operator path, without an arena measurement.
#[inline(always)]
pub(crate) fn unary(
    site: &OperatorSite,
    x: Value,
    exe: &Executable,
) -> Option<(Value, Option<u64>)> {
    if !crate::compile::scalar(&x) {
        return None;
    }
    if site.checked
        && !exe
            .lib(site.lib)
            .header
            .signature
            .params
            .iter()
            .zip([x])
            .all(|(param, value)| {
                (param.optional && matches!(value, Value::Absent))
                    || crate::compile::import_result_fits(&param.ty, value)
            })
    {
        return None;
    }
    let value = site.entry(exe).call(&[x])?;
    if !crate::compile::scalar(&value)
        || (site.checked
            && !crate::compile::import_result_fits(
                &exe.lib(site.lib).header.signature.result,
                value,
            ))
    {
        return None;
    }
    Some((value, site.table(exe).units(super::sizes(&[x], &value))))
}

/// Resolves an operator's entry, charge and resume sites.
pub(super) fn operator_site(expr: &Expr, exe: &Executable) -> Option<OperatorSite> {
    let (pc, reg) = (expr.pc, expr.reg);
    let Kind::Operator {
        lib,
        invoke,
        closes,
        ..
    } = &expr.kind
    else {
        return None;
    };
    let function = exe.lib(*lib);
    let (entry, fast) = function.specialization.prepared.as_ref()?;
    let extra = entry.depth();
    let depth = if function.operation() {
        Depth::Operation(extra)
    } else if extra > 0 {
        Depth::Template(extra)
    } else {
        Depth::None
    };
    let counted = invoke.and_then(|invoke| invoke.counted());
    let act = invoke
        .filter(|invoke| invoke.call)
        .and_then(|invoke| invoke.act);
    fast.scalar()?;
    let close = closes.first().copied();
    let site = OperatorSite {
        checked: invoke.is_some_and(|invoke| invoke.checked),
        lib: *lib,
        close,
        fixed: close.map_or(0, |close| u64::from(close.total)),
        pc,
        reg,
        counted,
        act,
        root: None,
        body_when_observed: act.is_some() && matches!(function.run, LibRun::Body(_)),
        depth,
    };
    Some(site)
}

/// A kind-pure pair of calls and its first general answer per kind.
pub(crate) struct KindTest {
    pub(super) kind: OperatorSite,
    pub(super) same: OperatorSite,
    pub(super) literal: Value,
    pub(super) memo: crate::compile::kind_test::KindMemo,
}
