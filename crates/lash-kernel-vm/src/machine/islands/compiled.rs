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
use program::Build;
pub(super) use program::{Instruction, Op, Read};

pub(crate) struct Compiled {
    pub(super) layout: Layout,
    pub(super) blocks: Vec<Block>,
    pub(super) ops: Vec<Instruction>,
    pub(super) registers: usize,
    headers: Box<[(u32, usize, bool)]>,
    actions: Box<[(u32, usize)]>,
    pub(super) cell_parameters: usize,
    pub(super) cleanups: bool,
    room: u32,
    observes: bool,
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
            for (index, value) in args.enumerate() {
                if index < given {
                    slots[index] = SlotState::Value(value);
                }
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
pub(super) enum Depth {
    None,
    /// An operation's admission, then its template's frames beyond one.
    Operation(u32),
    /// A template's frames.
    Template(u32),
}

/// A compiled operator or helper call.
pub(super) struct OperatorSite {
    pub(super) checked: bool,
    pub(super) lib: LibId,
    pub(super) entry: Entry,
    pub(super) table: ScalarTable,
    pub(super) pair: Option<lash_kernel_doc::scalar::Pair>,
    /// What its typed path's answer on two floats charges: its table's
    /// units for sizes of 1, as a float kernel answers a float or a
    /// boolean.
    pub(super) floats: Option<u64>,
    pub(super) close: Option<Close>,
    /// What its close charges.
    pub(super) fixed: u64,
    pub(super) pc: u32,
    pub(super) reg: u32,
    pub(super) counted: Option<usize>,
    pub(super) act: Option<ActIx>,
    pub(super) root: Option<StmtId>,
    /// A body-only helper exposes its frame to work observation.
    pub(super) body_when_observed: bool,
    pub(super) depth: Depth,
}

/// A prepared heap operation keeps the same semantic call coordinates.
pub(super) struct HeapSite {
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
pub(super) struct NativeSite {
    pub(super) lib: LibId,
    pub(super) close: Option<Close>,
    pub(super) pc: u32,
    pub(super) reg: u32,
    /// The depth its prepared entry's template checks.
    pub(super) depth: Depth,
    /// What the call charges where it is `list.len`, whose answer on a
    /// resident list is read from the arena ([`crate::compile::Lib::length_read`]).
    pub(super) length: Option<u64>,
}

/// A compiled test `same(x, ())` of a value against the empty tuple,
/// answered without building the tuple.
pub(super) struct UnitSite {
    pub(super) lib: LibId,
    pub(super) close: Option<Close>,
    /// Where an exit before it resumes: where the tuple's construction
    /// resumes, the head of the region `close` closes when it has one.
    pub(super) miss: u32,
    pub(super) reg: u32,
}

/// A compiled island call.
pub(super) struct CallSite {
    /// The source library call and root action, retained on materialization.
    pub(super) library: Option<LibId>,
    /// The call depth its library's prepared answer needs beyond the
    /// caller's, where an unpaid body call may answer immediately
    /// ([`answer_depth`]).
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

/// Where library `lib`'s body call may answer immediately: an uncharged
/// body of its own parameters whose prepared entry is not statement
/// metered. The call depth its immediate paths need beyond the caller's.
pub(super) fn answer_depth(exe: &Executable, lib: LibId) -> Option<u32> {
    let function = exe.lib(lib);
    let LibRun::Body(code) = function.run else {
        return None;
    };
    if function.shares_params || exe.code(code).charged {
        return None;
    }
    let (prepared, _) = function.prepared.as_ref()?;
    if matches!(prepared, Entry::Template { template, .. } if template.metered.is_some()) {
        return None;
    }
    Some(prepared.deepest(&|id| exe.lib(id), &|id| exe.library_lib(id)))
}

/// A whole typed scalar loop, compiled from its verified ordered facts.
pub(super) struct RepeatSite {
    pub(super) run: crate::compile::Typed,
    pub(super) iteration_max: u64,
}

impl Compiled {
    pub(crate) fn of(plan: &IslandPlan, exe: &Executable) -> Option<Self> {
        let stream = plan.stream(exe);
        let mut measure = Build::new(exe, &plan.shared, stream, None);
        measure.block(&plan.body)?;
        let mut cx = Build::new(exe, &plan.shared, stream, Some(measure.segments));
        cx.block(&plan.body)?;
        if cx.registers as u64 * super::SLOT_BYTES > super::ISLAND_HOLD {
            return None;
        }
        // These indexes project the common inventory. Source coordinates
        // remain the identity; physical positions are only lookup results.
        let mut headers = Vec::new();
        let mut actions = Vec::new();
        for (at, instruction) in cx.ops.iter().enumerate() {
            if let Op::Header { id, .. } = instruction.op {
                headers.push((id.0, at, false));
            }
            if let Some(pc) = instruction.op.scheduler_pc() {
                actions.push((pc, at + 1));
            }
        }
        // A loop the register program owns resumes there after a cold
        // exit: a scalar region's loop, a verified scalar loop, or one whose
        // range contains a scheduler handoff. An allocating region's loop
        // compiles to its cold operation, with no header.
        for (id, at, owned) in &mut headers {
            let Op::Header { after, .. } = cx.ops[*at].op else {
                unreachable!("a header index names its instruction");
            };
            *owned = matches!(
                exe.stmt(StmtId(*id)).form,
                crate::compile::Form::While {
                    region: Some(_),
                    ..
                } | crate::compile::Form::For {
                    region: Some(_),
                    ..
                }
            ) || exe.islands.scalar.iter().any(|plan| plan.stmt.0 == *id)
                || actions
                    .iter()
                    .any(|(_, next)| *at < *next && *next <= after);
        }
        headers.sort_unstable_by_key(|(id, _, _)| *id);
        actions.sort_unstable_by_key(|(pc, _)| *pc);
        Some(Self {
            layout: Layout::of(plan)?,
            blocks: cx.blocks,
            cleanups: cx
                .ops
                .iter()
                .any(|instruction| matches!(instruction.op, Op::Try { .. })),
            ops: cx.ops,
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

    #[inline(always)]
    pub(super) fn header(&self, id: StmtId) -> Option<usize> {
        self.headers
            .binary_search_by_key(&id.0, |(id, _, _)| *id)
            .ok()
            .map(|at| self.headers[at].1)
    }

    /// A generic cold activation re-enters only a loop the register
    /// program owns: a scalar region's loop, a verified scalar loop, or one
    /// whose common instruction range contains a scheduler handoff.
    #[inline(always)]
    pub(super) fn owns_loop(&self, id: StmtId) -> bool {
        self.headers
            .binary_search_by_key(&id.0, |(id, _, _)| *id)
            .is_ok_and(|at| self.headers[at].2)
    }

    #[inline(always)]
    pub(super) fn action_after(&self, source: usize) -> Option<usize> {
        let pc = u32::try_from(source.checked_sub(1)?).ok()?;
        self.actions
            .binary_search_by_key(&pc, |(pc, _)| *pc)
            .ok()
            .map(|at| self.actions[at].1)
    }

    /// Covers both the physical value and its source continuation role on exit.
    pub(super) fn register_bytes(&self) -> u64 {
        self.registers as u64 * super::SLOT_BYTES
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
    exe: &Executable,
    charging: bool,
) -> Option<(Value, Option<u64>)> {
    use lash_kernel_doc::scalar;
    let signature = site.checked.then(|| &exe.lib(site.lib).header.signature);
    if signature.is_some_and(|signature| {
        !signature.params.iter().zip([x, y]).all(|(param, value)| {
            (param.optional && matches!(value, Value::Absent))
                || crate::compile::import_result_fits(&param.ty, value)
        })
    }) {
        return None;
    }
    let answer = match (x, y) {
        (Value::Float(p), Value::Float(q)) => Some((
            scalar::floats(pair.floats?, p, q)?,
            if charging { site.floats } else { Some(0) },
        )),
        (Value::Int(p), Value::Int(q)) => {
            let value = scalar::ints(pair.ints?, p, q)?;
            Some((
                value,
                if charging {
                    site.table.units(super::sizes(&[x, y], &value))
                } else {
                    Some(0)
                },
            ))
        }
        _ => None,
    }?;
    signature
        .is_none_or(|signature| crate::compile::import_result_fits(&signature.result, answer.0))
        .then_some(answer)
}

/// An immediate unary answer uses the same prepared entry and scalar
/// table as the general operator path, without an arena measurement.
#[inline(always)]
pub(super) fn unary(
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
    let value = site.entry.call(&[x])?;
    if !crate::compile::scalar(&value)
        || (site.checked
            && !crate::compile::import_result_fits(
                &exe.lib(site.lib).header.signature.result,
                value,
            ))
    {
        return None;
    }
    Some((value, site.table.units(super::sizes(&[x], &value))))
}

/// Resolves an operator's entry, charge and resume sites.
fn operator_site(expr: &Expr, exe: &Executable) -> Option<OperatorSite> {
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
    let (entry, fast) = function.prepared.as_ref()?;
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
    let table = fast.scalar()?.clone();
    let close = closes.first().copied();
    let site = OperatorSite {
        checked: invoke.is_some_and(|invoke| invoke.checked),
        lib: *lib,
        entry: entry.clone(),
        pair: entry.pair(),
        floats: table.units(0),
        table,
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
pub(super) struct KindTest {
    pub(super) kind: OperatorSite,
    pub(super) same: OperatorSite,
    pub(super) literal: Value,
    pub(super) memo: crate::compile::kind_test::KindMemo,
}
