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
//! Island ops: reads of unshared slots, of captures and of session
//! bindings (`K-SES-001`), resident constants
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
use lash_kernel_exec::{Op, Reg};

use super::expr::{self, Arm, Callee, Close, Expr, Kind as ExprKind};
use super::{
    BlockId, CodeId, Executable, Form, LibId, LibRun, NestedCallee, NestedKind, Prepared, Stmt,
    StmtId, Tables,
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
/// has one, against `library`, and marks the nested calls that may enter
/// one. `codes`, `blocks` and `stmts` are where the document's own ids
/// start.
pub(super) fn form(tables: &mut Tables, library: &Prepared, base: (u32, u32, u32)) -> Islands {
    let (codes, blocks, stmts) = base;
    let mut islands = Islands {
        by_code: vec![None; tables.codes.len()],
        ..Islands::default()
    };
    for (index, code) in tables.codes.iter().enumerate() {
        if !code.charged
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
            library,
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
    library: &'a Prepared,
    blocks: u32,
    stmts: u32,
    calls: Vec<usize>,
    sites: u32,
    /// Which slots the code's island may read: unshared ones and captures.
    readable: Vec<bool>,
    /// Which slots it may bind and write: unshared ones.
    writable: Vec<bool>,
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
        let end = statement.ops.end().checked_sub(1)?;
        let mut tree = expr::statement(&self.tables.stream, statement.ops, false)?;
        let close = single(&tree.closes)?;
        let kind = match (&statement.form, tree.last) {
            (Form::Plain, Op::Return { src }) => Kind::Return {
                value: self.admitted(tree.take(src)?)?,
                close,
                reg: src.0,
            },
            (Form::Plain, Op::WriteLocal { local, src } | Op::Bind { local, src })
                if self.unshared(local.slot) =>
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
                    then_: self.block(then_block)?,
                    else_: self.block(else_block)?,
                }
            }
            (Form::Plain, Op::End) => Kind::Discard {
                value: self.admitted(tree.take(Reg(0))?)?,
                close,
                reg: 0,
            },
            _ => return None,
        };
        // Every value the statement computes reaches its end.
        if !tree.spent() {
            return None;
        }
        Some(Statement { id, leaves, kind })
    }

    /// Whether slot `slot` is one an island binds and writes.
    fn unshared(&self, slot: u32) -> bool {
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
            ExprKind::Const(value) => held(value).then_some(()),
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
                if !self.unshared(local.slot) {
                    return None;
                }
                self.admit(value)?;
                self.admit(body)
            }
            ExprKind::Act
            | ExprKind::Make { .. }
            | ExprKind::Record { .. }
            | ExprKind::Field { .. }
            | ExprKind::Pin(_) => None,
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
            && function.arity <= MAX_SLOTS)
            .then_some(())
    }

    /// Whether `lib` called with `given` arguments answers through its
    /// prepared entry with a scalar charge table: an operator, or a helper
    /// call when `helper`.
    fn operator(&self, lib: LibId, given: usize, helper: bool) -> Option<()> {
        let function = self.library.get_lib(lib)?;
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
