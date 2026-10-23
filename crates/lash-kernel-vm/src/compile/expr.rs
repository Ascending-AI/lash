//! The resolved expression tree of a synchronous statement
//! (`docs/design/kernel-regions.md` §1, §2.1): the one reading of a
//! statement's ops that loop regions, their compiled nodes and call islands
//! derive from.
//!
//! [`statement`] reads a statement's ops in the order the reference runs
//! them into one [`Expr`] per value the statement computes: each op's
//! operands resolved to the expressions that computed them, a conditional
//! expression with its two arms, an expression local with its value and
//! its body. Each node carries where it charges and where an exit from it
//! resumes. An op that charges (an operator, a call, a conditional's test)
//! closes the charge regions headed since the op before it that charged:
//! its [`Close`]s, the anchor at which the reference charges them whole. A
//! read, an index and a binding name the op the reference resumes at when a
//! tier cannot run them: the head of the region they stand in, which
//! nothing has charged yet.
//!
//! The tree is the grammar alone: which ops a statement is made of and how
//! their values flow. What a tier runs of it is the tier's to decide: loop
//! formation ([`super::regions`]) and island formation ([`super::islands`])
//! each check the tree against their own restrictions, and each plan's
//! verifier checks what its formation derived against the ops themselves,
//! never against this tree.

use lash_kernel_doc::Value;
use lash_kernel_exec::{
    ActIx, FieldIx, FieldListIx, LocalRef, NameIx, Op, OpRange, Reg, RegRange, SiteIx,
};

use super::{CodeId, Constant, LibId, Stream};
use lash_kernel_doc::JoinMode;
use lash_vocabulary::Type;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkedHook {
    Library(LibId),
    Code(CodeId),
    Import(lash_vocabulary::names::ImportSlot, Option<LibId>),
}
impl LinkedHook {
    pub(crate) fn library(self) -> Option<LibId> {
        match self {
            Self::Library(lib) | Self::Import(_, Some(lib)) => Some(lib),
            _ => None,
        }
    }
}

/// A loop protocol's hooks, compiled (`K-ITER-005`).
#[derive(Clone, Copy)]
pub(crate) struct Via {
    pub(crate) open: LinkedHook,
    pub(crate) next: Option<LinkedHook>,
    pub(crate) item: Option<LinkedHook>,
    pub(crate) check: Option<LinkedHook>,
}

/// What a nested action does.
pub(crate) enum NestedKind {
    Yield,
    Cancel,
    /// A `call`, `apply` or `invoke`: of a declared function, a closure or
    /// function reference, or a library function, which enters a frame
    /// when it runs its body.
    Call(NestedCallee),
    /// A library call inside an expression of a function that runs its
    /// body here: it enters a frame, as a call does, but it is no action,
    /// so no occurrence of its site is counted.
    Body(LibId),
    Perform {
        slot: lash_vocabulary::names::ImportSlot,
        result: Type,
        then_yield: bool,
    },

    Spawn(NestedCallee),
    Join,
    JoinMany(JoinMode),
}

impl NestedKind {
    /// Whether the task takes one more turn once the action's outcome is
    /// in (`K-TASK-005`).
    pub(crate) fn then_yield(&self) -> bool {
        matches!(
            self,
            Self::Perform {
                then_yield: true,
                ..
            }
        )
    }
}

/// What a nested call or `spawn` runs.
pub(crate) enum NestedCallee {
    Import(lash_vocabulary::names::ImportSlot, Option<LibId>),
    Declared(CodeId),
    Library(LibId),
    /// A document's body-helper call retains its typed boundary.
    CheckedHelper(LibId),
    /// A closure or function reference held in a variable, read into the
    /// action's first register before its arguments (`K-EVAL-004`): a role
    /// of the actions in them, named by the action's own site.
    Read,
}

/// A charge region an op closes: the reference charges it whole there,
/// and an exit before it resumes at its head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Close {
    pub(crate) region: u32,
    pub(crate) total: u32,
    /// The region's head op.
    pub(crate) head: u32,
}

/// The charge regions an op closes, in the order their heads stand. A
/// layout that splits a region heads the next one with nothing charged
/// between them, so one op may close several.
pub(crate) type Closes = Box<[Close]>;

/// The fixed units `closes` charge.
pub(crate) fn units(closes: &[Close]) -> u64 {
    closes.iter().map(|close| u64::from(close.total)).sum()
}

/// A nested action's resume entry, as an invoke of it reads it.
#[derive(Clone, Copy)]
pub(crate) struct Invoke {
    pub(crate) checked: bool,
    pub(crate) act: ActIx,
    pub(crate) occurrence: usize,
    /// It stands in a guest's code rather than a library body's.
    pub(crate) guest: bool,
    /// It is a call (`NestedKind::Call`) rather than a library body's run.
    pub(crate) call: bool,
    /// It is an operator's call (`Nested::operator`).
    pub(crate) operator: bool,
}

impl Invoke {
    /// The occurrence its call counts (`K-EFF-008`): a guest's call's.
    pub(crate) fn counted(&self) -> Option<usize> {
        (self.call && self.guest).then_some(self.occurrence)
    }
}

/// A value a statement computes, read from its ops.
#[derive(Clone)]
pub(crate) struct Expr {
    /// The op that computes it: where it charges, and where the reference
    /// resumes when a tier cannot run that op. An expression local's and a
    /// pin's are their value's: the op whose value they hold.
    pub(crate) pc: u32,
    /// The register its value goes to.
    pub(crate) reg: u32,
    pub(crate) kind: Kind,
}

#[derive(Clone)]
pub(crate) enum Kind {
    /// The value of a statement-root invoke (`Op::Act`), in the first
    /// register of its continuation.
    Act,
    /// A variable read: `scoped` when it reads an expression local open
    /// around it, `miss` where an exit at it resumes.
    Local {
        local: LocalRef,
        scoped: bool,
        miss: u32,
    },
    /// A session binding read: `miss` is where an exit at it resumes.
    Session { name: NameIx, miss: u32 },
    /// A resident constant.
    Const(Value),
    /// A library function its prepared entry answers: an operator
    /// (`Op::Operator`), or a nested invoke of an operator or a helper.
    Operator {
        lib: LibId,
        invoke: Option<Invoke>,
        args: Box<[Expr]>,
        closes: Closes,
    },
    /// A general call of a native (`Op::Call`).
    Native {
        lib: LibId,
        args: Box<[Expr]>,
        closes: Closes,
    },
    /// A guest call of a closure its callee reads, or of a declared
    /// function.
    Call {
        invoke: Invoke,
        callee: Callee,
        args: Box<[Expr]>,
        closes: Closes,
        /// The call site's place among the executable's closure call
        /// sites, which island formation numbers; `u32::MAX` until then.
        site: u32,
    },
    /// A tuple, list, set or map of its members: `miss` is where an exit
    /// at it resumes.
    Make {
        make: Make,
        items: Box<[Expr]>,
        miss: u32,
    },
    /// A record of the fields `fields` names.
    Record {
        fields: FieldListIx,
        values: Box<[Expr]>,
    },
    /// `target.field`: `miss` is where an exit at it resumes.
    Field {
        target: Box<Expr>,
        field: FieldIx,
        miss: u32,
    },
    /// `target[index]`: `miss` is where an exit at it resumes.
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
        site: SiteIx,
        miss: u32,
    },
    /// A value its statement still uses, pinned for it.
    Pin(Box<Expr>),
    /// `if(cond, then, otherwise)`, its test closing `closes`.
    Cond {
        cond: Box<Expr>,
        closes: Closes,
        then: Arm,
        otherwise: Arm,
    },
    /// `let(local = value, body)`: `miss` is where an exit at its binding
    /// resumes.
    Let {
        local: LocalRef,
        value: Box<Expr>,
        body: Box<Expr>,
        miss: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Make {
    Tuple,
    List,
    Set,
    Map,
}

#[derive(Clone)]
pub(crate) enum Callee {
    /// The closure its first operand reads.
    Read(Box<Expr>),
    Declared(CodeId),
}

/// A conditional expression's arm.
#[derive(Clone)]
pub(crate) struct Arm {
    /// Its first op: where taking it exits when it is not run.
    pub(crate) start: u32,
    /// Its value, computed into the conditional's register; `None` where
    /// its ops are no expression of the tree, or one a plan does not run.
    pub(crate) value: Option<Box<Expr>>,
    /// The regions its end closes.
    pub(crate) closes: Closes,
}

impl Arm {
    /// The fixed units it charges when taken: its regions, outside its own
    /// conditionals' arms.
    pub(crate) fn fixed(&self) -> u64 {
        units(&self.closes) + self.value.as_ref().map_or(0, |value| value.fixed())
    }
}

impl Expr {
    /// Its operands, in the order the reference evaluates them: a
    /// conditional's is its test, whose arms follow it.
    pub(crate) fn operands(&self) -> impl Iterator<Item = &Expr> {
        let (first, second, rest): (Option<&Expr>, Option<&Expr>, &[Expr]) = match &self.kind {
            Kind::Act | Kind::Local { .. } | Kind::Session { .. } | Kind::Const(_) => {
                (None, None, &[])
            }
            Kind::Operator { args, .. } | Kind::Native { args, .. } => (None, None, args),
            Kind::Call { callee, args, .. } => match callee {
                Callee::Read(read) => (Some(read), None, args),
                Callee::Declared(_) => (None, None, args),
            },
            Kind::Make { items, .. } => (None, None, items),
            Kind::Record { values, .. } => (None, None, values),
            Kind::Field { target, .. } => (Some(target), None, &[]),
            Kind::Index { target, index, .. } => (Some(target), Some(index), &[]),
            Kind::Pin(value) => (Some(value), None, &[]),
            Kind::Cond { cond, .. } => (Some(cond), None, &[]),
            Kind::Let { value, body, .. } => (Some(value), Some(body), &[]),
        };
        first.into_iter().chain(second).chain(rest)
    }

    /// The regions it closes where it charges.
    pub(crate) fn closes(&self) -> &[Close] {
        match &self.kind {
            Kind::Operator { closes, .. }
            | Kind::Native { closes, .. }
            | Kind::Call { closes, .. }
            | Kind::Cond { closes, .. } => closes,
            _ => &[],
        }
    }

    /// The fixed units the regions it and its operands close charge,
    /// outside its conditionals' arms.
    pub(crate) fn fixed(&self) -> u64 {
        units(self.closes()) + self.operands().map(Self::fixed).sum::<u64>()
    }
}

/// A statement read as expressions, up to the op that ends it.
pub(crate) struct Statement {
    /// The op that ends it.
    pub(crate) last: Op,
    /// The regions its last op closes.
    pub(crate) closes: Closes,
    regs: Vec<Option<Expr>>,
}

impl Statement {
    /// The expression register `reg` holds, taken.
    pub(crate) fn take(&mut self, reg: Reg) -> Option<Expr> {
        self.regs.get_mut(reg.ix())?.take()
    }

    /// Whether every value it computes reached its end: no register holds
    /// one its last op did not take.
    pub(crate) fn spent(&self) -> bool {
        self.regs.iter().all(Option::is_none)
    }
}

/// The ops of statement `range` of `stream` read as expressions, up to the
/// op that ends it: a statement-root invoke's continuation when `rooted`,
/// whose first op is the invoke (`Op::Act`) and whose value is in the first
/// register. `None` where the ops are no expressions the tree reads.
pub(crate) fn statement(stream: &Stream, range: OpRange, rooted: bool) -> Option<Statement> {
    let end = range.end().checked_sub(1)?;
    let last = *stream.ops.get(end as usize)?;
    let mut reader = Reader::new(stream, Vec::new());
    let mut start = range.start;
    if rooted {
        if !matches!(stream.ops.get(start as usize), Some(Op::Act)) {
            return None;
        }
        reader.define(
            Reg(0),
            Expr {
                pc: start,
                reg: 0,
                kind: Kind::Act,
            },
        )?;
        start += 1;
    }
    reader.ops(start, end)?;
    if !reader.lets.is_empty() {
        return None;
    }
    Some(Statement {
        last,
        closes: reader.open.into(),
        regs: reader.regs,
    })
}

/// Where the arms of the conditional whose `JumpIfFalse` at `pc` skips
/// `skip` ops end: the then-arm runs to its `Jump` before the first, the
/// else-arm from there to the second.
pub(crate) fn arm_ends(stream: &Stream, pc: u32, skip: u32) -> Option<(u32, u32)> {
    let then_end = pc.checked_add(1)?.checked_add(skip)?;
    let Op::Jump { skip: rest } = *stream.ops.get(then_end.checked_sub(1)? as usize)? else {
        return None;
    };
    Some((then_end, then_end.checked_add(rest)?))
}

/// The reading of one run of ops: a statement's, or an arm's.
struct Reader<'s> {
    stream: &'s Stream,
    /// The expression each register holds and no op has taken.
    regs: Vec<Option<Expr>>,
    /// The regions headed and not yet closed.
    open: Vec<Close>,
    /// Each expression local this run bound and has not ended: its
    /// variable, its register, its value and where an exit at its binding
    /// resumes.
    lets: Vec<(LocalRef, u32, Expr, u32)>,
    /// The frame positions of the expression locals open around the ops
    /// read, outermost first: an arm sees its conditional's.
    scope: Vec<u32>,
}

impl<'s> Reader<'s> {
    fn new(stream: &'s Stream, scope: Vec<u32>) -> Self {
        Self {
            stream,
            regs: Vec::new(),
            open: Vec::new(),
            lets: Vec::new(),
            scope,
        }
    }

    /// Register `at` gets `expr`; `None` when it holds one still untaken.
    fn define(&mut self, at: Reg, expr: Expr) -> Option<()> {
        let at = at.ix();
        if self.regs.len() <= at {
            self.regs.resize_with(at + 1, || None);
        }
        if self.regs[at].is_some() {
            return None;
        }
        self.regs[at] = Some(expr);
        Some(())
    }

    fn take(&mut self, at: Reg) -> Option<Expr> {
        self.regs.get_mut(at.ix())?.take()
    }

    fn operands(&mut self, args: RegRange) -> Option<Box<[Expr]>> {
        args.regs().map(|at| self.take(at)).collect()
    }

    /// The regions an op that charges here closes.
    fn close(&mut self) -> Closes {
        std::mem::take(&mut self.open).into()
    }

    /// Where an exit at op `pc` resumes: the head of the region it stands
    /// in, which nothing has charged yet.
    fn miss(&self, pc: u32) -> u32 {
        self.open.first().map_or(pc, |open| open.head)
    }

    /// Reads ops `start..end`.
    fn ops(&mut self, start: u32, end: u32) -> Option<()> {
        let stream = self.stream;
        let mut pc = start;
        while pc < end {
            let op = *stream.ops.get(pc as usize)?;
            let (reg, kind) = match op {
                Op::Region { region, total } => {
                    if total == u32::MAX {
                        return None;
                    }
                    self.open.push(Close {
                        region: region.0,
                        total,
                        head: pc,
                    });
                    pc += 1;
                    continue;
                }
                Op::Local { dst, local } => (
                    dst,
                    Kind::Local {
                        local,
                        scoped: self.scope.contains(&local.at),
                        miss: self.miss(pc),
                    },
                ),
                Op::Session { dst, name } => (
                    dst,
                    Kind::Session {
                        name,
                        miss: self.miss(pc),
                    },
                ),
                Op::Const { dst, c } => match stream.consts.get(c.ix())? {
                    Constant::Resident(value) => (dst, Kind::Const(*value)),
                    Constant::Deferred(_) => return None,
                },
                Op::Operator { lib, args } => (
                    args.start,
                    Kind::Operator {
                        lib: LibId(lib.0),
                        invoke: None,
                        args: self.operands(args)?,
                        closes: self.close(),
                    },
                ),
                Op::Call { lib, args } => (
                    args.start,
                    Kind::Native {
                        lib: LibId(lib.0),
                        args: self.operands(args)?,
                        closes: self.close(),
                    },
                ),
                Op::Nested { act, args } => (args.start, self.nested(act, args)?),
                Op::MakeTuple { items } => (items.start, self.make(Make::Tuple, items, pc)?),
                Op::MakeList { items } => (items.start, self.make(Make::List, items, pc)?),
                Op::MakeSet { items } => (items.start, self.make(Make::Set, items, pc)?),
                Op::MakeMap { entries } => (entries.start, self.make(Make::Map, entries, pc)?),
                Op::MakeRecord { fields, values } => (
                    values.start,
                    Kind::Record {
                        fields,
                        values: self.operands(values)?,
                    },
                ),
                Op::Field { at, field } => (
                    at,
                    Kind::Field {
                        target: Box::new(self.take(at)?),
                        field,
                        miss: self.miss(pc),
                    },
                ),
                Op::Index { at, site } => {
                    let target = Box::new(self.take(at)?);
                    let index = Box::new(self.take(Reg(at.0 + 1))?);
                    let miss = self.miss(pc);
                    (
                        at,
                        Kind::Index {
                            target,
                            index,
                            site,
                            miss,
                        },
                    )
                }
                Op::Pin { src } => {
                    let value = self.take(src)?;
                    let expr = Expr {
                        pc: value.pc,
                        reg: src.0,
                        kind: Kind::Pin(Box::new(value)),
                    };
                    self.define(src, expr)?;
                    pc += 1;
                    continue;
                }
                Op::LetBind { local, src } => {
                    let value = self.take(src)?;
                    let miss = self.miss(pc);
                    self.lets.push((local, src.0, value, miss));
                    self.scope.push(local.at);
                    pc += 1;
                    continue;
                }
                // The value its body computes, in the same register, is the
                // expression's.
                Op::LetEnd { local } => {
                    let (bound, reg, value, miss) = self.lets.pop()?;
                    self.scope.pop();
                    if bound.at != local.at {
                        return None;
                    }
                    let body = self.take(Reg(reg))?;
                    let expr = Expr {
                        pc: body.pc,
                        reg,
                        kind: Kind::Let {
                            local: bound,
                            value: Box::new(value),
                            body: Box::new(body),
                            miss,
                        },
                    };
                    self.define(Reg(reg), expr)?;
                    pc += 1;
                    continue;
                }
                Op::JumpIfFalse { cond, skip } => {
                    let closes = self.close();
                    let test = self.take(cond)?;
                    let (then_end, else_end) = arm_ends(stream, pc, skip)?;
                    let then = self.arm(pc + 1, then_end - 1, cond);
                    let otherwise = self.arm(then_end, else_end, cond);
                    let expr = Expr {
                        pc,
                        reg: cond.0,
                        kind: Kind::Cond {
                            cond: Box::new(test),
                            closes,
                            then,
                            otherwise,
                        },
                    };
                    self.define(cond, expr)?;
                    pc = else_end;
                    continue;
                }
                _ => return None,
            };
            self.define(
                reg,
                Expr {
                    pc,
                    reg: reg.0,
                    kind,
                },
            )?;
            pc += 1;
        }
        Some(())
    }

    fn make(&mut self, make: Make, items: RegRange, pc: u32) -> Option<Kind> {
        Some(Kind::Make {
            make,
            items: self.operands(items)?,
            miss: self.miss(pc),
        })
    }

    /// The invoke of nested action `act` on the registers `args`: an
    /// operator's or a helper's, answered by its prepared entry, or a guest
    /// call of a closure, the first operand, or of a declared function.
    fn nested(&mut self, act: ActIx, args: RegRange) -> Option<Kind> {
        let nested = self.stream.nested.get(act.ix())?;
        let invoke = Invoke {
            act,
            occurrence: nested.occurrence,
            guest: nested.guest,
            call: matches!(nested.kind, NestedKind::Call(_)),
            operator: nested.operator,
            checked: matches!(
                nested.kind,
                NestedKind::Call(NestedCallee::Import(..) | NestedCallee::CheckedHelper(_))
            ),
        };
        if let Some(lib) = nested.kind.operator_lib() {
            return Some(Kind::Operator {
                lib,
                invoke: Some(invoke),
                args: self.operands(args)?,
                closes: self.close(),
            });
        }
        let declared = match nested.kind {
            NestedKind::Call(NestedCallee::Read) if args.len >= 1 => None,
            NestedKind::Call(NestedCallee::Import(..)) => return None,
            NestedKind::Call(NestedCallee::Declared(code)) => Some(code),
            _ => return None,
        };
        let mut operands = self.operands(args)?.into_vec();
        let callee = match declared {
            Some(code) => Callee::Declared(code),
            None => Callee::Read(Box::new(operands.remove(0))),
        };
        Some(Kind::Call {
            invoke,
            callee,
            args: operands.into(),
            closes: self.close(),
            site: u32::MAX,
        })
    }

    /// The arm of ops `start..end` of a conditional whose value goes to
    /// `dst`: its value when its ops define that alone and end every
    /// expression local they bind.
    fn arm(&self, start: u32, end: u32, dst: Reg) -> Arm {
        let mut reader = Reader::new(self.stream, self.scope.clone());
        let value = reader.ops(start, end).and_then(|()| {
            let value = reader.take(dst)?;
            (reader.regs.iter().all(Option::is_none) && reader.lets.is_empty())
                .then(|| Box::new(value))
        });
        let closes = match value {
            Some(_) => reader.open.into(),
            None => Closes::default(),
        };
        Arm {
            start,
            value,
            closes,
        }
    }
}
