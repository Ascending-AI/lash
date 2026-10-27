//! Flatten the admitted source tree once. No evaluator closure or control
//! recursion survives compilation; live source registers accompany each exit.

use super::*;

#[path = "program/accumulation.rs"]
mod accumulation;
#[path = "program/numeric.rs"]
mod numeric;
pub(in crate::machine::islands) use numeric::{CallInputs, Destination, Operand};
#[path = "program/access.rs"]
mod access;
pub(in crate::machine::islands) use access::{AccessShape, AccessSite};
#[path = "program/guarded.rs"]
mod guarded;
#[path = "program/merge.rs"]
mod merge;
use crate::compile::expr::{Arm, Callee, Make};
use crate::compile::islands::{Block as SourceBlock, Kind as StatementKind, Statement, unit_test};
pub(in crate::machine::islands) use merge::{CollectionStep, Merge, Placement, ReadSlot, Visit};

pub(in crate::machine::islands) struct Instruction {
    pub(in crate::machine::islands) op: Op,
    /// Source statement admission immediately before its first operation.
    pub(in crate::machine::islands) begin: Option<StatementEntry>,
    /// Evaluated operands, in source evaluation order, before this op.
    pub(in crate::machine::islands) live: Box<[u32]>,
}

/// A pure source operand, read once in its original argument order.
/// Failure retains the operand's source PC and the completed prefix.
#[derive(Clone, Copy)]
pub(in crate::machine::islands) enum Read {
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
    pub(in crate::machine::islands) fn reg(&self) -> u32 {
        match self {
            Self::Literal { reg, .. } | Self::Local { reg, .. } => *reg,
        }
    }
}

pub(in crate::machine::islands) enum Op {
    Collection {
        step: std::sync::Arc<CollectionStep>,
        tail: bool,
        original: Box<Op>,
    },
    Try {
        id: StmtId,
        finally: Option<usize>,
        after: usize,
    },
    /// A `while`: its entry ends the ownership of the texts its
    /// accumulators made in an earlier activation.
    Loop {
        id: StmtId,
        header: usize,
        texts: Box<[usize]>,
    },
    /// A linear text accumulator's statement (FIG-6795) takes the long text
    /// of the variable at its slot, owned where its own write made it.
    Accumulate(usize),
    /// A `for` over the value in `reg`. One through a protocol (`via`)
    /// opens at source op `pc`, where its `open` hook runs.
    For {
        id: StmtId,
        header: usize,
        reg: u32,
        pc: u32,
        via: bool,
        texts: Box<[usize]>,
    },
    Header {
        repeat: Option<Box<RepeatSite>>,
        id: StmtId,
        cost: u64,
        metered: u64,
        at: Option<usize>,
        /// Its protocol has a hook that runs at each step (`next`, `check`
        /// or `item`): the source header steps the loop and runs it.
        hooked: bool,
        otherwise: usize,
        after: usize,
    },
    LoopBranch {
        reg: u32,
        pc: u32,
        otherwise: usize,
    },
    EndLoop,
    LoopDone(u32),
    Depart {
        continuing: bool,
    },
    Open(usize),
    /// A constant return is one semantic statement, without an operand
    /// window or extra dispatch between its unit and its fixed close.
    ReturnLiteral {
        entry: Option<usize>,
        id: StmtId,
        index: usize,
        ahead: u64,
        value: Value,
        close: Option<Close>,
        covered: bool,
        reg: u32,
    },
    Keep(super::Restart),
    Constant {
        dst: u32,
        value: Value,
    },
    Local {
        dst: u32,
        at: usize,
        shared: bool,
        miss: u32,
    },
    Session {
        dst: u32,
        name: u32,
        miss: u32,
    },
    Operator {
        site: Box<OperatorSite>,
        args: Box<[u32]>,
        covered: bool,
    },
    /// Pure resident operands need no intermediate register dispatches.
    DirectOperator {
        site: Box<OperatorSite>,
        reads: Box<[Read]>,
        covered: bool,
    },
    /// Fixed arity scalar work with source-owned kernels and destinations.
    Unary {
        site: Box<OperatorSite>,
        arg: Operand,
        covered: bool,
        to: Destination,
    },
    Binary {
        kernel: lash_kernel_doc::scalar::Pair,
        site: Box<OperatorSite>,
        left: Operand,
        right: Operand,
        covered: bool,
        to: Destination,
    },
    Native {
        site: NativeSite,
        args: Box<[u32]>,
    },
    DirectNative {
        site: NativeSite,
        reads: Box<[Read]>,
    },
    Heap {
        site: HeapSite,
        args: Box<[u32]>,
    },
    PropertyStore {
        site: HeapSite,
        args: Box<[u32]>,
        reads: Option<Box<[Read]>>,
        property: Box<super::PropertyWrite>,
    },
    Reserve {
        kind: lash_kernel_doc::ValueKind,
        members: usize,
        miss: u32,
    },
    Make {
        ahead: u64,
        make: Make,
        items: Box<[u32]>,
        reg: u32,
        members: usize,
        miss: u32,
    },
    Record {
        ahead: u64,
        names: Box<[Value]>,
        values: Box<[u32]>,
        reg: u32,
        members: usize,
        miss: u32,
    },
    Closure {
        code: CodeId,
        reg: u32,
        miss: u32,
    },
    Store {
        target: u32,
        index: Option<u32>,
        value: u32,
        field: Option<usize>,
        pc: u32,
    },
    Unit {
        site: UnitSite,
        src: u32,
    },
    Kind {
        site: Box<KindTest>,
        src: u32,
    },
    DirectKind {
        site: Box<KindTest>,
        reads: Box<[Read]>,
    },
    Field {
        dst: u32,
        field: usize,
        miss: u32,
    },
    Index {
        site: Option<crate::compile::ReadSite>,
        dst: u32,
        miss: u32,
    },
    Call {
        site: CallSite,
        read: bool,
        args: Box<[u32]>,
    },
    Bind {
        at: usize,
        src: u32,
    },
    Unbind(usize),
    Cond {
        reg: u32,
        close: Option<Close>,
        pc: u32,
        otherwise: usize,
    },
    Jump(usize),
    Close {
        close: Close,
        covered: bool,
    },
    Write {
        site: Box<WriteSite>,
    },
    Return {
        reg: u32,
        pc: u32,
    },
    Done {
        leaves: u32,
        empty: Option<crate::compile::BlockId>,
    },
    Branch {
        reg: u32,
        pc: u32,
        otherwise: usize,
        opens: [Option<usize>; 2],
    },
    /// A session binding's write of the value in `reg`, at source op `pc`.
    Publish {
        name: u32,
        reg: u32,
        pc: u32,
        bind: bool,
    },
    /// A statement's host or run effect on its live value, at source op
    /// `pc`: its source handler prints, finishes, fails or raises it.
    Effect(u32),
    /// Task state is claimed only after the physical windows materialize.
    Spawn(u32),
    Yield(u32),
    Join(u32),
    Wait(u32),
    Cancel(u32),
    Cold(u32),
    /// A resident member read with its complete source guard and completion edge.
    Access(Box<AccessSite>),
    /// A source initialization immediately followed by its compact read.
    AccessInit {
        head: usize,
        original: Box<WriteSite>,
    },
    /// A local's binary update and its write share one instruction.
    Accumulation {
        site: Box<OperatorSite>,
        append: Option<Box<Append>>,
        reads: Box<[Read]>,
        covered: bool,
        write: Box<WriteSite>,
    },
}

/// The most arms a guarded read enters.
pub(in crate::machine::islands) const GUARDED_ARMS: usize = 4;

pub(in crate::machine::islands) struct StatementEntry {
    pub(in crate::machine::islands) id: StmtId,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) ahead: u64,
}

pub(in crate::machine::islands) struct WriteSite {
    pub(in crate::machine::islands) begin: Option<StatementEntry>,
    pub(in crate::machine::islands) reads: Option<Box<[Read]>>,
    pub(in crate::machine::islands) at: usize,
    pub(in crate::machine::islands) shared: bool,
    pub(in crate::machine::islands) reg: u32,
    pub(in crate::machine::islands) pc: u32,
    pub(in crate::machine::islands) bind: bool,
    pub(in crate::machine::islands) close: Option<Close>,
    pub(in crate::machine::islands) covered: bool,
    pub(in crate::machine::islands) leaves: u32,
    /// A linear text accumulator's write (FIG-6795).
    pub(in crate::machine::islands) accumulates: bool,
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
        let literal_entry = index == 0
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
        if let StatementKind::Return {
            value: Expr {
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
                    hooked: false,
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
                let repeat = self
                    .exe
                    .islands
                    .scalar
                    .iter()
                    .find(|plan| plan.stmt == statement.id)
                    .and_then(|plan| {
                        Some(Box::new(RepeatSite {
                            run: plan.typed_repeat(self.exe)?,
                            iteration_max: plan.iteration_max,
                        }))
                    });
                self.ops[header].op = Op::Header {
                    repeat,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: None,
                    hooked: false,
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
                let hooked = via.is_some_and(|via| {
                    via.next.is_some() || via.check.is_some() || via.item.is_some()
                });
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
                    hooked,
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
                    hooked,
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
                for operand in operands {
                    self.expr(operand)?;
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
                self.emit(Op::Cold(self.exe.stmt(statement.id).ops.start));
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
            Kind::Operator {
                lib, args, closes, ..
            } if unit_test(Some(self.exe.lib(*lib)), args) => {
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
                        lib: *lib,
                        close: closes.first().copied(),
                        miss,
                        reg,
                    },
                    src: value.reg,
                });
                self.result(reg, &[value.reg]);
            }
            Kind::Operator { lib, args, .. } if self.kind(expr, *lib, args)? => {}
            Kind::Operator {
                args,
                lib,
                invoke,
                closes,
            } => {
                let Some(mut site) = operator_site(expr, self.exe) else {
                    let function = self.exe.lib(*lib);
                    let extra = function.prepared.as_ref().map_or(0, |(entry, _)| {
                        entry.deepest(&|id| self.exe.lib(id), &|id| self.exe.library_lib(id))
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
                    let args = self.args(args)?;
                    let close = self.ordered_closes(closes, reg);
                    self.open = None;
                    self.emit(Op::Heap {
                        site: HeapSite {
                            lib: *lib,
                            close,
                            pc,
                            reg,
                            depth,
                            checked: self.root_checked()
                                || invoke.is_some_and(|invoke| invoke.checked),
                            counted: invoke.and_then(|invoke| invoke.counted()),
                            act: invoke
                                .filter(|invoke| invoke.call)
                                .and_then(|invoke| invoke.act),
                            root: self.root,
                        },
                        args: args.clone(),
                    });
                    self.result(reg, &args);
                    return Some(());
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
                let direct = (closes.len() <= 1).then(|| self.reads(args)).flatten();
                if let Some(reads) = direct {
                    let consumed: Vec<_> = reads.iter().map(Read::reg).collect();
                    let covered = site.pair.is_some()
                        && reads.len() == 2
                        && self.cover(site.fixed.saturating_add(site.table.max()));
                    if !covered {
                        self.open = None;
                    }
                    self.emit(Op::DirectOperator {
                        site: Box::new(site),
                        reads,
                        covered,
                    });
                    self.result(reg, &consumed);
                    return Some(());
                }
                let args = self.args(args)?;
                site.close = self.ordered_closes(closes, reg);
                site.fixed = site.close.map_or(0, |close| u64::from(close.total));
                let covered = site.pair.is_some()
                    && args.len() == 2
                    && self.cover(site.fixed.saturating_add(site.table.max()));
                if !covered {
                    self.open = None;
                }
                self.emit(Op::Operator {
                    site: Box::new(site),
                    args: args.clone(),
                    covered,
                });
                self.result(reg, &args);
            }
            Kind::Native { lib, args, closes } => {
                let extra = self
                    .exe
                    .lib(*lib)
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
                    direct: {
                        let lib = self.exe.lib(*lib);
                        (lib.length_read().map(Direct::Length))
                            .or_else(|| lib.item_read().map(Direct::Item))
                            .or_else(|| lib.membership().map(Direct::Member))
                    },
                };
                self.site(site.depth, false);
                if closes.len() <= 1
                    && let Some(reads) = self.reads(args)
                {
                    let consumed: Vec<_> = reads.iter().map(Read::reg).collect();
                    self.open = None;
                    self.emit(Op::DirectNative { site, reads });
                    self.result(reg, &consumed);
                    return Some(());
                }
                let args = self.args(args)?;
                site.close = self.ordered_closes(closes, reg);
                self.open = None;
                self.emit(Op::Native {
                    site,
                    args: args.clone(),
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
                let (read, declared) = match callee {
                    Callee::Read(read) => {
                        self.expr(read)?;
                        (true, None)
                    }
                    Callee::Declared(code) => (false, Some(*code)),
                };
                let args = self.args(args)?;
                self.open = None;
                let after = self.start();
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
                self.emit(Op::Call {
                    site: CallSite {
                        checked: invoke.checked || self.root_checked(),
                        inputs: None,
                        root: self.root,
                        library,
                        answers: library.and_then(|lib| super::answer_depth(self.exe, lib)),
                        callee: declared,
                        close: closes.first().copied(),
                        pc,
                        reg,
                        act: invoke.act,
                        occurrence: invoke.counted(),
                        site: *site,
                        after,
                    },
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
                    ahead: *ahead,
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
                    code: *code,
                    reg,
                    miss: *miss,
                });
                self.result(reg, &[]);
            }
            Kind::Pin(value) => self.expr(value)?,
            Kind::Opaque => {
                self.emit(Op::Cold(pc));
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
                    crate::compile::NestedKind::Cancel => Op::Cancel(pc),
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
            crate::compile::ActionKind::Cancel(_) => Op::Cancel(pc),
            crate::compile::ActionKind::Call { .. } => return Some(None),
        }))
    }

    fn reads(&mut self, args: &[Expr]) -> Option<Box<[Read]>> {
        let reads = args
            .iter()
            .map(|arg| match arg.kind {
                Kind::Const(value) => Some(Read::Literal {
                    reg: arg.reg,
                    value,
                }),
                Kind::Local { local, miss, .. } => Some(Read::Local {
                    reg: arg.reg,
                    at: local.at as usize,
                    shared: self.shared[local.at as usize],
                    miss,
                }),
                _ => None,
            })
            .collect::<Option<Box<[_]>>>()?;
        for read in &reads {
            self.registers = self.registers.max(read.reg() as usize + 1);
        }
        Some(reads)
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
            self.emit(Op::Cold(arm.start));
        }
        Some(())
    }

    fn kind(&mut self, expr: &Expr, lib: LibId, args: &[Expr]) -> Option<bool> {
        let Some((inner, literal)) = crate::compile::kind_test::test(self.exe, lib, args) else {
            return Some(false);
        };
        let Kind::Operator {
            args: inner_args, ..
        } = &inner.kind
        else {
            return Some(false);
        };
        if [inner, expr]
            .iter()
            .any(|expr| matches!(&expr.kind, Kind::Operator { closes, .. } if closes.len() > 1))
        {
            return Some(false);
        }
        let kind = operator_site(inner, self.exe)?;
        let same = operator_site(expr, self.exe)?;
        if kind.counted.is_some() || same.counted.is_some() {
            return Some(false);
        }
        self.site(kind.depth, kind.body_when_observed);
        self.site(same.depth, same.body_when_observed);
        if let Some(reads) = self.reads(std::slice::from_ref(&inner_args[0])) {
            self.open = None;
            self.emit(Op::DirectKind {
                site: Box::new(KindTest {
                    kind,
                    same,
                    literal,
                    memo: Default::default(),
                }),
                reads,
            });
            self.result(expr.reg, &[inner_args[0].reg]);
            return Some(true);
        }
        self.expr(&inner_args[0])?;
        self.open = None;
        self.emit(Op::Kind {
            site: Box::new(KindTest {
                kind,
                same,
                literal,
                memo: Default::default(),
            }),
            src: inner_args[0].reg,
        });
        self.result(expr.reg, &[inner_args[0].reg]);
        Some(true)
    }
}

impl Op {
    pub(in crate::machine::islands) fn scheduler_pc(&self) -> Option<u32> {
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
