//! Flatten the admitted source tree once. No evaluator closure or control
//! recursion survives compilation; live source registers accompany each exit.

use super::*;
use crate::machine::islands::code::{
    HeapInputs, HeapKernel, KindInput, NativeInputs, OperatorInputs, ScalarInputs,
};

#[path = "program/accumulation.rs"]
mod accumulation;
#[path = "program/update.rs"]
mod update;
pub(crate) use update::Update;
#[path = "program/format.rs"]
mod format;
pub(crate) use format::Format;
#[path = "program/numeric.rs"]
mod numeric;
pub(crate) use numeric::{CallInputs, Destination, Operand};
#[path = "program/access.rs"]
mod access;
pub(crate) use access::{AccessShape, AccessSite};
#[path = "program/guarded.rs"]
mod guarded;
#[path = "program/merge.rs"]
mod merge;
use crate::compile::expr::{Arm, Callee, Make};
use crate::compile::islands::{Block as SourceBlock, Kind as StatementKind, Statement, unit_test};
pub(crate) use merge::{
    Append as CollectionAppend, CollectionStep, Merge, Placement, ReadSlot, Reduction, Visit,
};

pub(crate) struct CollectionKernel {
    pub(crate) step: std::sync::Arc<CollectionStep>,
    pub(crate) tail: bool,
}

impl CollectionKernel {
    pub(crate) fn addresses(&mut self, visit: &mut impl FnMut(&mut usize)) {
        match std::sync::Arc::make_mut(&mut self.step) {
            CollectionStep::Visit(step) => {
                for at in &mut step.after {
                    visit(at);
                }
            }
            CollectionStep::Merge(step) => visit(&mut step.callback),
            _ => {}
        }
    }
}

pub(crate) struct Instruction {
    pub(crate) op: Op,
    /// A source-certified kernel with this operation as its generic suffix.
    pub(crate) kernel: Option<CollectionKernel>,
    /// Source statement admission immediately before its first operation.
    pub(crate) begin: Option<StatementEntry>,
    /// Evaluated operands, in source evaluation order, before this op.
    pub(in crate::machine::islands) live: Box<[u32]>,
}

/// A pure source operand, read once in its original argument order.
/// Failure retains the operand's source PC and the completed prefix.
#[derive(Clone, Copy)]
pub(crate) enum Read {
    Literal {
        reg: u32,
        value: Value,
    },
    Local {
        reg: u32,
        at: usize,
        shared: bool,
        miss: u32,
    },
}

impl Read {
    pub(crate) fn reg(&self) -> u32 {
        match self {
            Self::Literal { reg, .. } | Self::Local { reg, .. } => *reg,
        }
    }
}

pub(crate) struct StoreInputs {
    pub(crate) reads: [Read; 3],
    /// Target, index and value positions in source evaluation order.
    pub(crate) roles: [usize; 3],
}

/// The most arms a guarded read enters.
pub(crate) const GUARDED_ARMS: usize = 4;

#[derive(Clone)]
pub(crate) struct StatementEntry {
    pub(crate) id: StmtId,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) ahead: u64,
}

#[derive(Clone)]
pub(crate) struct WriteSite {
    pub(crate) begin: Option<StatementEntry>,
    pub(crate) reads: Option<Box<[Read]>>,
    pub(crate) access: Option<AccessStart>,
    pub(crate) at: usize,
    pub(crate) shared: bool,
    pub(crate) reg: u32,
    pub(in crate::machine::islands) pc: u32,
    pub(crate) bind: bool,
    pub(in crate::machine::islands) close: Option<Close>,
    pub(in crate::machine::islands) covered: bool,
    pub(in crate::machine::islands) leaves: u32,
    /// A linear text accumulator's write (FIG-6795).
    pub(in crate::machine::islands) accumulates: bool,
}

/// A write may start the following member kernel. The source write remains
/// its only fallback, and another initializer is an index into that stream.
#[derive(Clone, Copy)]
pub(crate) struct AccessStart {
    pub(crate) head: usize,
    pub(crate) init: Option<usize>,
}

pub(super) struct Build<'e> {
    exe: &'e Executable,
    stream: &'e Stream,
    shared: &'e [bool],
    root: Option<StmtId>,
    begin: Option<StatementEntry>,
    pub(super) room: u32,
    pub(super) observes: bool,
    pub(super) segments: Vec<u64>,
    measured: Option<Vec<u64>>,
    open: Option<usize>,
    pub(super) blocks: Vec<Block>,
    pub(super) ops: Vec<Instruction>,
    pub(super) fused: bool,
    pub(super) charged: bool,
    pub(super) registers: usize,
    restarts: usize,
    captures: Vec<(u32, super::Restart, bool)>,
    source_registers: u32,
    live: Vec<u32>,
    statement: Option<StmtId>,
    /// The accumulator slots of each loop being built, innermost last.
    texts: Vec<Vec<usize>>,
}

impl<'e> Build<'e> {
    pub(super) fn new(
        exe: &'e Executable,
        shared: &'e [bool],
        stream: &'e Stream,
        measured: Option<Vec<u64>>,
        source_registers: u32,
    ) -> Self {
        Self {
            exe,
            stream,
            shared,
            root: None,
            begin: None,
            room: 0,
            observes: false,
            segments: Vec::new(),
            measured,
            open: None,
            blocks: Vec::new(),
            ops: Vec::new(),
            fused: true,
            charged: true,
            registers: 0,
            restarts: 0,
            captures: Vec::new(),
            source_registers,
            live: Vec::new(),
            statement: None,
            texts: Vec::new(),
        }
    }

    fn emit(&mut self, op: Op) -> usize {
        let pc = self.ops.len();
        self.ops.push(Instruction {
            op,
            kernel: None,
            begin: self.begin.take(),
            live: self.live.clone().into(),
        });
        pc
    }

    fn result(&mut self, reg: u32, consumed: &[u32]) {
        self.live.retain(|at| !consumed.contains(at));
        self.live.push(reg);
        self.registers = self.registers.max(reg as usize + 1);
    }

    fn start(&mut self) -> u64 {
        let at = self.segments.len();
        self.segments.push(0);
        self.open = Some(at);
        self.measured.as_ref().map_or(0, |measured| measured[at])
    }

    fn cover(&mut self, units: u64) -> bool {
        let Some(at) = self.open else {
            return false;
        };
        self.segments[at] = self.segments[at].saturating_add(units);
        true
    }

    fn close(&mut self, close: Option<Close>, _reg: u32) {
        if let Some(close) = close {
            let covered = self.cover(u64::from(close.total));
            self.emit(Op::Close { close, covered });
        }
    }

    /// Preserve every source region at the operation that commits it.
    fn ordered_closes(&mut self, closes: &[Close], reg: u32) -> Option<Close> {
        if closes.len() <= 1 {
            return closes.first().copied();
        }
        for close in closes {
            self.close(Some(*close), reg);
        }
        None
    }

    fn site(&mut self, depth: Depth, observes: bool) {
        let room = match depth {
            Depth::None | Depth::Operation(0 | 1) => 0,
            Depth::Operation(extra) | Depth::Template(extra) => extra,
        };
        self.room = self.room.max(room);
        self.observes |= observes;
    }

    pub(super) fn block(&mut self, block: &SourceBlock) -> Option<()> {
        self.block_entry(block, true)
    }

    fn block_entry(&mut self, block: &SourceBlock, opening: bool) -> Option<()> {
        let index = self.blocks.len();
        self.blocks.push(Block {
            id: block.id,
            declares: block.declares.clone(),
            starts: Box::default(),
            end: 0,
            after: 0,
        });
        let literal_entry = self.fused
            && index == 0
            && matches!(
                &*block.statements,
                [Statement {
                    kind: StatementKind::Return {
                        value: Expr {
                            kind: Kind::Const(_),
                            ..
                        },
                        ..
                    },
                    ..
                }]
            );
        if opening && !literal_entry {
            self.emit(Op::Open(index));
        }
        let entry = self.ops.len();
        let mut starts = Vec::with_capacity(block.statements.len());
        for (index, statement) in block.statements.iter().enumerate() {
            starts.push(self.ops.len());
            let mark = (
                self.ops.len(),
                self.segments.len(),
                self.blocks.len(),
                self.texts.len(),
                self.restarts,
            );
            if self.statement(statement, index).is_none() {
                // A straight-line statement the register cannot build runs
                // cold: it exits at its first op, as a cold statement does.
                if !matches!(
                    self.exe.stmt(statement.id).form,
                    crate::compile::Form::Plain | crate::compile::Form::Action(_)
                ) {
                    return None;
                }
                self.ops.truncate(mark.0);
                self.segments.truncate(mark.1);
                self.blocks.truncate(mark.2);
                self.texts.truncate(mark.3);
                self.restarts = mark.4;
                (self.root, self.begin, self.open) = (None, None, None);
                let cold = Statement {
                    id: statement.id,
                    leaves: 0,
                    root: None,
                    kind: StatementKind::Cold,
                };
                self.statement(&cold, index)?;
            }
        }
        self.blocks[index].starts = starts.into();
        if literal_entry {
            let Op::ReturnLiteral { entry: opens, .. } = &mut self.ops[entry].op else {
                return None;
            };
            *opens = Some(index);
        }
        self.blocks[index].end = self.ops.len();
        self.emit(Op::Done {
            leaves: 1,
            empty: block.statements.is_empty().then_some(block.id),
        });
        self.blocks[index].after = self.ops.len();
        Some(())
    }

    fn statement(&mut self, statement: &Statement, index: usize) -> Option<()> {
        self.statement = Some(statement.id);
        self.captures.clear();
        self.live.clear();
        let ahead = self.start();
        self.cover(1);
        if self.fused
            && let StatementKind::Return {
                value:
                    Expr {
                        kind: Kind::Const(value),
                        ..
                    },
                close,
                reg,
            } = &statement.kind
        {
            let covered = close.is_some_and(|close| self.cover(u64::from(close.total)));
            self.emit(Op::ReturnLiteral {
                entry: None,
                id: statement.id,
                index,
                ahead,
                value: *value,
                close: *close,
                covered,
                reg: *reg,
            });
            self.open = None;
            return Some(());
        }
        if statement.root.is_none()
            && let StatementKind::Write {
                value,
                close,
                pc,
                reg,
                at,
                bind,
                ..
            } = &statement.kind
            && let Some(reads) = self.reads(std::slice::from_ref(value))
        {
            let covered = close.is_some_and(|close| self.cover(u64::from(close.total)));
            self.emit(Op::Write {
                site: Box::new(WriteSite {
                    access: None,
                    begin: Some(StatementEntry {
                        id: statement.id,
                        index,
                        ahead,
                    }),
                    reads: Some(reads),
                    at: *at as usize,
                    shared: self.shared[*at as usize],
                    reg: *reg,
                    pc: *pc,
                    bind: *bind,
                    close: *close,
                    covered,
                    leaves: statement.leaves,
                    accumulates: false,
                }),
            });
            self.open = None;
            return Some(());
        }
        self.begin = Some(StatementEntry {
            id: statement.id,
            index,
            ahead,
        });
        if let Some(root) = &statement.root {
            self.root = Some(statement.id);
            self.expr(root)?;
            self.root = None;
        }
        match &statement.kind {
            StatementKind::Try {
                body,
                catch,
                finally,
            } => {
                self.open = None;
                let start = self.emit(Op::Try {
                    id: statement.id,
                    finally: None,
                    after: 0,
                });
                self.block(body)?;
                if let Some(block) = catch {
                    self.block(block)?;
                }
                let cleanup = finally.as_ref().map(|_| self.ops.len());
                if let Some(block) = finally {
                    self.block(block)?;
                }
                let after = self.ops.len();
                self.ops[start].op = Op::Try {
                    id: statement.id,
                    finally: cleanup,
                    after,
                };
            }
            StatementKind::While {
                cond,
                close,
                pc,
                reg,
                cost,
                metered,
                body,
                otherwise,
            } => {
                self.open = None;
                let start = self.emit(Op::Loop {
                    id: statement.id,
                    header: 0,
                    texts: Box::default(),
                });
                let header = self.emit(Op::Header {
                    repeat: None,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: None,
                    item_hook: false,
                    otherwise: 0,
                    after: 0,
                });
                self.expr(cond)?;
                self.close(*close, *reg);
                self.open = None;
                let branch = self.emit(Op::LoopBranch {
                    reg: *reg,
                    pc: *pc,
                    otherwise: 0,
                });
                self.live.clear();
                self.texts.push(Vec::new());
                self.block(body)?;
                let texts = self.texts.pop().unwrap_or_default().into();
                let exhausted = self.ops.len();
                self.loop_else(otherwise.as_ref(), statement.leaves)?;
                let after = self.ops.len();
                self.ops[start].op = Op::Loop {
                    id: statement.id,
                    header,
                    texts,
                };
                let repeat = self.fused.then_some(()).and_then(|_| {
                    self.exe
                        .islands
                        .scalar
                        .iter()
                        .find(|plan| plan.stmt == statement.id)
                        .and_then(|plan| {
                            Some(Box::new(RepeatSite {
                                run: super::super::code::repeat::Repeat::of(
                                    statement,
                                    plan,
                                    self.shared,
                                    self.exe,
                                    self.charged,
                                )?,
                                iteration_max: plan.iteration_max,
                            }))
                        })
                });
                self.ops[header].op = Op::Header {
                    repeat,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: None,
                    item_hook: false,
                    otherwise: exhausted,
                    after,
                };
                self.ops[branch].op = Op::LoopBranch {
                    reg: *reg,
                    pc: *pc,
                    otherwise: exhausted,
                };
            }
            StatementKind::For {
                value,
                close,
                pc,
                reg,
                cost,
                metered,
                at,
                body,
                otherwise,
            } => {
                let via = match &self.exe.stmt(statement.id).form {
                    crate::compile::Form::For { via, .. } => *via,
                    _ => None,
                };
                let item_hook = via.is_some_and(|via| via.item.is_some());
                self.expr(value)?;
                self.close(*close, *reg);
                self.open = None;
                let start = self.emit(Op::For {
                    id: statement.id,
                    header: 0,
                    reg: *reg,
                    pc: *pc,
                    via: via.is_some(),
                    texts: Box::default(),
                });
                self.live.clear();
                let header = self.emit(Op::Header {
                    repeat: None,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: Some(*at as usize),
                    item_hook,
                    otherwise: 0,
                    after: 0,
                });
                self.texts.push(Vec::new());
                self.block(body)?;
                let texts = self.texts.pop().unwrap_or_default().into();
                let exhausted = self.ops.len();
                self.loop_else(otherwise.as_ref(), statement.leaves)?;
                let after = self.ops.len();
                self.ops[start].op = Op::For {
                    id: statement.id,
                    header,
                    reg: *reg,
                    pc: *pc,
                    via: via.is_some(),
                    texts,
                };
                self.ops[header].op = Op::Header {
                    repeat: None,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: Some(*at as usize),
                    item_hook,
                    otherwise: exhausted,
                    after,
                };
            }
            StatementKind::Depart { close, continuing } => {
                self.close(*close, 0);
                self.emit(Op::Depart {
                    continuing: *continuing,
                });
            }
            StatementKind::Return { value, close, reg } => {
                self.expr(value)?;
                self.close(*close, *reg);
                self.emit(Op::Return {
                    reg: *reg,
                    pc: self.exe.stmt(statement.id).ops.end() - 1,
                });
            }
            StatementKind::Write {
                value,
                close,
                pc,
                reg,
                at,
                bind,
                accumulates,
            } => {
                // A write that may stop after its concatenation took its
                // operand's storage would leave the slot a text with none.
                let accumulates =
                    *accumulates && !*bind && close.is_none() && !self.shared[*at as usize];
                if accumulates {
                    self.emit(Op::Accumulate(*at as usize));
                    if let Some(texts) = self.texts.last_mut() {
                        texts.push(*at as usize);
                    }
                }
                self.expr(value)?;
                let covered = close.is_some_and(|close| self.cover(u64::from(close.total)));
                self.emit(Op::Write {
                    site: Box::new(WriteSite {
                        access: None,
                        begin: None,
                        reads: None,
                        at: *at as usize,
                        shared: self.shared[*at as usize],
                        reg: *reg,
                        pc: *pc,
                        bind: *bind,
                        close: *close,
                        covered,
                        leaves: statement.leaves,
                        accumulates,
                    }),
                });
                self.fuse_accumulation();
            }
            StatementKind::Store {
                operands,
                target,
                index,
                value,
                field,
                closes,
                pc,
            } => {
                let inputs = index
                    .filter(|_| field.is_none() && closes.is_empty())
                    .and_then(|index| self.store_reads(operands, [*target, index, *value]));
                if inputs.is_none() {
                    for operand in operands {
                        self.expr(operand)?;
                    }
                }
                for close in closes {
                    self.close(Some(*close), *value);
                }
                self.emit(Op::Store {
                    target: *target,
                    index: *index,
                    value: *value,
                    field: *field,
                    pc: *pc,
                    inputs,
                    cache: index.is_some_and(|index| {
                        operands.iter().any(|operand| {
                            operand.reg == index
                                && matches!(operand.kind, Kind::Const(Value::Text(_)))
                        })
                    }),
                    finish: None,
                });
                self.emit(Op::Done {
                    leaves: statement.leaves,
                    empty: None,
                });
            }
            StatementKind::Remove {
                operands,
                target,
                index,
                closes,
                pc,
            } => {
                for operand in operands {
                    self.expr(operand)?;
                }
                for close in closes {
                    self.close(Some(*close), *index);
                }
                self.emit(Op::Remove {
                    target: *target,
                    index: *index,
                    pc: *pc,
                });
                self.emit(Op::Done {
                    leaves: statement.leaves,
                    empty: None,
                });
            }
            StatementKind::Session {
                value,
                close,
                pc,
                reg,
                name,
                bind,
            } => {
                self.expr(value)?;
                self.close(*close, *reg);
                self.emit(Op::Publish {
                    name: *name,
                    reg: *reg,
                    pc: *pc,
                    bind: *bind,
                });
                self.live.clear();
                self.emit(Op::Done {
                    leaves: statement.leaves,
                    empty: None,
                });
            }
            StatementKind::Effect {
                value,
                close,
                pc,
                reg,
            } => {
                self.expr(value)?;
                self.close(*close, *reg);
                self.emit(Op::Effect(*pc));
            }
            StatementKind::Discard { value, close, reg } => {
                self.expr(value)?;
                self.close(*close, *reg);
                self.live.clear();
                self.emit(Op::Done {
                    leaves: statement.leaves,
                    empty: None,
                });
            }
            StatementKind::If {
                cond,
                close,
                pc,
                reg,
                then_,
                else_,
            } => {
                let head = self.ops.len();
                self.expr(cond)?;
                self.close(*close, *reg);
                self.open = None;
                let branch = self.emit(Op::Branch {
                    reg: *reg,
                    pc: *pc,
                    otherwise: 0,
                    opens: [None, None],
                });
                let then_entry = self.branch_entry(then_, statement.leaves)?;
                let jump = self.emit(Op::Jump(0));
                let otherwise = self.ops.len();
                let else_entry = self.branch_entry(else_, statement.leaves)?;
                self.ops[branch].op = Op::Branch {
                    reg: *reg,
                    pc: *pc,
                    otherwise,
                    opens: [then_entry, else_entry],
                };
                self.ops[jump].op = Op::Jump(self.ops.len());
                self.guard(head);
            }
            StatementKind::Cold => {
                self.emit(Op::Cold(Deferred::Source(
                    self.exe.stmt(statement.id).ops.start,
                )));
            }
        }
        self.open = None;
        self.live.clear();
        self.captures.iter().all(|capture| capture.2).then_some(())
    }

    /// The branch instruction enters a nonempty arm's source block itself.
    fn branch_entry(&mut self, block: &SourceBlock, leaves: u32) -> Option<Option<usize>> {
        self.live.clear();
        if block.statements.is_empty() {
            self.emit(Op::Done {
                leaves,
                empty: Some(block.id),
            });
            Some(None)
        } else {
            let index = self.blocks.len();
            self.block_entry(block, false)?;
            Some(Some(index))
        }
    }

    fn branch(&mut self, block: &SourceBlock, leaves: u32) -> Option<()> {
        self.live.clear();
        if block.statements.is_empty() {
            self.emit(Op::Done {
                leaves,
                empty: Some(block.id),
            });
        } else {
            self.block(block)?;
        }
        Some(())
    }

    fn loop_else(&mut self, block: Option<&SourceBlock>, leaves: u32) -> Option<()> {
        self.live.clear();
        match block {
            Some(block) => {
                self.emit(Op::EndLoop);
                self.branch(block, leaves)?;
            }
            None => {
                self.emit(Op::LoopDone(leaves));
            }
        }
        Some(())
    }

    fn args(&mut self, args: &[Expr]) -> Option<Box<[u32]>> {
        for arg in args {
            self.expr(arg)?;
        }
        Some(args.iter().map(|arg| arg.reg).collect())
    }

    fn expr(&mut self, expr: &Expr) -> Option<()> {
        let (pc, reg) = (expr.pc, expr.reg);
        let collection = match &expr.kind {
            Kind::Operator { lib, .. } => self.exe.lib(*lib).collection_method(),
            Kind::Call { invoke, .. } => invoke
                .act
                .and_then(|act| self.stream.nested[act.ix()].kind.operator_lib())
                .is_some_and(|lib| self.exe.lib(lib).collection_method()),
            _ => false,
        };
        if collection {
            for close in expr.closes() {
                if let Some((made, input)) =
                    crate::compile::islands::collection_restart(self.stream, close.head, pc)
                {
                    let saved = self.source_registers + 2 * self.restarts as u32;
                    self.registers = self.registers.max(saved as usize + 2);
                    let restart = super::Restart {
                        statement: self.statement?,
                        head: close.head,
                        reg: input,
                        saved,
                    };
                    self.restarts += 1;
                    self.captures.push((made, restart, false));
                    self.emit(Op::Constant {
                        dst: saved + 1,
                        value: Value::Bool(false),
                    });
                }
            }
        }
        match &expr.kind {
            Kind::Local { local, miss, .. } => {
                self.emit(Op::Local {
                    dst: reg,
                    at: local.at as usize,
                    shared: self.shared[local.at as usize],
                    miss: *miss,
                });
                self.result(reg, &[]);
            }
            Kind::Session { name, miss } => {
                self.emit(Op::Session {
                    dst: reg,
                    name: name.0,
                    miss: *miss,
                });
                self.result(reg, &[]);
            }
            Kind::Const(value) => {
                self.emit(Op::Constant {
                    dst: reg,
                    value: *value,
                });
                self.result(reg, &[]);
            }
            Kind::Text { constant, miss } => {
                self.emit(Op::Cold(Deferred::Text {
                    dst: reg,
                    constant: *constant,
                    miss: *miss,
                }));
                self.result(reg, &[]);
            }
            Kind::Operator {
                lib, args, closes, ..
            } if self.fused && unit_test(Some(self.exe.lib(*lib)), args) => {
                let [value, unit] = &args[..] else {
                    return None;
                };
                let Kind::Make { miss, .. } = unit.kind else {
                    return None;
                };
                self.expr(value)?;
                self.open = None;
                self.emit(Op::Unit {
                    site: UnitSite {
                        source: self.statement.and_then(|id| {
                            let stmt = self.exe.stmt(id);
                            let comparison = self.stream.source_sites.get(pc as usize)?.as_ref()?;
                            if !matches!(
                                stmt.form,
                                crate::compile::Form::If { .. }
                                    | crate::compile::Form::While { .. }
                            ) || **comparison != stmt.site.child(0)
                            {
                                return None;
                            }
                            Some(Box::new(super::UnitSource {
                                statement: id,
                                binding: match &value.kind {
                                    Kind::Local { local, .. } => Some(local.at),
                                    _ => None,
                                },
                                value: (**self
                                    .stream
                                    .source_sites
                                    .get(value.pc as usize)?
                                    .as_ref()?)
                                .clone(),
                                tuple: (**self
                                    .stream
                                    .source_sites
                                    .get(unit.pc as usize)?
                                    .as_ref()?)
                                .clone(),
                                comparison: (**comparison).clone(),
                                test: stmt.site.clone(),
                            }))
                        }),
                        lib: *lib,
                        close: closes.first().copied(),
                        miss,
                        reg,
                    },
                    src: value.reg,
                });
                self.result(reg, &[value.reg]);
            }
            Kind::Operator { lib, args, .. } if self.fused && self.kind(expr, *lib, args)? => {}
            Kind::Operator {
                args,
                lib,
                invoke,
                closes,
            } => {
                let Some(mut site) = operator_site(expr, self.exe) else {
                    let function = self.exe.lib(*lib);
                    let extra =
                        function
                            .specialization
                            .prepared
                            .as_ref()
                            .map_or(0, |(entry, _)| {
                                entry
                                    .deepest(&|id| self.exe.lib(id), &|id| self.exe.library_lib(id))
                            });
                    let depth = if function.operation() {
                        Depth::Operation(extra)
                    } else {
                        Depth::Template(extra)
                    };
                    self.site(
                        depth,
                        (self.root.is_some() || invoke.is_some_and(|invoke| invoke.call))
                            && matches!(function.run, LibRun::Body(_)),
                    );
                    let site = HeapSite {
                        lib: *lib,
                        close: closes.first().copied(),
                        pc,
                        reg,
                        depth,
                        checked: self.root_checked() || invoke.is_some_and(|invoke| invoke.checked),
                        counted: invoke.and_then(|invoke| invoke.counted()),
                        act: invoke
                            .filter(|invoke| invoke.call)
                            .and_then(|invoke| invoke.act),
                        root: self.root,
                    };
                    return self.heap(site, args, closes);
                };
                site.root = self.root;
                site.counted = site.counted.or_else(|| {
                    self.root.and_then(|id| {
                        let crate::compile::Form::Action(action) = &self.exe.stmt(id).form else {
                            return None;
                        };
                        action.guest.then_some(action.occurrence)
                    })
                });
                site.checked |= self.root_checked();
                site.body_when_observed |=
                    self.root.is_some() && matches!(self.exe.lib(site.lib).run, LibRun::Body(_));
                self.site(site.depth, site.body_when_observed);
                if site.concat(self.exe).is_some() {
                    let extra = self.exe.lib(site.lib).prepared_depth(self.exe);
                    self.site(Depth::Template(extra), site.body_when_observed);
                }
                let direct = (closes.len() <= 1).then(|| self.reads(args)).flatten();
                if let Some(reads) = direct {
                    let consumed: Vec<_> = reads.iter().map(Read::reg).collect();
                    let covered = site.pair(self.exe).is_some()
                        && reads.len() == 2
                        && self.cover(site.fixed.saturating_add(site.table(self.exe).max()));
                    if !covered {
                        self.open = None;
                    }
                    self.emit(Op::Operator {
                        site: Box::new(site),
                        inputs: OperatorInputs::Source(reads),
                        covered,
                        to: Destination::Register,
                    });
                    self.result(reg, &consumed);
                    return Some(());
                }
                let args = self.args(args)?;
                site.close = self.ordered_closes(closes, reg);
                site.fixed = site.close.map_or(0, |close| u64::from(close.total));
                let covered = site.pair(self.exe).is_some()
                    && args.len() == 2
                    && self.cover(site.fixed.saturating_add(site.table(self.exe).max()));
                if !covered {
                    self.open = None;
                }
                self.emit(Op::Operator {
                    site: Box::new(site),
                    inputs: OperatorInputs::Registers(args.clone()),
                    covered,
                    to: Destination::Register,
                });
                self.result(reg, &args);
            }
            Kind::Native { lib, args, closes } => {
                let extra = self
                    .exe
                    .lib(*lib)
                    .specialization
                    .prepared
                    .as_ref()
                    .map_or(0, |(entry, _)| entry.depth());
                let mut site = NativeSite {
                    lib: *lib,
                    close: closes.first().copied(),
                    pc,
                    reg,
                    depth: if extra > 0 {
                        Depth::Template(extra)
                    } else {
                        Depth::None
                    },
                };
                self.site(site.depth, false);
                if closes.len() <= 1
                    && let Some(reads) = self.reads(args)
                {
                    let consumed: Vec<_> = reads.iter().map(Read::reg).collect();
                    self.open = None;
                    self.emit(Op::Native {
                        site,
                        inputs: NativeInputs::Source(reads),
                    });
                    self.result(reg, &consumed);
                    return Some(());
                }
                let args = self.args(args)?;
                site.close = self.ordered_closes(closes, reg);
                self.open = None;
                self.emit(Op::Native {
                    site,
                    inputs: NativeInputs::Registers(args.clone()),
                });
                self.result(reg, &args);
            }
            Kind::Field {
                target,
                field,
                miss,
            } => {
                self.expr(target)?;
                self.emit(Op::Field {
                    dst: reg,
                    field: field.ix(),
                    miss: *miss,
                });
                self.result(reg, &[target.reg]);
            }
            Kind::Index {
                target,
                index,
                miss,
                site,
            } => {
                self.expr(target)?;
                self.expr(index)?;
                self.emit(Op::Index {
                    site: crate::compile::ReadSite::new(site.0, self.stream.layout),
                    dst: reg,
                    miss: *miss,
                });
                self.result(reg, &[target.reg, index.reg]);
            }
            Kind::Call {
                invoke,
                callee,
                args,
                closes,
                site,
            } => {
                let inputs = self.call_reads(callee, args, reg);
                let (read, declared) = match callee {
                    Callee::Read(read) => {
                        if inputs.is_none() {
                            self.expr(read)?;
                        }
                        (true, None)
                    }
                    Callee::Declared(code) => (false, Some(*code)),
                };
                let args = if inputs.is_some() {
                    args.iter().map(|arg| arg.reg).collect()
                } else {
                    self.args(args)?
                };
                self.open = None;
                let library = self
                    .root
                    .and_then(|id| match &self.exe.stmt(id).form {
                        crate::compile::Form::Action(action) => match &action.kind {
                            crate::compile::ActionKind::Call { callee, .. } => callee.library(),
                            _ => None,
                        },
                        _ => None,
                    })
                    .or_else(|| {
                        invoke
                            .act
                            .and_then(|act| self.stream.nested[act.ix()].kind.operator_lib())
                    });
                if !read
                    && args.len() == 2
                    && let Some(lib) = library
                    && let Some(extra) = super::answer_depth(self.exe, lib)
                    && let Some(units) = self.member_charge(lib)
                {
                    let depth = Depth::Template(extra);
                    self.site(depth, true);
                    let inputs = match inputs {
                        Some((None, reads)) => HeapInputs::Read(reads.try_into().ok()?),
                        Some((Some(_), _)) => unreachable!(),
                        None => HeapInputs::Registers,
                    };
                    self.emit(Op::Heap {
                        site: HeapSite {
                            lib,
                            close: closes.first().copied(),
                            pc,
                            reg,
                            depth,
                            checked: invoke.checked || self.root_checked(),
                            counted: invoke.counted(),
                            act: invoke.act,
                            root: self.root,
                        },
                        args: args.clone(),
                        resident: Some(HeapKernel::Read {
                            units,
                            to: Destination::Register,
                        }),
                        inputs,
                    });
                    self.result(reg, &args);
                    return Some(());
                }
                let after = self.start();
                let mut call_site = CallSite {
                    checked: invoke.checked || self.root_checked(),
                    inputs: None,
                    root: self.root,
                    library,
                    answers: library
                        .filter(|lib| self.exe.lib(*lib).arity == args.len())
                        .and_then(|lib| super::answer_depth(self.exe, lib)),
                    callee: declared,
                    close: closes.first().copied(),
                    pc,
                    reg,
                    act: invoke.act,
                    occurrence: invoke.counted(),
                    site: *site,
                    after,
                };
                if let Some((callee, reads)) = inputs {
                    let member = if callee.is_none() {
                        self.member_write(&call_site, &reads)
                    } else {
                        None
                    };
                    call_site.inputs = Some(Box::new(CallInputs {
                        member,
                        callee: callee.map(Operand::from),
                        args: reads.into_vec().into_iter().map(Operand::from).collect(),
                    }));
                }
                self.emit(Op::Call {
                    site: call_site,
                    read,
                    args: args.clone(),
                });
                if read {
                    self.live.retain(|at| *at != reg);
                }
                self.result(reg, &args);
            }
            Kind::Cond {
                cond,
                closes,
                then,
                otherwise,
            } => {
                self.expr(cond)?;
                self.open = None;
                let branch = self.emit(Op::Cond {
                    reg,
                    close: closes.first().copied(),
                    pc,
                    otherwise: 0,
                });
                self.live.retain(|at| *at != cond.reg);
                let outer = self.live.clone();
                self.arm(then, reg)?;
                let jump = self.emit(Op::Jump(0));
                self.live = outer;
                self.open = None;
                let target = self.ops.len();
                self.ops[branch].op = Op::Cond {
                    reg,
                    close: closes.first().copied(),
                    pc,
                    otherwise: target,
                };
                self.arm(otherwise, reg)?;
                self.ops[jump].op = Op::Jump(self.ops.len());
                self.open = None;
            }
            Kind::Let {
                local, value, body, ..
            } => {
                self.expr(value)?;
                self.emit(Op::Bind {
                    at: local.at as usize,
                    src: value.reg,
                });
                self.live.retain(|at| *at != value.reg);
                self.expr(body)?;
                self.emit(Op::Unbind(local.at as usize));
            }
            Kind::Make {
                make,
                items,
                ahead,
                miss,
            } => {
                let kind = make.kind();
                let members = if *make == Make::Map {
                    items.len() / 2
                } else {
                    items.len()
                };
                self.emit(Op::Reserve {
                    kind,
                    members,
                    miss: expr.start(self.stream),
                });
                for item in items {
                    self.expr(item)?;
                }
                let replay_guard = *make == Make::List
                    && *miss < pc
                    && items.first().is_some_and(|item| item.pc < *miss);
                let items: Box<[_]> = items.iter().map(|item| item.reg).collect();
                let keeps: Vec<_> = self
                    .captures
                    .iter_mut()
                    .filter(|capture| capture.0 == pc)
                    .map(|capture| {
                        capture.2 = true;
                        capture.1
                    })
                    .collect();
                for restart in keeps {
                    self.emit(Op::Keep(restart));
                }
                self.emit(Op::Make {
                    pc,
                    ahead: *ahead,
                    replay_guard,
                    make: *make,
                    items: items.clone(),
                    reg,
                    members,
                    miss: *miss,
                });
                self.result(reg, &items);
            }
            Kind::Record {
                fields,
                ahead,
                values,
                miss,
            } => {
                let names: Box<[_]> = self
                    .stream
                    .field_lists
                    .get(fields.ix())?
                    .iter()
                    .map(|name| lash_kernel_doc::arena::ShortText::new(name).map(Value::Text))
                    .collect::<Option<_>>()?;
                self.emit(Op::Reserve {
                    kind: lash_kernel_doc::ValueKind::Record,
                    members: names.len(),
                    miss: expr.start(self.stream),
                });
                for value in values {
                    self.expr(value)?;
                }
                let values: Box<[_]> = values.iter().map(|value| value.reg).collect();
                self.emit(Op::Record {
                    ahead: *ahead,
                    members: names.len(),
                    names,
                    values: values.clone(),
                    reg,
                    miss: *miss,
                });
                self.result(reg, &values);
            }
            Kind::Closure { code, miss } => {
                self.emit(Op::Closure {
                    pc,
                    code: *code,
                    reg,
                    miss: *miss,
                });
                self.result(reg, &[]);
            }
            Kind::Pin(value) => self.expr(value)?,
            Kind::Opaque => {
                self.emit(Op::Cold(Deferred::Source(pc)));
            }
            Kind::Act => {
                // A statement-root call was emitted from its root expression.
                if let Some(op) = self.root_action(pc)? {
                    self.open = None;
                    self.emit(op);
                    self.result(reg, &[]);
                }
            }
            Kind::Schedule { act, args, closes } => {
                let args = self.args(args)?;
                self.close(closes.first().copied(), reg);
                self.open = None;
                let op = match self.stream.nested[act.ix()].kind {
                    crate::compile::NestedKind::Spawn(_) => Op::Spawn(pc),
                    crate::compile::NestedKind::Yield => Op::Yield(pc),
                    crate::compile::NestedKind::Join | crate::compile::NestedKind::JoinMany(_) => {
                        Op::Join(pc)
                    }
                    crate::compile::NestedKind::Perform { .. } => Op::Wait(pc),
                    crate::compile::NestedKind::Cancel
                    | crate::compile::NestedKind::CancelEffect => Op::Cancel(pc),
                    _ => return None,
                };
                self.emit(op);
                self.result(reg, &args);
            }
        }
        Some(())
    }

    /// The scheduler instruction of a statement-root action; `None` within
    /// a statement-root call, whose root expression is its instruction.
    fn root_action(&self, pc: u32) -> Option<Option<Op>> {
        if !matches!(
            self.stream.ops.get(pc as usize),
            Some(lash_kernel_exec::Op::Act)
        ) {
            return None;
        }
        let crate::compile::Form::Action(action) = &self.exe.stmt(self.statement?).form else {
            return None;
        };
        Some(Some(match action.kind {
            crate::compile::ActionKind::Spawn { .. } => Op::Spawn(pc),
            crate::compile::ActionKind::Yield => Op::Yield(pc),
            crate::compile::ActionKind::Join(_) | crate::compile::ActionKind::JoinMany(..) => {
                Op::Join(pc)
            }
            crate::compile::ActionKind::Perform { .. } => Op::Wait(pc),
            crate::compile::ActionKind::Cancel(_) | crate::compile::ActionKind::CancelEffect(_) => {
                Op::Cancel(pc)
            }
            crate::compile::ActionKind::Call { .. } => return Some(None),
        }))
    }

    fn root_checked(&self) -> bool {
        self.root.is_some_and(|id| {
            matches!(
                &self.exe.stmt(id).form,
                crate::compile::Form::Action(crate::compile::Action {
                    kind: crate::compile::ActionKind::Call {
                        callee: crate::compile::Callee::CheckedHelper(_)
                            | crate::compile::Callee::Import(..),
                        ..
                    },
                    ..
                })
            )
        })
    }

    fn arm(&mut self, arm: &Arm, reg: u32) -> Option<()> {
        if let Some(value) = &arm.value {
            self.expr(value)?;
            self.close(arm.closes.first().copied(), reg);
        } else {
            self.emit(Op::Cold(Deferred::Source(arm.start)));
        }
        Some(())
    }
}

impl Op {
    pub(crate) fn scheduler_pc(&self) -> Option<u32> {
        match self {
            Self::Spawn(pc)
            | Self::Yield(pc)
            | Self::Join(pc)
            | Self::Wait(pc)
            | Self::Cancel(pc) => Some(*pc),
            _ => None,
        }
    }
}
