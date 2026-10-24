//! Call islands at lowering (`docs/design/kernel-regions.md` §1, §5 R4): a
//! function or closure body whose statements are region statements and
//! calls of region functions, which a call runs as virtual frames instead
//! of machine frames.
//!
//! An [`IslandPlan`] states the body's shape: per statement its kind and
//! step edge, and its expressions as the statement's resolved tree reads
//! them ([`super::expr`]): their ops in evaluation order with the charge
//! regions each closes, the registers each holds while the next operand
//! runs, and the op the reference resumes at when it exits there. Formation
//! admits the tree's island ops and makes a conditional arm of others cold;
//! no op of an island closes more than one region. Lowering admits
//! a plan only when [`IslandPlan::verify`] finds every charge region it
//! closes stated as the stream's `RegionTable` sums it and every counted
//! site at the occurrence the stream numbers. Plans belong to the
//! executable: deterministic, immutable and never saved. Their compiled
//! form is built on first entry and cached in the plan.
//!
//! The register program admits resident values, prepared operators and
//! heap helpers, allocating natives, constructors, member reads/writes,
//! guest calls, conditionals and expression locals. Heap operations borrow
//! the existing semantic handlers within a bounded nursery. Stores retain
//! their source operand order and ordered charge regions. Metered while,
//! direct-cursor for, break/continue/else and internal try/finally cleanup
//! share the instruction loop. Existing captured/free cells retain their
//! identities; fresh shared cells remain with their allocation owner.
//! Unsupported operations are cold and exit at their source operation.
//! Allocating region plans retain precise-exit and accumulator ownership.

use std::sync::OnceLock;

use lash_kernel_exec::{Op, Reg};

use super::expr::{self, Arm, Callee, Close, Expr, Kind as ExprKind, Make};
use super::{
    BlockId, CodeId, Executable, Form, Lib, LibId, LibRun, NestedCallee, NestedKind, Prepared,
    Stmt, StmtId, Tables,
};
use crate::machine::islands::Compiled;

/// The fixed padding of non-building native calls; guest arity is independent.
pub(crate) const MAX_ARGS: usize = 12;

/// The existing per-slot reservation. Every Value is an immediate or an
/// arena handle; the capacity meter counts its backing storage in the arena.
pub(crate) const HELD_BYTES: u64 = 32;

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
    Try {
        body: Block,
        catch: Option<Block>,
        finally: Option<Block>,
    },
    While {
        cond: Expr,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        cost: u64,
        metered: u64,
        body: Block,
        otherwise: Option<Block>,
    },
    For {
        value: Expr,
        close: Option<Close>,
        pc: u32,
        reg: u32,
        cost: u64,
        metered: u64,
        at: u32,
        body: Block,
        otherwise: Option<Block>,
    },
    Depart {
        close: Option<Close>,
        continuing: bool,
    },
    Return {
        value: Expr,
        close: Option<Close>,
        reg: u32,
    },
    /// `set` of a writable variable or `let` of an unshared variable.
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
    Store {
        operands: Box<[Expr]>,
        target: u32,
        index: Option<u32>,
        value: u32,
        field: Option<usize>,
        closes: Box<[Close]>,
        pc: u32,
    },
    /// A statement of an `if`'s block that no island runs: reaching it
    /// exits at its first op, which the reference runs as its step does.
    Cold,
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
    /// Verified scalar loop facts consumed by the register program only.
    pub(crate) scalar: Vec<super::regions::LoopPlan>,
    pub(crate) plans: Vec<IslandPlan>,
    /// Each document code's plan, by its place among the document's codes.
    pub(crate) by_code: Vec<Option<u32>>,
    /// How many closure call sites the plans hold.
    pub(crate) sites: usize,
}

impl Islands {
    /// Project loop membership from the verified source plans. A cold
    /// statement, including a cold loop, remains with its semantic owner.
    pub(super) fn owns_loop(&self, id: StmtId) -> bool {
        self.plans.iter().any(|plan| plan.body.owns_loop(id))
    }
}

impl Block {
    fn owns_loop(&self, id: StmtId) -> bool {
        self.statements
            .iter()
            .any(|statement| match &statement.kind {
                Kind::While {
                    body, otherwise, ..
                }
                | Kind::For {
                    body, otherwise, ..
                } => {
                    statement.id == id
                        || body.owns_loop(id)
                        || otherwise.as_ref().is_some_and(|block| block.owns_loop(id))
                }
                Kind::If { then_, else_, .. } => then_.owns_loop(id) || else_.owns_loop(id),
                Kind::Try {
                    body,
                    catch,
                    finally,
                } => {
                    body.owns_loop(id)
                        || [catch, finally]
                            .into_iter()
                            .flatten()
                            .any(|block| block.owns_loop(id))
                }
                _ => false,
            })
    }
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
/// has one, against `library`, and marks the nested calls that may enter
/// one. `codes`, `blocks` and `stmts` are where the document's own ids
/// start.
pub(super) fn form(
    tables: &mut Tables,
    library: &Prepared,
    base: (u32, u32, u32),
    loop_test: u64,
) -> Islands {
    let (codes, blocks, stmts) = base;
    let mut islands = Islands {
        by_code: vec![None; tables.codes.len()],
        ..Islands::default()
    };
    for (index, code) in tables.codes.iter().enumerate() {
        if !code.charged || code.params.iter().any(|param| param.shared) {
            continue;
        }
        // Own shared bindings still need their allocation owner. Captures
        // and restored free bindings already have cells whose identity is
        // retained across every read, write and exit.
        let mut writable: Vec<bool> = code.slots.iter().map(|info| !info.shared).collect();
        let mut readable = writable.clone();
        for capture in &code.captures {
            readable[capture.inner as usize] = true;
            writable[capture.inner as usize] = true;
        }
        for (_, free) in &code.free {
            readable[free.slot as usize] = true;
            writable[free.slot as usize] = true;
        }
        let mut former = Former {
            tables,
            library,
            blocks,
            stmts,
            calls: Vec::new(),
            sites: islands.sites as u32,
            readable,
            writable,
            loop_test,
        };
        let Some(body) = former.block(code.body, false) else {
            continue;
        };
        let plan = IslandPlan {
            code: CodeId(codes + index as u32),
            params: code.params.iter().map(|param| param.at).collect(),
            captures: code
                .captures
                .iter()
                .map(|capture| capture.inner_at)
                .chain(code.free.iter().map(|(_, local)| local.at))
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
        if !nested.guest {
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
    loop_test: u64,
    tables: &'a Tables,
    library: &'a Prepared,
    blocks: u32,
    stmts: u32,
    calls: Vec<usize>,
    sites: u32,
    /// Which slots the program may read: unshared ones and existing cells.
    readable: Vec<bool>,
    /// Which slots it may write: unshared ones and existing cells.
    writable: Vec<bool>,
}

/// Whether `lib` called on `args` is `same(x, ())`, the test of a value
/// against the empty tuple its second operand builds, which an island
/// answers without building it: the kernel's `same`, with its prepared
/// entry's charge.
pub(crate) fn unit_test(function: Option<&Lib>, args: &[Expr]) -> bool {
    function.is_some_and(|function| function.is_same() && function.prepared.is_some())
        && matches!(
            args,
            [_, Expr { kind: ExprKind::Make { make: Make::Tuple, items, .. }, .. }]
                if items.is_empty()
        )
}

/// Whether an island op closes at most one region: a head closes the
/// region before it with nothing charged between, which no island charges.
fn single(closes: &[Close]) -> Option<Option<Close>> {
    match closes {
        [] => Some(None),
        [close] => Some(Some(*close)),
        _ => None,
    }
}

impl Former<'_> {
    /// Block `id`'s plan: an `if`'s block when `nested`, whose statements
    /// no island runs are cold; a body's otherwise, which has none.
    fn block(&mut self, id: BlockId, nested: bool) -> Option<Block> {
        let block = &self.tables.blocks[(id.0 - self.blocks) as usize];
        let statements = block
            .stmts
            .iter()
            .map(|stmt| {
                let (calls, sites) = (self.calls.len(), self.sites);
                match self.statement(*stmt) {
                    Some(statement) => Some(statement),
                    None if nested => {
                        self.calls.truncate(calls);
                        self.sites = sites;
                        Some(Statement {
                            id: *stmt,
                            leaves: 0,
                            kind: Kind::Cold,
                        })
                    }
                    None => None,
                }
            })
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
        if let Form::While {
            test,
            body,
            else_block,
            metered,
            ..
        } = &statement.form
        {
            let (test, body, else_block, metered) = (*test, *body, *else_block, *metered);
            let mut tree = expr::statement(&self.tables.stream, test, false)?;
            let Op::Test { cond } = tree.last else {
                return None;
            };
            let close = single(&tree.closes)?;
            let cond_expr = self.admitted(tree.take(cond)?)?;
            if !tree.spent() {
                return None;
            }
            return Some(Statement {
                id,
                leaves,
                kind: Kind::While {
                    cond: cond_expr,
                    close,
                    pc: test.end() - 1,
                    reg: cond.0,
                    cost: self.loop_test,
                    metered,
                    body: self.block(body, true)?,
                    otherwise: match else_block {
                        Some(block) => Some(self.block(block, true)?),
                        None => None,
                    },
                },
            });
        }
        let end = statement.ops.end().checked_sub(1)?;
        let mut tree = expr::statement(&self.tables.stream, statement.ops, false)?;
        let close = if matches!(tree.last, Op::SetField { .. } | Op::SetIndex { .. }) {
            None
        } else {
            single(&tree.closes)?
        };
        let kind = match (&statement.form, tree.last) {
            (Form::Plain, Op::Return { src }) => Kind::Return {
                value: self.admitted(tree.take(src)?)?,
                close,
                reg: src.0,
            },
            (Form::Plain, Op::WriteLocal { local, src } | Op::Bind { local, src })
                if self.writable(local.slot) =>
            {
                Kind::Write {
                    value: self.admitted(tree.take(src)?)?,
                    close,
                    pc: end,
                    reg: src.0,
                    at: local.at,
                    bind: matches!(tree.last, Op::Bind { .. }),
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
                    cond: self.admitted(tree.take(cond)?)?,
                    close,
                    pc: end,
                    reg: cond.0,
                    then_: self.block(then_block, true)?,
                    else_: self.block(else_block, true)?,
                }
            }
            (Form::Plain, Op::SetField { target, field, src }) => {
                let mut operands = vec![tree.take(target)?, tree.take(src)?];
                operands.sort_by_key(|expr| expr.pc);
                for operand in &mut operands {
                    self.admit(operand)?;
                }
                Kind::Store {
                    operands: operands.into(),
                    target: target.0,
                    index: None,
                    value: src.0,
                    field: Some(field.ix()),
                    closes: tree.closes.clone(),
                    pc: end,
                }
            }
            (Form::Plain, Op::SetIndex { target, src }) => {
                let index = Reg(target.0 + 1);
                let mut operands = vec![tree.take(target)?, tree.take(index)?, tree.take(src)?];
                operands.sort_by_key(|expr| expr.pc);
                for operand in &mut operands {
                    self.admit(operand)?;
                }
                Kind::Store {
                    operands: operands.into(),
                    target: target.0,
                    index: Some(index.0),
                    value: src.0,
                    field: None,
                    closes: tree.closes.clone(),
                    pc: end,
                }
            }
            (Form::Plain, Op::End) => Kind::Discard {
                value: self.admitted(tree.take(Reg(0))?)?,
                close,
                reg: 0,
            },
            (
                Form::Try {
                    body,
                    catch,
                    finally,
                    ..
                },
                Op::Try { .. },
            ) => {
                let (body, catch, finally) = (*body, *catch, *finally);
                Kind::Try {
                    body: self.block(body, true)?,
                    catch: match catch {
                        Some((_, block)) => Some(self.block(block, true)?),
                        None => None,
                    },
                    finally: match finally {
                        Some(block) => Some(self.block(block, true)?),
                        None => None,
                    },
                }
            }
            (Form::Plain, Op::Break | Op::Continue) => Kind::Depart {
                close,
                continuing: matches!(tree.last, Op::Continue),
            },
            (
                Form::For {
                    metered,
                    binding,
                    via: None,
                    body,
                    else_block,
                    ..
                },
                Op::ForIn { iterable },
            ) if self.writable(binding.slot) => {
                let (metered, at, body, else_block) = (*metered, binding.at, *body, *else_block);
                Kind::For {
                    value: self.admitted(tree.take(iterable)?)?,
                    close,
                    pc: end,
                    reg: iterable.0,
                    cost: self.loop_test,
                    metered,
                    at,
                    body: self.block(body, true)?,
                    otherwise: match else_block {
                        Some(block) => Some(self.block(block, true)?),
                        None => None,
                    },
                }
            }
            _ => return None,
        };
        // Every value the statement computes reaches its end.
        if !tree.spent() {
            return None;
        }
        Some(Statement { id, leaves, kind })
    }

    /// Whether slot `slot` is one an island binds and writes.
    fn writable(&self, slot: u32) -> bool {
        self.writable.get(slot as usize).copied().unwrap_or(false)
    }

    /// `expr`, admitted.
    fn admitted(&mut self, mut expr: Expr) -> Option<Expr> {
        self.admit(&mut expr)?;
        Some(expr)
    }

    /// Admits `expr` as island ops, in the order the reference runs them:
    /// its counted sites recorded and its closure call sites numbered, and
    /// each conditional arm of other ops made cold.
    fn admit(&mut self, expr: &mut Expr) -> Option<()> {
        match &mut expr.kind {
            ExprKind::Local { local, .. } => self
                .readable
                .get(local.slot as usize)
                .copied()?
                .then_some(()),
            ExprKind::Session { name, .. } => {
                self.tables.stream.names.get(name.ix())?;
                Some(())
            }
            ExprKind::Const(_) => Some(()),
            ExprKind::Operator {
                lib,
                invoke,
                args,
                closes,
            } if unit_test(self.library.get_lib(*lib), args) => {
                single(closes)?;
                // An operator's own op, or its invoke that counts nothing.
                if invoke.is_some_and(|invoke| invoke.call || !invoke.operator) {
                    return None;
                }
                self.admit(&mut args[0])
            }
            ExprKind::Operator {
                lib,
                invoke,
                args,
                closes,
            } => {
                for arg in args.iter_mut() {
                    self.admit(arg)?;
                }
                single(closes)?;
                let given = args.len();
                match invoke {
                    None => self.operator(*lib, given, false),
                    Some(invoke) if !invoke.guest => None,
                    Some(invoke) if invoke.call => {
                        self.operator(*lib, given, true)?;
                        self.calls.push(invoke.occurrence);
                        Some(())
                    }
                    Some(invoke) if invoke.operator => self.operator(*lib, given, false),
                    Some(_) => None,
                }
            }
            ExprKind::Native { lib, args, closes } => {
                for arg in args.iter_mut() {
                    self.admit(arg)?;
                }
                single(closes)?;
                self.native(*lib, args.len())
            }
            ExprKind::Index { target, index, .. } => {
                self.admit(target)?;
                self.admit(index)
            }
            ExprKind::Field { target, .. } => self.admit(target),
            ExprKind::Call {
                invoke,
                callee,
                args,
                closes,
                site,
            } => {
                if !invoke.guest {
                    return None;
                }
                if let Callee::Read(read) = callee {
                    self.admit(read)?;
                }
                for arg in args.iter_mut() {
                    self.admit(arg)?;
                }
                single(closes)?;
                self.calls.push(invoke.occurrence);
                if let Callee::Read(_) = callee {
                    *site = self.sites;
                    self.sites += 1;
                }
                Some(())
            }
            ExprKind::Cond {
                cond,
                closes,
                then,
                otherwise,
            } => {
                self.admit(cond)?;
                single(closes)?;
                let hot = [self.arm(then), self.arm(otherwise)];
                hot.contains(&true).then_some(())
            }
            ExprKind::Let {
                local, value, body, ..
            } => {
                if !self.writable(local.slot) {
                    return None;
                }
                self.admit(value)?;
                self.admit(body)
            }
            ExprKind::Make { items, .. } => {
                for item in items {
                    self.admit(item)?;
                }
                Some(())
            }
            ExprKind::Record { values, .. } => {
                for value in values {
                    self.admit(value)?;
                }
                Some(())
            }
            ExprKind::Pin(value) => self.admit(value),
            ExprKind::Act => None,
        }
    }

    /// Admits a conditional's `arm`: hot when its value is island ops that
    /// close at most one region at its end, cold otherwise, taking it then
    /// exiting at its first op. Whether it is hot.
    fn arm(&mut self, arm: &mut Arm) -> bool {
        let calls = self.calls.len();
        let sites = self.sites;
        let hot = arm.closes.len() <= 1
            && arm
                .value
                .as_deref_mut()
                .is_some_and(|value| self.admit(value).is_some());
        if !hot {
            self.calls.truncate(calls);
            self.sites = sites;
            arm.value = None;
            arm.closes = Box::default();
        }
        hot
    }

    /// Whether `lib` called with `given` arguments is a native an island
    /// calls through its general call: no operation, whose admission is
    /// the reference's, and no plan or body.
    fn native(&self, lib: LibId, given: usize) -> Option<()> {
        let function = self.library.get_lib(lib)?;
        (matches!(function.run, LibRun::Native { .. })
            && !function.operation()
            && given <= function.arity
            && function.arity <= MAX_ARGS)
            .then_some(())
    }

    /// Whether `lib` called with `given` arguments answers through its
    /// prepared entry, including its heap paths: an operator, or a helper
    /// call when `helper`.
    fn operator(&self, lib: LibId, given: usize, helper: bool) -> Option<()> {
        let function = self.library.get_lib(lib)?;
        function.prepared.as_ref()?;
        if function.arity != given || given > MAX_ARGS {
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

    /// A form already resolved in this executable. A cached callee is
    /// admitted only after `compiled` succeeds, and the form never changes.
    #[inline(always)]
    pub(crate) fn resolved(&self) -> Option<&Compiled> {
        self.compiled.get()?.as_ref()
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

fn verify_closes(closes: &[Close], stream: &super::Stream) -> Result<(), Refused> {
    closes
        .iter()
        .try_for_each(|close| verify_close(Some(close), stream))
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
            Kind::Store {
                operands, closes, ..
            } => {
                for operand in operands {
                    verify_expr(operand, stream, calls)?;
                }
                verify_closes(closes, stream)?;
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
            Kind::Cold => {}
            Kind::Try {
                body,
                catch,
                finally,
            } => {
                verify_block(body, stream, calls)?;
                for block in [catch, finally].into_iter().flatten() {
                    verify_block(block, stream, calls)?;
                }
            }
            Kind::While {
                cond,
                close,
                body,
                otherwise,
                ..
            } => {
                verify_expr(cond, stream, calls)?;
                verify_close(close.as_ref(), stream)?;
                verify_block(body, stream, calls)?;
                if let Some(block) = otherwise {
                    verify_block(block, stream, calls)?;
                }
            }
            Kind::For {
                value,
                close,
                body,
                otherwise,
                ..
            } => {
                verify_expr(value, stream, calls)?;
                verify_close(close.as_ref(), stream)?;
                verify_block(body, stream, calls)?;
                if let Some(block) = otherwise {
                    verify_block(block, stream, calls)?;
                }
            }
            Kind::Depart { close, .. } => verify_close(close.as_ref(), stream)?,
        }
    }
    Ok(())
}

fn verify_expr(expr: &Expr, stream: &super::Stream, calls: &mut Vec<usize>) -> Result<(), Refused> {
    for operand in expr.operands() {
        verify_expr(operand, stream, calls)?;
    }
    verify_closes(expr.closes(), stream)?;
    match &expr.kind {
        ExprKind::Operator {
            invoke: Some(invoke),
            ..
        } if invoke.call => {
            match stream.ops.get(expr.pc as usize) {
                Some(Op::Nested { act, .. })
                    if stream.nested.get(act.ix()).map(|nested| nested.occurrence)
                        == Some(invoke.occurrence) => {}
                _ => return Err(Refused::Calls),
            }
            calls.push(invoke.occurrence);
        }
        ExprKind::Call { invoke, .. } => {
            if stream
                .nested
                .get(invoke.act.ix())
                .map(|nested| nested.occurrence)
                != Some(invoke.occurrence)
            {
                return Err(Refused::Calls);
            }
            calls.push(invoke.occurrence);
        }
        ExprKind::Cond {
            then, otherwise, ..
        } => {
            for arm in [then, otherwise] {
                if let Some(value) = &arm.value {
                    verify_expr(value, stream, calls)?;
                    verify_closes(&arm.closes, stream)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}
