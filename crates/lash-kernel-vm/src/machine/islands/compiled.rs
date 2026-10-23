//! Private register program for an admitted complete scalar/control body.
//! Source coordinates and event tables remain the authority at every exit.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_exec::ActIx;

use super::{MAX_SLOTS, Slots};
use crate::compile::expr::{Close, Expr, Kind};
use crate::compile::islands::IslandPlan;
use crate::compile::{BlockId, CodeId, Entry, Executable, LibId, LibRun, ScalarTable, StmtId};

#[path = "program.rs"]
mod program;
use program::Build;
pub(super) use program::{Instruction, Op};

pub(crate) struct Compiled {
    pub(super) layout: Layout,
    pub(super) blocks: Vec<Block>,
    pub(super) ops: Vec<Instruction>,
    pub(super) registers: usize,
    room: u32,
    observes: bool,
}

pub(super) struct Block {
    pub(super) id: BlockId,
    pub(super) declares: Box<[u32]>,
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
    params: [u8; MAX_SLOTS],
    captures: [u8; MAX_SLOTS],
    captured: usize,
}

impl Layout {
    fn of(plan: &IslandPlan) -> Option<Self> {
        let mut layout = Self {
            arity: plan.params.len(),
            leading: plan
                .params
                .iter()
                .enumerate()
                .all(|(index, at)| *at as usize == index),
            params: [0; MAX_SLOTS],
            captures: [0; MAX_SLOTS],
            captured: plan.captures.len(),
        };
        for (to, from) in [
            (&mut layout.params, &plan.params),
            (&mut layout.captures, &plan.captures),
        ] {
            for (to, at) in to.iter_mut().zip(from.iter()) {
                *to = u8::try_from(*at)
                    .ok()
                    .filter(|at| usize::from(*at) < MAX_SLOTS)?;
            }
            if from.len() > MAX_SLOTS {
                return None;
            }
        }
        Some(layout)
    }

    /// Lays out the fresh frame `slots`, every variable empty: `args`
    /// in its parameters, those past them absent, and the captured values
    /// from `captured` in its captured variables. `args` are at most its
    /// arity.
    #[inline(always)]
    pub(super) fn fill(
        &self,
        slots: &mut Slots,
        args: &[Value],
        captured: &[(Value, ArenaHandle)],
    ) {
        let given = args.len().min(self.arity);
        if self.leading {
            // Over `args`, whose length a call site fixes, so the copy
            // unrolls there rather than becoming a call of `memcpy`.
            for (index, value) in args.iter().enumerate() {
                if index < given {
                    slots[index] = Some(*value);
                }
            }
            for slot in &mut slots[given..self.arity] {
                *slot = Some(Value::Absent);
            }
        } else {
            for (index, at) in self.params[..self.arity].iter().enumerate() {
                slots[usize::from(*at)] = Some(args.get(index).copied().unwrap_or(Value::Absent));
            }
        }
        for (at, (value, _)) in self.captures[..self.captured].iter().zip(captured) {
            slots[usize::from(*at)] = Some(*value);
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
    /// A helper whose deferred plan an observed run runs as its body, as
    /// it has no fallback (`answer_admitted`).
    pub(super) body_when_observed: bool,
    pub(super) depth: Depth,
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
    pub(super) callee: Option<CodeId>,
    pub(super) close: Option<Close>,
    pub(super) pc: u32,
    pub(super) reg: u32,
    pub(super) act: ActIx,
    pub(super) occurrence: usize,
    pub(super) site: u32,
    /// The most the segment from its return charges; 0 where none follows.
    pub(super) after: u64,
}

impl Compiled {
    pub(crate) fn of(plan: &IslandPlan, exe: &Executable) -> Option<Self> {
        let mut measure = Build::new(exe, None);
        measure.block(&plan.body)?;
        let mut cx = Build::new(exe, Some(measure.segments));
        cx.block(&plan.body)?;
        Some(Self {
            layout: Layout::of(plan)?,
            blocks: cx.blocks,
            ops: cx.ops,
            registers: cx.registers,
            room: cx.room,
            observes: cx.observes,
        })
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
) -> Option<(Value, Option<u64>)> {
    use lash_kernel_doc::scalar;
    let signature = &exe.lib(site.lib).header.signature;
    if site.checked
        && !signature.params.iter().zip([x, y]).all(|(param, value)| {
            (param.optional && matches!(value, Value::Absent))
                || crate::compile::import_result_fits(&param.ty, value)
        })
    {
        return None;
    }
    let answer = match (x, y) {
        (Value::Float(p), Value::Float(q)) => {
            Some((scalar::floats(pair.floats?, p, q)?, site.floats))
        }
        (Value::Int(p), Value::Int(q)) => {
            let value = scalar::ints(pair.ints?, p, q)?;
            Some((value, site.table.units(super::sizes(&[x, y], &value))))
        }
        _ => None,
    }?;
    (!site.checked || crate::compile::import_result_fits(&signature.result, answer.0))
        .then_some(answer)
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
    let counted = invoke
        .filter(|invoke| invoke.call)
        .map(|invoke| invoke.occurrence);
    let table = fast.scalar()?.clone();
    let close = closes.first().copied();
    let site = OperatorSite {
        checked: invoke.is_some_and(|invoke| invoke.checked),
        lib: *lib,
        entry: entry.clone(),
        floats: table.units(0),
        table,
        close,
        fixed: close.map_or(0, |close| u64::from(close.total)),
        pc,
        reg,
        counted,
        body_when_observed: counted.is_some()
            && matches!(function.run, LibRun::DeferredPlan { .. })
            && exe.fallback(*lib).is_none(),
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
