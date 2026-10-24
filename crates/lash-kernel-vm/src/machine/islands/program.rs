//! Flatten the admitted source tree once. No evaluator closure or control
//! recursion survives compilation; live source registers accompany each exit.

use super::*;
use crate::compile::expr::{Arm, Callee, Make};
use crate::compile::islands::{Block as SourceBlock, Kind as StatementKind, Statement, unit_test};

pub(in crate::machine::islands) struct Instruction {
    pub(in crate::machine::islands) op: Op,
    /// Evaluated operands, in source evaluation order, before this op.
    pub(in crate::machine::islands) live: Box<[u32]>,
}

pub(in crate::machine::islands) enum Op {
    Try {
        id: StmtId,
        finally: Option<usize>,
        after: usize,
    },
    Loop {
        id: StmtId,
        header: usize,
    },
    For {
        id: StmtId,
        header: usize,
        reg: u32,
        pc: u32,
    },
    Header {
        repeat: Option<Box<RepeatSite>>,
        id: StmtId,
        cost: u64,
        metered: u64,
        at: Option<usize>,
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
    Begin {
        id: StmtId,
        index: usize,
        ahead: u64,
    },
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
    Constant {
        dst: u32,
        value: Value,
    },
    Local {
        dst: u32,
        at: usize,
        cell: Option<usize>,
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
    Native {
        site: NativeSite,
        args: Box<[u32]>,
    },
    Heap {
        site: HeapSite,
        args: Box<[u32]>,
    },
    Reserve {
        kind: lash_kernel_doc::ValueKind,
        members: usize,
        miss: u32,
    },
    Make {
        make: Make,
        items: Box<[u32]>,
        reg: u32,
        members: usize,
        miss: u32,
    },
    Record {
        names: Box<[Value]>,
        values: Box<[u32]>,
        reg: u32,
        members: usize,
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
        at: usize,
        cell: Option<usize>,
        reg: u32,
        pc: u32,
        bind: bool,
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
    },
    Cold(u32),
}

pub(super) struct Build<'e> {
    exe: &'e Executable,
    captures: &'e [u32],
    pub(super) room: u32,
    pub(super) observes: bool,
    pub(super) segments: Vec<u64>,
    measured: Option<Vec<u64>>,
    open: Option<usize>,
    pub(super) blocks: Vec<Block>,
    pub(super) ops: Vec<Instruction>,
    pub(super) registers: usize,
    live: Vec<u32>,
}

impl<'e> Build<'e> {
    pub(super) fn new(
        exe: &'e Executable,
        captures: &'e [u32],
        measured: Option<Vec<u64>>,
    ) -> Self {
        Self {
            exe,
            captures,
            room: 0,
            observes: false,
            segments: Vec::new(),
            measured,
            open: None,
            blocks: Vec::new(),
            ops: Vec::new(),
            registers: 0,
            live: Vec::new(),
        }
    }

    fn emit(&mut self, op: Op) -> usize {
        let pc = self.ops.len();
        self.ops.push(Instruction {
            op,
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

    fn site(&mut self, depth: Depth, observes: bool) {
        let room = match depth {
            Depth::None | Depth::Operation(0 | 1) => 0,
            Depth::Operation(extra) | Depth::Template(extra) => extra,
        };
        self.room = self.room.max(room);
        self.observes |= observes;
    }

    pub(super) fn block(&mut self, block: &SourceBlock) -> Option<()> {
        let index = self.blocks.len();
        self.blocks.push(Block {
            id: block.id,
            declares: block.declares.clone(),
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
        if !literal_entry {
            self.emit(Op::Open(index));
        }
        let entry = self.ops.len();
        for (index, statement) in block.statements.iter().enumerate() {
            self.statement(statement, index)?;
        }
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
        if !self.writes_last(statement.id) {
            return None;
        }
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
        self.emit(Op::Begin {
            id: statement.id,
            index,
            ahead,
        });
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
                });
                let header = self.emit(Op::Header {
                    repeat: None,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: None,
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
                self.block(body)?;
                let exhausted = self.ops.len();
                self.loop_else(otherwise.as_ref(), statement.leaves)?;
                let after = self.ops.len();
                self.ops[start].op = Op::Loop {
                    id: statement.id,
                    header,
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
                self.expr(value)?;
                self.close(*close, *reg);
                self.open = None;
                let start = self.emit(Op::For {
                    id: statement.id,
                    header: 0,
                    reg: *reg,
                    pc: *pc,
                });
                self.live.clear();
                let header = self.emit(Op::Header {
                    repeat: None,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: Some(*at as usize),
                    otherwise: 0,
                    after: 0,
                });
                self.block(body)?;
                let exhausted = self.ops.len();
                self.loop_else(otherwise.as_ref(), statement.leaves)?;
                let after = self.ops.len();
                self.ops[start].op = Op::For {
                    id: statement.id,
                    header,
                    reg: *reg,
                    pc: *pc,
                };
                self.ops[header].op = Op::Header {
                    repeat: None,
                    id: statement.id,
                    cost: *cost,
                    metered: *metered,
                    at: Some(*at as usize),
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
            } => {
                self.expr(value)?;
                self.close(*close, *reg);
                self.emit(Op::Write {
                    at: *at as usize,
                    cell: self.captures.iter().position(|capture| capture == at),
                    reg: *reg,
                    pc: *pc,
                    bind: *bind,
                });
                self.emit(Op::Done {
                    leaves: statement.leaves,
                    empty: None,
                });
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
            StatementKind::Discard { value, close, reg } => {
                self.expr(value)?;
                self.close(*close, *reg);
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
                self.expr(cond)?;
                self.close(*close, *reg);
                self.open = None;
                let branch = self.emit(Op::Branch {
                    reg: *reg,
                    pc: *pc,
                    otherwise: 0,
                });
                self.branch(then_, statement.leaves)?;
                let jump = self.emit(Op::Jump(0));
                let otherwise = self.ops.len();
                self.ops[branch].op = Op::Branch {
                    reg: *reg,
                    pc: *pc,
                    otherwise,
                };
                self.branch(else_, statement.leaves)?;
                self.ops[jump].op = Op::Jump(self.ops.len());
            }
            StatementKind::Cold => {
                self.emit(Op::Cold(self.exe.stmt(statement.id).ops.start));
            }
        }
        self.open = None;
        self.live.clear();
        Some(())
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
        match &expr.kind {
            Kind::Local { local, miss, .. } => {
                self.emit(Op::Local {
                    dst: reg,
                    at: local.at as usize,
                    cell: self
                        .captures
                        .iter()
                        .position(|capture| *capture == local.at),
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
                let Some(site) = operator_site(expr, self.exe) else {
                    let function = self.exe.lib(*lib);
                    let (entry, _) = function.prepared.as_ref()?;
                    let extra =
                        entry.deepest(&|id| self.exe.lib(id), &|id| self.exe.library_lib(id));
                    let depth = if function.operation() {
                        Depth::Operation(extra)
                    } else {
                        Depth::Template(extra)
                    };
                    self.site(
                        depth,
                        invoke.is_some_and(|invoke| invoke.call)
                            && matches!(function.run, LibRun::DeferredPlan { .. })
                            && self.exe.fallback(*lib).is_none(),
                    );
                    let args = self.args(args)?;
                    self.open = None;
                    self.emit(Op::Heap {
                        site: HeapSite {
                            lib: *lib,
                            close: closes.first().copied(),
                            pc,
                            reg,
                            depth,
                            checked: invoke.is_some_and(|invoke| invoke.checked),
                            counted: invoke
                                .filter(|invoke| invoke.call)
                                .map(|invoke| invoke.occurrence),
                        },
                        args: args.clone(),
                    });
                    self.result(reg, &args);
                    return Some(());
                };
                self.site(site.depth, site.body_when_observed);
                let args = self.args(args)?;
                let covered = site.entry.pair().is_some()
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
                    length: self.exe.lib(*lib).length_read(),
                };
                self.site(site.depth, false);
                let args = self.args(args)?;
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
                    site: crate::compile::ReadSite::new(site.0, self.exe.document_stream().layout),
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
                self.emit(Op::Call {
                    site: CallSite {
                        callee: declared,
                        close: closes.first().copied(),
                        pc,
                        reg,
                        act: invoke.act,
                        occurrence: invoke.occurrence,
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
            Kind::Make { make, items, miss } => {
                let kind = make.kind();
                let members = if *make == Make::Map {
                    items.len() / 2
                } else {
                    items.len()
                };
                self.emit(Op::Reserve {
                    kind,
                    members,
                    miss: expr.start(self.exe.document_stream()),
                });
                for item in items {
                    self.expr(item)?;
                }
                let items: Box<[_]> = items.iter().map(|item| item.reg).collect();
                self.emit(Op::Make {
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
                values,
                miss,
            } => {
                let names: Box<[_]> = self
                    .exe
                    .document_stream()
                    .field_lists
                    .get(fields.ix())?
                    .iter()
                    .map(|name| lash_kernel_doc::arena::ShortText::new(name).map(Value::Text))
                    .collect::<Option<_>>()?;
                self.emit(Op::Reserve {
                    kind: lash_kernel_doc::ValueKind::Record,
                    members: names.len(),
                    miss: expr.start(self.exe.document_stream()),
                });
                for value in values {
                    self.expr(value)?;
                }
                let values: Box<[_]> = values.iter().map(|value| value.reg).collect();
                self.emit(Op::Record {
                    members: names.len(),
                    names,
                    values: values.clone(),
                    reg,
                    miss: *miss,
                });
                self.result(reg, &values);
            }
            Kind::Pin(value) => self.expr(value)?,
            Kind::Act => return None,
        }
        Some(())
    }

    fn writes_last(&self, id: StmtId) -> bool {
        let stream = self.exe.document_stream();
        let ops =
            &stream.ops[self.exe.stmt(id).ops.start as usize..self.exe.stmt(id).ops.end() as usize];
        let before = match ops {
            [before @ .., _, lash_kernel_exec::Op::End] => before,
            _ => ops,
        };
        !before.iter().any(|op| {
            let lib = match op {
                lash_kernel_exec::Op::Operator { lib, .. } => Some(LibId(lib.0)),
                lash_kernel_exec::Op::Nested { act, .. } => stream
                    .nested
                    .get(act.ix())
                    .and_then(|nested| nested.kind.operator_lib()),
                _ => None,
            };
            lib.is_some_and(|lib| {
                let function = self.exe.lib(lib);
                !matches!(function.run, LibRun::Native { .. })
                    && function.prepared.as_ref().is_some_and(|(entry, _)| {
                        entry.writes(&|id| self.exe.lib(id), &|id| self.exe.library_lib(id))
                    })
            })
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
        let kind = operator_site(inner, self.exe)?;
        let same = operator_site(expr, self.exe)?;
        if kind.counted.is_some() || same.counted.is_some() {
            return Some(false);
        }
        self.site(kind.depth, kind.body_when_observed);
        self.site(same.depth, same.body_when_observed);
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
