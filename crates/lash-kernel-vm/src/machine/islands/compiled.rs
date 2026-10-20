//! An island plan's compiled form: each statement of its body and each
//! expression of its resolved tree as a closure over the island's virtual
//! frame, which charges every event as it runs and records, at a stop, the
//! registers the frame holds and the op the reference resumes at.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_exec::ActIx;

use super::{Answer, Island, MAX_ARGS, Slots};
use crate::compile::expr::{Arm, Callee, Close, Expr, Kind};
use crate::compile::islands::{
    Block, IslandPlan, Kind as StatementKind, MAX_SLOTS, Statement, held, unit_test,
};
use crate::compile::{BlockId, CodeId, Entry, Executable, LibId, LibRun, ScalarTable, StmtId};

/// A compiled island expression: its value, every event charged as it
/// runs, or a stop with the registers it held recorded.
pub(super) type Node = Box<dyn Fn(&mut Island<'_>, &mut Slots) -> Answer + Send + Sync>;

/// A compiled operand: a variable, a session binding or a constant read
/// in place, or an expression.
pub(super) enum Operand {
    Slot { at: usize, miss: u32 },
    Session { name: u32, miss: u32 },
    Const(Value),
    Node(Node),
}

impl Operand {
    #[inline(always)]
    pub(super) fn eval(&self, island: &mut Island<'_>, slots: &mut Slots) -> Answer {
        match self {
            Self::Slot { at, miss } => match slots.get(*at) {
                Some(Some(value)) => Ok(*value),
                _ => Err(island.at(*miss)),
            },
            Self::Session { name, miss } => match island.session(*name) {
                Some(value) => Ok(value),
                None => Err(island.at(*miss)),
            },
            Self::Const(value) => Ok(*value),
            Self::Node(node) => node(island, slots),
        }
    }
}

/// An island plan's compiled form.
pub(crate) struct Compiled {
    pub(super) layout: Layout,
    pub(super) body: CBlock,
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

pub(super) struct CBlock {
    pub(super) id: BlockId,
    pub(super) declares: Box<[u32]>,
    pub(super) statements: Box<[CStatement]>,
}

pub(super) struct CStatement {
    pub(super) id: StmtId,
    pub(super) leaves: u32,
    pub(super) kind: CKind,
}

pub(super) enum CKind {
    Return {
        value: Operand,
        close: Option<Close>,
        reg: u32,
    },
    Write {
        value: Operand,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        at: u32,
        bind: bool,
    },
    If {
        cond: Operand,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        then_: CBlock,
        else_: CBlock,
    },
    Discard {
        value: Operand,
        close: Option<Close>,
        reg: u32,
    },
    /// A statement no island runs: reaching it exits at its first op.
    Cold { pc: u32 },
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
    pub(super) lib: LibId,
    pub(super) entry: Entry,
    pub(super) table: ScalarTable,
    /// What its typed path's answer on two floats charges: its table's
    /// units for sizes of 1, as a float kernel answers a float or a
    /// boolean.
    pub(super) floats: Option<u64>,
    pub(super) close: Option<Close>,
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
}

impl Compiled {
    pub(crate) fn of(plan: &IslandPlan, exe: &Executable) -> Option<Self> {
        Some(Self {
            layout: Layout::of(plan)?,
            body: block(&plan.body, exe)?,
        })
    }
}

fn block(block: &Block, exe: &Executable) -> Option<CBlock> {
    Some(CBlock {
        id: block.id,
        declares: block.declares.clone(),
        statements: block
            .statements
            .iter()
            .map(|statement| self::statement(statement, exe))
            .collect::<Option<_>>()?,
    })
}

fn statement(statement: &Statement, exe: &Executable) -> Option<CStatement> {
    let kind = match &statement.kind {
        StatementKind::Return { value, close, reg } => CKind::Return {
            value: expr(value, exe)?,
            close: *close,
            reg: *reg,
        },
        StatementKind::Write {
            value,
            close,
            pc,
            reg,
            at,
            bind,
        } => CKind::Write {
            value: expr(value, exe)?,
            close: *close,
            pc: *pc,
            reg: *reg,
            at: *at,
            bind: *bind,
        },
        StatementKind::If {
            cond,
            close,
            pc,
            reg,
            then_,
            else_,
        } => CKind::If {
            cond: expr(cond, exe)?,
            close: *close,
            pc: *pc,
            reg: *reg,
            then_: block(then_, exe)?,
            else_: block(else_, exe)?,
        },
        StatementKind::Discard { value, close, reg } => CKind::Discard {
            value: expr(value, exe)?,
            close: *close,
            reg: *reg,
        },
        StatementKind::Cold => CKind::Cold {
            pc: exe.stmt(statement.id).ops.start,
        },
    };
    Some(CStatement {
        id: statement.id,
        leaves: statement.leaves,
        kind,
    })
}

fn expr(expr: &Expr, exe: &Executable) -> Option<Operand> {
    let operands = |args: &[Expr]| {
        args.iter()
            .map(|arg| self::expr(arg, exe))
            .collect::<Option<Vec<_>>>()
    };
    let (pc, reg) = (expr.pc, expr.reg);
    Some(Operand::Node(match &expr.kind {
        Kind::Local { local, miss, .. } => {
            return Some(Operand::Slot {
                at: local.at as usize,
                miss: *miss,
            });
        }
        Kind::Session { name, miss } => {
            return Some(Operand::Session {
                name: name.0,
                miss: *miss,
            });
        }
        Kind::Const(value) => return Some(Operand::Const(*value)),
        Kind::Operator {
            lib, args, closes, ..
        } if unit_test(Some(exe.lib(*lib)), args) => {
            let [value, unit] = &args[..] else {
                return None;
            };
            let Kind::Make { miss, .. } = unit.kind else {
                return None;
            };
            let site = UnitSite {
                lib: *lib,
                close: closes.first().copied(),
                miss,
                reg,
            };
            let value = self::expr(value, exe)?;
            Box::new(move |island, slots| {
                let x = value.eval(island, slots)?;
                island.unit_test(&site, x)
            })
        }
        Kind::Operator { lib, args, .. } if let Some(node) = kind_node(expr, exe, *lib, args) => {
            node
        }
        Kind::Operator { args, .. } => {
            let site = operator_site(expr, exe)?;
            operator(site, operands(args)?)
        }
        Kind::Native { lib, args, closes } => {
            let extra = exe
                .lib(*lib)
                .prepared
                .as_ref()
                .map_or(0, |(entry, _)| entry.depth());
            let site = NativeSite {
                lib: *lib,
                close: closes.first().copied(),
                pc,
                reg,
                depth: if extra > 0 {
                    Depth::Template(extra)
                } else {
                    Depth::None
                },
                length: exe.lib(*lib).length_read(),
            };
            native(site, operands(args)?)
        }
        Kind::Field {
            target,
            field,
            miss,
        } => {
            let target = self::expr(target, exe)?;
            let (field, miss) = (field.ix(), *miss);
            Box::new(move |island, slots| {
                let x = target.eval(island, slots)?;
                match island.field(x, field) {
                    Some(value) => Ok(value),
                    None => Err(island.at_held(miss, reg, x)),
                }
            })
        }
        Kind::Index {
            target,
            index,
            miss,
            ..
        } => {
            let target = self::expr(target, exe)?;
            let index = self::expr(index, exe)?;
            let miss = *miss;
            Box::new(move |island, slots| {
                let x = target.eval(island, slots)?;
                let at = match index.eval(island, slots) {
                    Ok(at) => at,
                    Err(stop) => return Err(island.held(stop, reg, x)),
                };
                match island.item(x, at) {
                    Some(value) => Ok(value),
                    None => Err(island.at_operands(miss, reg, &[x, at])),
                }
            })
        }
        Kind::Call {
            invoke,
            callee,
            args,
            closes,
            site,
        } => {
            let (read, declared) = match callee {
                Callee::Read(read) => (Some(self::expr(read, exe)?), None),
                Callee::Declared(code) => (None, Some(*code)),
            };
            let operands = operands(args)?;
            if operands.len() > MAX_ARGS {
                return None;
            }
            let site = CallSite {
                callee: declared,
                close: closes.first().copied(),
                pc,
                reg,
                act: invoke.act,
                occurrence: invoke.occurrence,
                site: *site,
            };
            call(site, read, operands)
        }
        Kind::Cond {
            cond,
            closes,
            then,
            otherwise,
        } => {
            let condition = self::expr(cond, exe)?;
            let close = closes.first().copied();
            let then_ = arm(then, exe)?;
            let else_ = arm(otherwise, exe)?;
            Box::new(move |island, slots| {
                let value = condition.eval(island, slots)?;
                if let Some(close) = &close
                    && !island.close(close)
                {
                    return Err(island.at_held(close.head, reg, value));
                }
                match value {
                    Value::Bool(true) => then_.eval(island, slots),
                    Value::Bool(false) => else_.eval(island, slots),
                    _ => Err(island.at_held(pc, reg, value)),
                }
            })
        }
        Kind::Let {
            local,
            value,
            body,
            miss,
        } => {
            let value = self::expr(value, exe)?;
            let body = self::expr(body, exe)?;
            let (at, miss) = (local.at as usize, *miss);
            Box::new(move |island, slots| {
                let bound = value.eval(island, slots)?;
                // A held binding is within the headroom admitted at entry;
                // any other is the reference's to admit.
                if !held(&bound) {
                    let stop = island.at(miss);
                    return Err(island.held(stop, reg, bound));
                }
                slots[at] = Some(bound);
                let value = body.eval(island, slots)?;
                slots[at] = None;
                Ok(value)
            })
        }
        Kind::Act | Kind::Make { .. } | Kind::Record { .. } | Kind::Pin(_) => {
            return None;
        }
    }))
}

/// A conditional's `arm`: its value, then the region its end closes; a
/// cold arm exits at its first op.
fn arm(arm: &Arm, exe: &Executable) -> Option<Operand> {
    let Some(value) = &arm.value else {
        let pc = arm.start;
        return Some(Operand::Node(Box::new(move |island, _| Err(island.at(pc)))));
    };
    let value = expr(value, exe)?;
    Some(match arm.closes.first().copied() {
        None => value,
        Some(close) => Operand::Node(Box::new(move |island, slots| {
            let result = value.eval(island, slots)?;
            if island.close(&close) {
                Ok(result)
            } else {
                Err(island.at(close.head))
            }
        })),
    })
}

/// The compiled call `site` of the callee `read` gives, or of its declared
/// code, over `operands`.
fn call(site: CallSite, read: Option<Operand>, operands: Vec<Operand>) -> Node {
    let callee = move |island: &mut Island<'_>, slots: &mut Slots| match &read {
        Some(read) => read.eval(island, slots).map(Some),
        None => Ok(None),
    };
    let mut operands = operands.into_iter();
    match (operands.next(), operands.next(), operands.next()) {
        (Some(a), None, None) => Box::new(move |island, slots| {
            let callee = callee(island, slots)?;
            let x = match a.eval(island, slots) {
                Ok(x) => x,
                Err(stop) => return Err(island.call_held(stop, &site, callee, &[])),
            };
            island.call(&site, callee, &[x])
        }),
        (Some(a), Some(b), None) => Box::new(move |island, slots| {
            let callee = callee(island, slots)?;
            let x = match a.eval(island, slots) {
                Ok(x) => x,
                Err(stop) => return Err(island.call_held(stop, &site, callee, &[])),
            };
            let y = match b.eval(island, slots) {
                Ok(y) => y,
                Err(stop) => return Err(island.call_held(stop, &site, callee, &[x])),
            };
            island.call(&site, callee, &[x, y])
        }),
        (first, second, third) => {
            let operands: Vec<Operand> = first
                .into_iter()
                .chain(second)
                .chain(third)
                .chain(operands)
                .collect();
            Box::new(move |island, slots| {
                let callee = callee(island, slots)?;
                let mut values = [Value::Absent; MAX_ARGS];
                for (index, operand) in operands.iter().enumerate() {
                    match operand.eval(island, slots) {
                        Ok(value) => values[index] = value,
                        Err(stop) => {
                            return Err(island.call_held(stop, &site, callee, &values[..index]));
                        }
                    }
                }
                island.call(&site, callee, &values[..operands.len()])
            })
        }
    }
}

/// The compiled general native call `site` over `operands`.
fn native(site: NativeSite, operands: Vec<Operand>) -> Node {
    Box::new(move |island, slots| {
        let mut values = [Value::Absent; MAX_SLOTS];
        for (index, operand) in operands.iter().enumerate() {
            match operand.eval(island, slots) {
                Ok(value) => values[index] = value,
                Err(stop) => {
                    let mut stop = stop;
                    for (at, value) in values[..index].iter().enumerate() {
                        stop = island.held(stop, site.reg + at as u32, *value);
                    }
                    return Err(stop);
                }
            }
        }
        island.native(&site, &values, operands.len())
    })
}

/// The answer of `pair`'s kernel on `x` and `y` when both are floats or
/// both word integers and it has a kernel for them, with the units operator
/// `site`'s table charges it where it certifies its sizes.
#[inline(always)]
fn typed(
    site: &OperatorSite,
    pair: &lash_kernel_doc::scalar::Pair,
    x: Value,
    y: Value,
) -> Option<(Value, Option<u64>)> {
    use lash_kernel_doc::scalar;
    match (x, y) {
        (Value::Float(p), Value::Float(q)) => {
            Some((scalar::floats(pair.floats?, p, q)?, site.floats))
        }
        (Value::Int(p), Value::Int(q)) => {
            let value = scalar::ints(pair.ints?, p, q)?;
            Some((value, site.table.units(super::sizes(&[x, y], &value))))
        }
        _ => None,
    }
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
    let site = OperatorSite {
        lib: *lib,
        entry: entry.clone(),
        floats: table.units(0),
        table,
        close: closes.first().copied(),
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

/// The fused node of `same(kind(x), literal)` `expr`, where its two calls
/// count no occurrence; each close is replayed at its original call.
fn kind_node(expr: &Expr, exe: &Executable, lib: LibId, args: &[Expr]) -> Option<Node> {
    let (inner, literal) = crate::compile::kind_test::test(exe, lib, args)?;
    let Kind::Operator {
        args: inner_args, ..
    } = &inner.kind
    else {
        return None;
    };
    let kind_site = operator_site(inner, exe)?;
    let same_site = operator_site(expr, exe)?;
    if kind_site.counted.is_some() || same_site.counted.is_some() {
        return None;
    }
    let value = self::expr(&inner_args[0], exe)?;
    let site = KindTest {
        kind: kind_site,
        same: same_site,
        literal,
        memo: Default::default(),
    };
    Some(Box::new(move |island, slots| {
        let x = value.eval(island, slots)?;
        island.kind_test(&site, x)
    }))
}

/// A kind-pure pair of calls and its first general answer per kind.
pub(super) struct KindTest {
    pub(super) kind: OperatorSite,
    pub(super) same: OperatorSite,
    pub(super) literal: Value,
    pub(super) memo: crate::compile::kind_test::KindMemo,
}

/// The compiled operator `site` over `operands`.
fn operator(site: OperatorSite, operands: Vec<Operand>) -> Node {
    let mut operands = operands.into_iter();
    let pair = site.entry.pair();
    match (operands.next(), operands.next(), operands.next()) {
        // Two operands whose entry has typed paths: two floats or two
        // word integers take the path for their kind, committed in place
        // where it commits what the general path would.
        (Some(a), Some(b), None) if let Some(pair) = pair => Box::new(move |island, slots| {
            let x = a.eval(island, slots)?;
            let y = match b.eval(island, slots) {
                Ok(y) => y,
                Err(stop) => return Err(island.held(stop, site.reg, x)),
            };
            if let Some((value, units)) = typed(&site, &pair, x, y)
                && let Some(answer) = island.typed(&site, value, units)
            {
                return answer;
            }
            island.operate(&site, &[x, y])
        }),
        (Some(a), Some(b), None) => Box::new(move |island, slots| {
            let x = a.eval(island, slots)?;
            let y = match b.eval(island, slots) {
                Ok(y) => y,
                Err(stop) => return Err(island.held(stop, site.reg, x)),
            };
            island.operate(&site, &[x, y])
        }),
        (Some(a), None, None) => Box::new(move |island, slots| {
            let x = a.eval(island, slots)?;
            island.operate(&site, &[x])
        }),
        (first, second, third) => {
            let operands: Vec<Operand> = first.into_iter().chain(second).chain(third).collect();
            Box::new(move |island, slots| {
                let mut values = [Value::Absent; 3];
                for (index, operand) in operands.iter().enumerate() {
                    match operand.eval(island, slots) {
                        Ok(value) => values[index] = value,
                        Err(stop) => {
                            let mut stop = stop;
                            for (at, value) in values[..index].iter().enumerate() {
                                stop = island.held(stop, site.reg + at as u32, *value);
                            }
                            return Err(stop);
                        }
                    }
                }
                island.operate(&site, &values[..operands.len()])
            })
        }
    }
}

#[cfg(test)]
#[path = "kind_laws.rs"]
mod kind_laws;
