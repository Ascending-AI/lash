//! Call islands at lowering (`docs/design/kernel-regions.md` §1, §5 R4): a
//! function or closure body whose statements are region statements and
//! calls of region functions, which a call runs as virtual frames instead
//! of machine frames.
//!
//! An [`IslandPlan`] states the body's shape: per statement its kind and
//! step edge, per expression its ops in evaluation order with the charge
//! region each closes, the registers it holds while the next operand runs,
//! and the op the reference resumes at when it exits there. Lowering admits
//! a plan only when [`IslandPlan::verify`] finds every charge region it
//! closes stated as the stream's `RegionTable` sums it and every counted
//! site at the occurrence the stream numbers. Plans belong to the
//! executable: deterministic, immutable and never saved. Their compiled
//! form is built on first entry and cached in the plan.
//!
//! Island ops: reads of unshared slots and of captures, resident constants
//! an island frame may hold ([`held`]), prepared operators (`Op::Operator`,
//! or `Op::Nested` of an operator), helper calls whose prepared entry has a
//! scalar table (`call lib`, counted), general calls of natives that build
//! nothing (`Op::Call`), reads of a list's or a tuple's items, calls of
//! closures and declared functions (counted), conditional expressions and
//! expression locals. A
//! conditional arm of other ops is cold: taking it exits there. Every other
//! op is a barrier, which no plan contains. Island statements are `return`,
//! `set` and `let` of unshared variables, `if`, and `do`, which discards
//! its value.

use std::sync::OnceLock;

use lash_kernel_doc::{Unit, Value};
use lash_kernel_exec::{ActIx, Op, OpRange, Reg, RegRange};

use super::{
    BlockId, CodeId, Constant, Executable, Form, Lib, LibId, LibRun, NestedCallee, NestedKind,
    Stmt, StmtId, Tables,
};
use crate::machine::islands::Compiled;

/// The most variables an island frame holds.
pub(crate) const MAX_SLOTS: usize = 12;

/// The most the census prices a value an island frame holds where it is
/// held: a short text's 16 bytes and its at most 14, word-rounded.
pub(crate) const HELD_BYTES: u64 = 32;

/// Whether an island frame may hold `value` in a variable or a register:
/// a value that owns no storage the island would make or admit, priced at
/// most [`HELD_BYTES`] wherever it is held. Scalars, short texts and
/// function names, tasks, and mutable objects by identity, whose own
/// storage is held where it was made. Any other value exits before it is
/// bound, written or passed, and the reference admits it.
#[inline(always)]
pub(crate) fn held(value: &Value) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Absent
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Function(_)
            | Value::Task(_)
            | Value::TaskRef(_)
            | Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Record(_)
            | Value::Closure(_)
            | Value::ObjectRef(_)
    )
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

/// An island expression, in the order the reference runs its ops.
#[derive(Clone)]
pub(crate) enum Expr {
    /// A variable read: `miss` is where the reference resumes when the
    /// variable is unbound, the head of the region it stands in.
    Local {
        at: u32,
        miss: u32,
    },
    Const(Value),
    /// A prepared operator, or a helper call (`counted`, at its
    /// occurrence) answered by its prepared entry.
    Operator {
        lib: LibId,
        args: Box<[Expr]>,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        counted: Option<usize>,
    },
    /// A general call of a native (`Op::Call`), answered without building
    /// anything.
    Native {
        lib: LibId,
        args: Box<[Expr]>,
        close: Option<Close>,
        pc: u32,
        reg: u32,
    },
    /// `target[index]` of a list or a tuple: `miss` is where the reference
    /// resumes when the island cannot read it, the head of the region it
    /// stands in.
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
        reg: u32,
        miss: u32,
    },
    /// A call of a closure read first, or of a declared function.
    Call {
        callee: Callee,
        args: Box<[Expr]>,
        close: Option<Close>,
        pc: u32,
        /// The callee's register, or the first argument's.
        reg: u32,
        act: ActIx,
        occurrence: usize,
        /// The call site's place among the executable's closure call sites.
        site: u32,
    },
    /// `if(cond, then, else)`.
    Cond {
        cond: Box<Expr>,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        then_: Arm,
        else_: Arm,
    },
    /// An expression local, `let(at = value, body)`: `miss` is where the
    /// reference resumes when the island cannot bind it.
    Let {
        value: Box<Expr>,
        at: u32,
        reg: u32,
        miss: u32,
        body: Box<Expr>,
    },
}

#[derive(Clone)]
pub(crate) enum Callee {
    Read(Box<Expr>),
    Declared(CodeId),
}

/// A conditional expression's arm.
#[derive(Clone)]
pub(crate) enum Arm {
    /// Its value, then the region its end closes.
    Hot {
        value: Box<Expr>,
        close: Option<Close>,
    },
    /// An arm of other ops: taking it exits at its first op.
    Cold { pc: u32 },
}

/// An island statement.
#[derive(Clone)]
pub(crate) struct Statement {
    pub(crate) id: StmtId,
    /// The blocks its step leaves when it completes in its own block.
    pub(crate) leaves: u32,
    pub(crate) kind: Kind,
}

#[derive(Clone)]
pub(crate) enum Kind {
    Return {
        value: Expr,
        close: Option<Close>,
        reg: u32,
    },
    /// `set` (`bind` false) or `let` of an unshared variable.
    Write {
        value: Expr,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        at: u32,
        bind: bool,
    },
    If {
        cond: Expr,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        then_: Block,
        else_: Block,
    },
    /// `do`: a value the statement computes in its first register and
    /// discards.
    Discard {
        value: Expr,
        close: Option<Close>,
        reg: u32,
    },
}

#[derive(Clone)]
pub(crate) struct Block {
    pub(crate) id: BlockId,
    pub(crate) declares: Box<[u32]>,
    pub(crate) statements: Box<[Statement]>,
}

/// A body that runs as an island.
pub(crate) struct IslandPlan {
    pub(crate) code: CodeId,
    /// Where each parameter sits, in order.
    pub(crate) params: Box<[u32]>,
    /// Where each captured variable sits, in the closure's cell order.
    pub(crate) captures: Box<[u32]>,
    pub(crate) slots: usize,
    pub(crate) body: Block,
    /// Its counted sites' occurrences (`K-EFF-008`), in op order.
    pub(crate) calls: Box<[usize]>,
    compiled: OnceLock<Option<Compiled>>,
}

/// Why lowering does not admit a formed plan.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A region an op closes is not stated as its table sums it.
    Region { region: u32 },
    /// A counted site is not at the occurrence the stream numbers.
    Calls,
}

/// The island plans lowering formed, by code.
#[derive(Default)]
pub(crate) struct Islands {
    pub(crate) plans: Vec<IslandPlan>,
    /// Each document code's plan, by its place among the document's codes.
    pub(crate) by_code: Vec<Option<u32>>,
    /// How many closure call sites the plans hold.
    pub(crate) sites: usize,
}

impl Executable {
    /// The island plan of code `code`, by its index.
    #[inline]
    pub(crate) fn island_of(&self, code: CodeId) -> Option<u32> {
        let own = (code.0 as usize).checked_sub(self.code_base)?;
        self.islands.by_code.get(own).copied().flatten()
    }
}

/// Forms and verifies the island of every document code of `tables` that
/// has one, against `libs`, and marks the nested calls that may enter
/// one. `codes`, `blocks` and `stmts` are where the document's own ids
/// start.
pub(super) fn form(tables: &mut Tables, libs: &[Lib], base: (u32, u32, u32)) -> Islands {
    let (codes, blocks, stmts) = base;
    let mut islands = Islands {
        by_code: vec![None; tables.codes.len()],
        ..Islands::default()
    };
    for (index, code) in tables.codes.iter().enumerate() {
        if !code.charging
            || !code.charged
            || !code.free.is_empty()
            || code.slots_by_at.len() > MAX_SLOTS
            || code.params.iter().any(|param| param.shared)
        {
            continue;
        }
        // An island writes only unshared variables, and reads those and the
        // captured ones: a shared variable of its own would bind a cell.
        let writable: Vec<bool> = code.slots.iter().map(|info| !info.shared).collect();
        let mut readable = writable.clone();
        for capture in &code.captures {
            readable[capture.inner as usize] = true;
        }
        let mut former = Former {
            tables,
            libs,
            blocks,
            stmts,
            calls: Vec::new(),
            sites: islands.sites as u32,
            readable,
            writable,
        };
        let Some(body) = former.block(code.body) else {
            continue;
        };
        let plan = IslandPlan {
            code: CodeId(codes + index as u32),
            params: code.params.iter().map(|param| param.at).collect(),
            captures: code
                .captures
                .iter()
                .map(|capture| capture.inner_at)
                .collect(),
            slots: code.slots_by_at.len(),
            body,
            calls: former.calls.into(),
            compiled: OnceLock::new(),
        };
        if plan.verify(&tables.stream).is_ok() {
            islands.sites = former.sites as usize;
            islands.by_code[index] = Some(islands.plans.len() as u32);
            islands.plans.push(plan);
        }
    }
    if !islands.plans.is_empty() {
        mark(tables, &islands, codes);
    }
    islands
}

/// Marks each guest call of a closure, or of a declared function with an
/// island, as one that may enter an island.
fn mark(tables: &mut Tables, islands: &Islands, codes: u32) {
    for nested in &mut tables.stream.nested {
        if matches!(nested.site.unit, Unit::Library(_)) {
            continue;
        }
        nested.island = match nested.kind {
            NestedKind::Call(NestedCallee::Read) => true,
            NestedKind::Call(NestedCallee::Declared(code)) => (code.0 as usize)
                .checked_sub(codes as usize)
                .and_then(|own| islands.by_code.get(own).copied().flatten())
                .is_some(),
            _ => false,
        };
    }
}

/// The walk that forms one code's plan.
struct Former<'a> {
    tables: &'a Tables,
    libs: &'a [Lib],
    blocks: u32,
    stmts: u32,
    calls: Vec<usize>,
    sites: u32,
    /// Which slots the code's island may read: unshared ones and captures.
    readable: Vec<bool>,
    /// Which slots it may bind and write: unshared ones.
    writable: Vec<bool>,
}

/// The expressions a statement's ops define, by register, and the charge
/// region open where the walk stands.
#[derive(Default)]
struct Walk {
    regs: Vec<Option<Expr>>,
    open: Option<Close>,
    /// Each expression local bound and not yet ended: its slot, register
    /// and value, and where an exit at its binding resumes.
    lets: Vec<(u32, u32, Expr, u32)>,
}

impl Walk {
    fn define(&mut self, at: Reg, expr: Expr) -> Option<()> {
        let at = at.0 as usize;
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
        self.regs.get_mut(at.0 as usize)?.take()
    }

    fn operands(&mut self, args: RegRange) -> Option<Vec<Expr>> {
        (args.start.0..args.start.0 + args.len)
            .map(|at| self.take(Reg(at)))
            .collect()
    }

    /// Where an exit at in-region op `pc` resumes: the head of the region
    /// it stands in, which nothing has charged yet.
    fn miss(&self, pc: u32) -> u32 {
        self.open.map_or(pc, |open| open.head)
    }

    fn held(&self) -> bool {
        self.regs.iter().any(Option::is_some) || !self.lets.is_empty()
    }
}

impl Former<'_> {
    fn block(&mut self, id: BlockId) -> Option<Block> {
        let block = &self.tables.blocks[(id.0 - self.blocks) as usize];
        let statements = block
            .stmts
            .iter()
            .map(|stmt| self.statement(*stmt))
            .collect::<Option<_>>()?;
        Some(Block {
            id,
            declares: block.declares.clone().into(),
            statements,
        })
    }

    fn statement(&mut self, id: StmtId) -> Option<Statement> {
        let at = (id.0 - self.stmts) as usize;
        let statement: &Stmt = &self.tables.stmts[at];
        let leaves = self.tables.edges[at].leaves;
        let OpRange { start, len } = statement.ops;
        let end = start.checked_add(len)?.checked_sub(1)?;
        let mut walk = Walk::default();
        self.ops(&mut walk, start, end)?;
        let terminal = *self.tables.stream.ops.get(end as usize)?;
        let close = walk.open.take();
        let kind = match (&statement.form, terminal) {
            (Form::Plain, Op::Return { src }) => Kind::Return {
                value: walk.take(src)?,
                close,
                reg: src.0,
            },
            (Form::Plain, Op::WriteLocal { local, src } | Op::Bind { local, src })
                if self.unshared(local.slot) =>
            {
                Kind::Write {
                    value: walk.take(src)?,
                    close,
                    pc: end,
                    reg: src.0,
                    at: local.at,
                    bind: matches!(terminal, Op::Bind { .. }),
                }
            }
            (
                Form::If {
                    then_block,
                    else_block,
                },
                Op::Branch { cond, then_, else_ },
            ) if then_.0 == then_block.0 && else_.0 == else_block.0 => {
                let (then_block, else_block) = (*then_block, *else_block);
                Kind::If {
                    cond: walk.take(cond)?,
                    close,
                    pc: end,
                    reg: cond.0,
                    then_: self.block(then_block)?,
                    else_: self.block(else_block)?,
                }
            }
            (Form::Plain, Op::End) => Kind::Discard {
                value: walk.take(Reg(0))?,
                close,
                reg: 0,
            },
            _ => return None,
        };
        // Every value the statement computes reaches its end.
        if walk.held() {
            return None;
        }
        Some(Statement { id, leaves, kind })
    }

    /// Whether slot `slot` is one an island binds and writes.
    fn unshared(&self, slot: u32) -> bool {
        self.writable.get(slot as usize).copied().unwrap_or(false)
    }

    /// Walks ops `start..end` into `walk`.
    fn ops(&mut self, walk: &mut Walk, start: u32, end: u32) -> Option<()> {
        let stream = &self.tables.stream;
        let mut pc = start;
        while pc < end {
            let op = *stream.ops.get(pc as usize)?;
            match op {
                Op::Region { region, total } => {
                    // A head closes the region before it with nothing
                    // charged between: the walk keeps one open region.
                    if walk.open.is_some() || total == u32::MAX {
                        return None;
                    }
                    walk.open = Some(Close {
                        region: region.0,
                        total,
                        head: pc,
                    });
                }
                Op::Local { dst, local } => {
                    if !self.readable.get(local.slot as usize).copied()? {
                        return None;
                    }
                    let miss = walk.miss(pc);
                    walk.define(dst, Expr::Local { at: local.at, miss })?;
                }
                Op::Const { dst, c } => match stream.consts.get(c.ix())? {
                    Constant::Resident(value) if held(value) => {
                        walk.define(dst, Expr::Const(*value))?;
                    }
                    _ => return None,
                },
                Op::Operator { lib, args } => {
                    let lib = LibId(lib.0);
                    self.operator(lib, args.len as usize, false)?;
                    let operands = walk.operands(args)?;
                    let close = walk.open.take();
                    walk.define(
                        args.start,
                        Expr::Operator {
                            lib,
                            args: operands.into(),
                            close,
                            pc,
                            reg: args.start.0,
                            counted: None,
                        },
                    )?;
                }
                Op::Call { lib, args } => {
                    let lib = LibId(lib.0);
                    self.native(lib, args.len as usize)?;
                    let operands = walk.operands(args)?;
                    let close = walk.open.take();
                    walk.define(
                        args.start,
                        Expr::Native {
                            lib,
                            args: operands.into(),
                            close,
                            pc,
                            reg: args.start.0,
                        },
                    )?;
                }
                Op::Index { at, .. } => {
                    let target = walk.take(at)?;
                    let index = walk.take(Reg(at.0 + 1))?;
                    let miss = walk.miss(pc);
                    walk.define(
                        at,
                        Expr::Index {
                            target: Box::new(target),
                            index: Box::new(index),
                            reg: at.0,
                            miss,
                        },
                    )?;
                }
                Op::Nested { act, args } => {
                    let nested = stream.nested.get(act.ix())?;
                    if matches!(nested.site.unit, Unit::Library(_)) {
                        return None;
                    }
                    let expr = match nested.kind {
                        NestedKind::Body(lib) if nested.operator => {
                            self.operator(lib, args.len as usize, false)?;
                            Expr::Operator {
                                lib,
                                args: walk.operands(args)?.into(),
                                close: walk.open.take(),
                                pc,
                                reg: args.start.0,
                                counted: None,
                            }
                        }
                        NestedKind::Call(NestedCallee::Library(lib)) => {
                            self.operator(lib, args.len as usize, true)?;
                            self.calls.push(nested.occurrence);
                            Expr::Operator {
                                lib,
                                args: walk.operands(args)?.into(),
                                close: walk.open.take(),
                                pc,
                                reg: args.start.0,
                                counted: Some(nested.occurrence),
                            }
                        }
                        NestedKind::Call(NestedCallee::Read) if args.len >= 1 => {
                            let mut operands = walk.operands(args)?.into_iter();
                            let callee = operands.next()?;
                            self.calls.push(nested.occurrence);
                            let site = self.sites;
                            self.sites += 1;
                            Expr::Call {
                                callee: Callee::Read(Box::new(callee)),
                                args: operands.collect(),
                                close: walk.open.take(),
                                pc,
                                reg: args.start.0,
                                act,
                                occurrence: nested.occurrence,
                                site,
                            }
                        }
                        NestedKind::Call(NestedCallee::Declared(code)) => {
                            self.calls.push(nested.occurrence);
                            Expr::Call {
                                callee: Callee::Declared(code),
                                args: walk.operands(args)?.into(),
                                close: walk.open.take(),
                                pc,
                                reg: args.start.0,
                                act,
                                occurrence: nested.occurrence,
                                site: u32::MAX,
                            }
                        }
                        _ => return None,
                    };
                    walk.define(args.start, expr)?;
                }
                Op::JumpIfFalse { cond, skip } => {
                    let close = walk.open.take();
                    let condition = walk.take(cond)?;
                    let then_end = pc.checked_add(1)?.checked_add(skip)?;
                    let Op::Jump { skip: rest } = *stream.ops.get(then_end as usize - 1)? else {
                        return None;
                    };
                    let else_end = then_end.checked_add(rest)?;
                    let then_ = self.arm(pc + 1, then_end - 1, cond)?;
                    let else_ = self.arm(then_end, else_end, cond)?;
                    if matches!((&then_, &else_), (Arm::Cold { .. }, Arm::Cold { .. })) {
                        return None;
                    }
                    walk.define(
                        cond,
                        Expr::Cond {
                            cond: Box::new(condition),
                            close,
                            pc,
                            reg: cond.0,
                            then_,
                            else_,
                        },
                    )?;
                    pc = else_end;
                    continue;
                }
                Op::LetBind { local, src } if self.unshared(local.slot) => {
                    let value = walk.take(src)?;
                    let miss = walk.miss(pc);
                    walk.lets.push((local.at, src.0, value, miss));
                }
                Op::LetEnd { local } => {
                    let (at, reg, value, miss) = walk.lets.pop()?;
                    if at != local.at {
                        return None;
                    }
                    let body = walk.take(Reg(reg))?;
                    walk.define(
                        Reg(reg),
                        Expr::Let {
                            value: Box::new(value),
                            at,
                            reg,
                            miss,
                            body: Box::new(body),
                        },
                    )?;
                }
                _ => return None,
            }
            pc += 1;
        }
        Some(())
    }

    /// The arm of ops `start..end` of a conditional whose value goes to
    /// `dst`: hot when its ops are island ops that define only `dst`.
    fn arm(&mut self, start: u32, end: u32, dst: Reg) -> Option<Arm> {
        let calls = self.calls.len();
        let sites = self.sites;
        let mut walk = Walk::default();
        let hot = self.ops(&mut walk, start, end).and_then(|()| {
            let close = walk.open.take();
            let value = walk.take(dst)?;
            (!walk.held()).then(|| Arm::Hot {
                value: Box::new(value),
                close,
            })
        });
        Some(hot.unwrap_or_else(|| {
            self.calls.truncate(calls);
            self.sites = sites;
            Arm::Cold { pc: start }
        }))
    }

    /// Whether `lib` called with `given` arguments is a native an island
    /// calls through its general call: no operation, whose admission is
    /// the reference's, and no plan or body.
    fn native(&self, lib: LibId, given: usize) -> Option<()> {
        let function = self.libs.get(lib.0 as usize)?;
        (matches!(function.run, LibRun::Native { .. })
            && !function.operation()
            && given <= function.arity
            && function.arity <= MAX_SLOTS)
            .then_some(())
    }

    /// Whether `lib` called with `given` arguments answers through its
    /// prepared entry with a scalar charge table: an operator, or a helper
    /// call when `helper`.
    fn operator(&self, lib: LibId, given: usize, helper: bool) -> Option<()> {
        let function = self.libs.get(lib.0 as usize)?;
        let (entry, fast) = function.prepared.as_ref()?;
        fast.scalar()?;
        // An island holds immediates alone: an entry with no immediate
        // paths never answers it.
        if !entry.immediate() {
            return None;
        }
        if function.arity != given || !(1..=3).contains(&given) {
            return None;
        }
        // An operation whose body shares a parameter makes its cell at
        // entry: an allocation, which no island makes.
        if function.operation() && function.shares_params {
            return None;
        }
        // A helper whose body runs a plan or a machine function answers
        // through it, not its entry, under an observer only when its plan
        // is deferred; the island checks that at the call.
        if helper && matches!(function.run, LibRun::Machine(_)) {
            return None;
        }
        Some(())
    }
}

impl IslandPlan {
    /// Checks the plan against `stream`: every region an op closes is
    /// headed there with the total its `RegionSummary` gives, and every
    /// counted site is at the occurrence the stream numbers, in op order.
    pub(crate) fn verify(&self, stream: &super::Stream) -> Result<(), Refused> {
        let mut calls = Vec::new();
        verify_block(&self.body, stream, &mut calls)?;
        if *calls != *self.calls {
            return Err(Refused::Calls);
        }
        Ok(())
    }

    /// The plan as stated, uncompiled: what a law misstates.
    #[cfg(test)]
    pub(crate) fn copied(&self) -> Self {
        Self {
            code: self.code,
            params: self.params.clone(),
            captures: self.captures.clone(),
            slots: self.slots,
            body: self.body.clone(),
            calls: self.calls.clone(),
            compiled: OnceLock::new(),
        }
    }

    /// The plan's compiled form, built on its first entry.
    pub(crate) fn compiled(&self, exe: &Executable) -> Option<&Compiled> {
        self.compiled
            .get_or_init(|| Compiled::of(self, exe))
            .as_ref()
    }
}

fn verify_close(close: Option<&Close>, stream: &super::Stream) -> Result<(), Refused> {
    let Some(close) = close else {
        return Ok(());
    };
    let refused = Refused::Region {
        region: close.region,
    };
    let stated = stream
        .regions
        .summary(close.region)
        .map_err(|_| Refused::Region {
            region: close.region,
        })?;
    match stream.ops.get(close.head as usize) {
        Some(Op::Region { region, total })
            if region.0 == close.region
                && *total == close.total
                && stated.total() == u64::from(close.total) =>
        {
            Ok(())
        }
        _ => Err(refused),
    }
}

fn verify_block(
    block: &Block,
    stream: &super::Stream,
    calls: &mut Vec<usize>,
) -> Result<(), Refused> {
    for statement in &block.statements {
        match &statement.kind {
            Kind::Return { value, close, .. }
            | Kind::Write { value, close, .. }
            | Kind::Discard { value, close, .. } => {
                verify_expr(value, stream, calls)?;
                verify_close(close.as_ref(), stream)?;
            }
            Kind::If {
                cond,
                close,
                then_,
                else_,
                ..
            } => {
                verify_expr(cond, stream, calls)?;
                verify_close(close.as_ref(), stream)?;
                verify_block(then_, stream, calls)?;
                verify_block(else_, stream, calls)?;
            }
        }
    }
    Ok(())
}

fn verify_expr(expr: &Expr, stream: &super::Stream, calls: &mut Vec<usize>) -> Result<(), Refused> {
    match expr {
        Expr::Local { .. } | Expr::Const(_) => Ok(()),
        Expr::Operator {
            args,
            close,
            counted,
            pc,
            ..
        } => {
            for arg in args {
                verify_expr(arg, stream, calls)?;
            }
            verify_close(close.as_ref(), stream)?;
            if let Some(occurrence) = counted {
                match stream.ops.get(*pc as usize) {
                    Some(Op::Nested { act, .. })
                        if stream.nested.get(act.ix()).map(|nested| nested.occurrence)
                            == Some(*occurrence) => {}
                    _ => return Err(Refused::Calls),
                }
                calls.push(*occurrence);
            }
            Ok(())
        }
        Expr::Native { args, close, .. } => {
            for arg in args {
                verify_expr(arg, stream, calls)?;
            }
            verify_close(close.as_ref(), stream)
        }
        Expr::Index { target, index, .. } => {
            verify_expr(target, stream, calls)?;
            verify_expr(index, stream, calls)
        }
        Expr::Call {
            callee,
            args,
            close,
            act,
            occurrence,
            ..
        } => {
            if let Callee::Read(callee) = callee {
                verify_expr(callee, stream, calls)?;
            }
            for arg in args {
                verify_expr(arg, stream, calls)?;
            }
            verify_close(close.as_ref(), stream)?;
            if stream.nested.get(act.ix()).map(|nested| nested.occurrence) != Some(*occurrence) {
                return Err(Refused::Calls);
            }
            calls.push(*occurrence);
            Ok(())
        }
        Expr::Cond {
            cond,
            close,
            then_,
            else_,
            ..
        } => {
            verify_expr(cond, stream, calls)?;
            verify_close(close.as_ref(), stream)?;
            for arm in [then_, else_] {
                if let Arm::Hot { value, close } = arm {
                    verify_expr(value, stream, calls)?;
                    verify_close(close.as_ref(), stream)?;
                }
            }
            Ok(())
        }
        Expr::Let { value, body, .. } => {
            verify_expr(value, stream, calls)?;
            verify_expr(body, stream, calls)
        }
    }
}
