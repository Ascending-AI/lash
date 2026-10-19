//! An island plan's compiled form: each statement of its body and each
//! expression of its resolved tree as a closure over the island's virtual
//! frame, which charges every event as it runs and records, at a stop, the
//! registers the frame holds and the op the reference resumes at.

use lash_kernel_doc::Value;
use lash_kernel_exec::ActIx;

use super::{Answer, Island, MAX_ARGS, Slots};
use crate::compile::expr::{Arm, Callee, Close, Expr, Kind};
use crate::compile::islands::{
    Block, IslandPlan, Kind as StatementKind, MAX_SLOTS, Statement, held,
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
    pub(super) body: CBlock,
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
            lib,
            invoke,
            args,
            closes,
        } => {
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
            let site = OperatorSite {
                lib: *lib,
                entry: entry.clone(),
                table: fast.scalar()?.clone(),
                close: closes.first().copied(),
                pc,
                reg,
                counted,
                body_when_observed: counted.is_some()
                    && matches!(function.run, LibRun::DeferredPlan { .. })
                    && exe.fallback(*lib).is_none(),
                depth,
            };
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
        Kind::Act | Kind::Make { .. } | Kind::Record { .. } | Kind::Field { .. } | Kind::Pin(_) => {
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

/// The compiled operator `site` over `operands`.
fn operator(site: OperatorSite, operands: Vec<Operand>) -> Node {
    let mut operands = operands.into_iter();
    match (operands.next(), operands.next(), operands.next()) {
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
