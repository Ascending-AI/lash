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
#[path = "resource.rs"]
pub(super) mod resource;
pub(crate) use super::code::Op;
use program::Build;
pub(super) use program::{
    CallInputs, CollectionAppend, CollectionPacket, CollectionStep, Destination, Format, Merge,
    Operand, Placement, ReadSlot, Reduction, Visit, WriteSite,
};
pub(crate) use program::{Instruction, Read, StatementEntry, StoreInputs};

#[cfg(test)]
thread_local! {
    pub(crate) static CONSTRUCTOR_DEBT_PUBLICATIONS: std::cell::Cell<[u64; 3]> = const { std::cell::Cell::new([0; 3]) };
}
#[cfg(test)]
pub(crate) fn take_constructor_debt_publications() -> [u64; 3] {
    CONSTRUCTOR_DEBT_PUBLICATIONS.with(|counts| counts.replace([0; 3]))
}
#[cfg(test)]
pub(in crate::machine::islands) fn constructor_debt_published(family: usize) {
    CONSTRUCTOR_DEBT_PUBLICATIONS.with(|counts| {
        let mut current = counts.get();
        current[family] += 1;
        counts.set(current);
    });
}

#[cfg(test)]
thread_local! { static ORIGINAL_PAYMENTS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) }; }
#[cfg(test)]
pub(crate) fn take_original_payments() -> (u64, u64) {
    ORIGINAL_PAYMENTS.with(|payments| payments.replace((0, 0)))
}
#[cfg(test)]
pub(in crate::machine::islands) fn original_paid(units: u64) {
    ORIGINAL_PAYMENTS.with(|payments| {
        let (count, paid) = payments.get();
        if count == 0 {
            eprintln!(
                "actual constructor original publication: paid {units} units; affine receipt issued"
            );
        }
        payments.set((count + 1, paid + units));
    });
}

/// Heap text construction and operations the source handler must answer.
#[derive(Clone)]
pub(crate) enum Deferred {
    Source(u32),
    Text {
        dst: u32,
        constant: lash_kernel_exec::ConstIx,
        source: crate::compile::expr::SourceRead,
        original: std::sync::Arc<crate::compile::room::charges::OriginalPrefix>,
    },
}

/// A literal return's value and source-derived statement/close price.
pub(super) struct LiteralReturn {
    pub(super) reg: u32,
    pub(super) value: Value,
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
    pub(super) placement: Option<std::sync::Arc<crate::compile::room::placement::BindingPlacement>>,
    pub(super) literal: Option<LiteralReturn>,
    pub(super) layout: Layout,
    pub(super) blocks: Vec<Block>,
    members: Box<[super::execute::member_region::MemberEntry]>,
    pub(super) code: super::code::Code,
    pub(super) registers: usize,
    headers: Box<[(u32, usize)]>,
    actions: Box<[(u32, usize)]>,
    native_suffixes: Box<[(u32, usize)]>,
    pub(super) cell_parameters: usize,
    pub(super) cleanups: bool,
    cleanup: Box<[Cleanup]>,
    room: u32,
    observes: bool,
    pub(crate) source_room: std::sync::Arc<crate::compile::room::Source>,
}

pub(super) struct Block {
    pub(super) id: BlockId,
    pub(super) declares: Box<[u32]>,
    /// Physical entries projected from this block's source statement order.
    pub(super) starts: Box<[usize]>,
    pub(super) end: usize,
    pub(super) after: usize,
    /// The slots this block's Leave releases when it is a proven release:
    /// its scope and operand groups, resolved once from the immutable source.
    pub(super) leave:
        std::sync::OnceLock<Option<Box<[lash_kernel_state::kernel_internals::logical::Slot]>>>,
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
    /// The canonical source close and formula, compiled with this packet.
    pub(super) charges: Option<std::sync::Arc<crate::compile::room::charges::ChargeProgram>>,
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

/// A compiled general native call.
#[derive(Clone)]
pub(crate) struct NativeSite {
    pub(super) lib: LibId,
    pub(super) close: Option<Close>,
    pub(super) charges: Option<std::sync::Arc<crate::compile::room::charges::ChargeProgram>>,
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
#[derive(Clone)]
pub(crate) struct UnitSource {
    pub(super) compiled: std::sync::Arc<crate::compile::room::publication::UnitComparison>,
    pub(super) statement: StmtId,
    pub(super) binding: Option<u32>,
    pub(super) value: lash_kernel_value::sites::Site,
    pub(super) tuple: lash_kernel_value::sites::Site,
    pub(super) comparison: lash_kernel_value::sites::Site,
    pub(super) test: lash_kernel_value::sites::Site,
}
pub(crate) struct UnitSite {
    pub(super) source: Option<Box<UnitSource>>,
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
        if let Some(site) = self.code.sites.format.first_mut() {
            site.value.joined = Box::default();
            return true;
        }
        false
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
        let placement = crate::compile::room::placement::BindingPlacement::compile(exe, plan.code)
            .ok()
            .map(std::sync::Arc::new);
        let mut measure = Build::new(exe, &plan.shared, stream, None);
        measure.placement = placement.clone();
        measure.fused = fused;
        measure.charged = plan.charged;
        measure.block(&plan.body)?;
        let mut cx = Build::new(exe, &plan.shared, stream, Some(measure.segments));
        cx.fused = fused;
        cx.placement = placement.clone();
        cx.charged = plan.charged;
        cx.block(&plan.body)?;
        if fused {
            cx.numeric();
            cx.formats();
        }
        if placement.is_some() {
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
                    reg,
                    ..
                }) => Some(LiteralReturn {
                    reg: *reg,
                    value: *value,
                    statement: *id,
                    index: *index,
                    close: *close,
                }),
                _ => None,
            }
        } else {
            None
        };
        let mut code = super::code::Code::lower(&mut cx.ops, &mut cx.blocks, fused)?;
        let native_suffixes = code.text_suffixes(stream)?;
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
                Opcode::Spawn | Opcode::Yield | Opcode::Join | Opcode::Wait | Opcode::Cancel => {
                    actions.push((packet.y, at + 1));
                }
                _ => {}
            }
        }
        headers.sort_unstable_by_key(|(id, _)| *id);
        actions.sort_unstable_by_key(|(pc, _)| *pc);
        cleanup.sort_unstable_by_key(|entry| entry.id.0);
        let mut members = Vec::new();
        if fused && let Some(placement) = &placement {
            for (block, physical) in cx.blocks.iter().enumerate() {
                for (index, packet) in physical.starts.iter().copied().enumerate() {
                    if !code.entry(packet).is_some_and(|entry| {
                        entry.index == index && entry.id == exe.block(physical.id).stmts[index]
                    }) {
                        continue;
                    }
                    if let Some(entry) = super::execute::member_region::MemberEntry::compile(
                        exe,
                        std::sync::Arc::clone(placement),
                        block,
                        physical,
                        index,
                    ) {
                        members.push(entry);
                    }
                }
            }
        }
        members.sort_unstable_by_key(super::execute::member_region::MemberEntry::packet);
        let compiled = Self {
            placement,
            members: members.into(),
            literal,
            source_room: std::sync::Arc::clone(&plan.source_room),
            layout: Layout::of(plan)?,
            blocks: cx.blocks,
            cleanups: cleanup.iter().any(|entry| entry.finally.is_some()),
            cleanup: cleanup.into(),
            code,
            registers: cx.registers,
            headers: headers.into(),
            actions: actions.into(),
            native_suffixes: native_suffixes.into(),
            cell_parameters: plan
                .params
                .iter()
                .filter(|at| plan.shared[**at as usize])
                .count(),
            room: cx.room,
            observes: cx.observes,
        };
        compiled.compile_collection_sites(exe)?;
        Some(compiled)
    }

    pub(super) fn member(
        &self,
        packet: usize,
    ) -> Option<&super::execute::member_region::MemberEntry> {
        self.members
            .binary_search_by_key(&packet, super::execute::member_region::MemberEntry::packet)
            .ok()
            .map(|at| &self.members[at])
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

    pub(super) fn native_suffix(&self, source: usize) -> Option<usize> {
        let pc = u32::try_from(source).ok()?;
        self.native_suffixes
            .binary_search_by_key(&pc, |(pc, _)| *pc)
            .ok()
            .map(|at| self.native_suffixes[at].1)
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
    typed_table(pair, x, y, charging.then(|| site.table(exe)))
}

#[inline(always)]
pub(super) fn typed_table(
    pair: &lash_kernel_doc::scalar::Pair,
    x: Value,
    y: Value,
    table: Option<&ScalarTable>,
) -> Option<(Value, Option<u64>)> {
    use lash_kernel_doc::scalar;
    match (x, y) {
        (Value::Float(p), Value::Float(q)) => pair
            .floats
            .and_then(|op| scalar::floats(op, p, q))
            .map(|value| (value, table.map_or(Some(0), |table| table.units(0)))),
        (Value::Int(p), Value::Int(q)) => {
            pair.ints
                .and_then(|op| scalar::ints(op, p, q))
                .map(|value| {
                    (
                        value,
                        table.map_or(Some(0), |table| table.units(super::sizes(&[x, y], &value))),
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
        charges: None,
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
