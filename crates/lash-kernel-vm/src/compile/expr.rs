//! Source stream grammar retained by the independent oracle's exact boundaries.
//! This tracks register consumption and lexical nesting without building a
//! second executable expression program.
use super::{CodeId, Constant, LibId, Stream};
use lash_kernel_doc::JoinMode;
use lash_kernel_exec::{ActIx, LocalRef, Op, OpRange, Reg, RegRange};
use lash_kernel_value::Type;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkedHook {
    Library(LibId),
    Code(CodeId),
    Import(lash_kernel_value::names::ImportSlot, Option<LibId>),
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
    CancelEffect,
    /// A `call`, `apply` or `invoke`: of a declared function, a closure or
    /// function reference, or a library function, which enters a frame
    /// when it runs its body.
    Call(NestedCallee),
    /// A library call inside an expression of a function that runs its
    /// body here: it enters a frame, as a call does, but it is no action,
    /// so no occurrence of its site is counted.
    Body(LibId),
    Perform {
        slot: lash_kernel_value::names::ImportSlot,
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
    Import(lash_kernel_value::names::ImportSlot, Option<LibId>),
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

/// The source value shape an exact loop-input receipt needs.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Local { scoped: bool },
    Other,
}

pub(crate) struct Statement {
    pub(crate) last: Op,
    pub(crate) closes: Box<[Close]>,
    regs: Vec<Option<Kind>>,
}
impl Statement {
    pub(crate) fn take(&mut self, reg: Reg) -> Option<Kind> {
        self.regs.get_mut(reg.ix())?.take()
    }
    pub(crate) fn spent(&self) -> bool {
        self.regs.iter().all(Option::is_none)
    }
}

pub(crate) fn statement(stream: &Stream, range: OpRange, rooted: bool) -> Option<Statement> {
    let end = range.end().checked_sub(1)?;
    let last = *stream.ops.get(end as usize)?;
    let mut reader = Reader {
        stream,
        regs: Vec::new(),
        open: Vec::new(),
        lets: Vec::new(),
        scope: Vec::new(),
    };
    let mut start = range.start;
    if rooted {
        if !matches!(stream.ops.get(start as usize), Some(Op::Act)) {
            return None;
        }
        reader.define(Reg(0), Kind::Other)?;
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

struct Reader<'a> {
    stream: &'a Stream,
    regs: Vec<Option<Kind>>,
    open: Vec<Close>,
    lets: Vec<(LocalRef, Reg)>,
    scope: Vec<u32>,
}
impl Reader<'_> {
    fn define(&mut self, reg: Reg, kind: Kind) -> Option<()> {
        if self.regs.len() <= reg.ix() {
            self.regs.resize(reg.ix() + 1, None);
        }
        if self.regs[reg.ix()].is_some() {
            return None;
        }
        self.regs[reg.ix()] = Some(kind);
        Some(())
    }
    fn take(&mut self, reg: Reg) -> Option<Kind> {
        self.regs.get_mut(reg.ix())?.take()
    }
    fn operands(&mut self, args: RegRange) -> Option<()> {
        for reg in args.regs() {
            self.take(reg)?;
        }
        Some(())
    }
    fn nested(&mut self, act: ActIx, args: RegRange) -> Option<()> {
        let nested = self.stream.nested.get(act.ix())?;
        if nested.kind.operator_lib().is_none()
            && !matches!(
                nested.kind,
                NestedKind::Spawn(_)
                    | NestedKind::Perform { .. }
                    | NestedKind::Join
                    | NestedKind::JoinMany(_)
                    | NestedKind::Yield
                    | NestedKind::Cancel
                    | NestedKind::CancelEffect
                    | NestedKind::Call(NestedCallee::Declared(_))
            )
            && !matches!(nested.kind, NestedKind::Call(NestedCallee::Read) if args.len >= 1)
        {
            return None;
        }
        self.operands(args)?;
        self.open.clear();
        Some(())
    }
    fn ops(&mut self, start: u32, end: u32) -> Option<()> {
        let mut pc = start;
        while pc < end {
            let op = *self.stream.ops.get(pc as usize)?;
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
                        scoped: self.scope.contains(&local.at),
                    },
                ),
                Op::Session { dst, .. } | Op::Closure { dst, .. } => (dst, Kind::Other),
                Op::Const { dst, c } => {
                    match self.stream.consts.get(c.ix())? {
                        Constant::Resident(_)
                        | Constant::Deferred(lash_kernel_doc::Literal::Text(_)) => {}
                        Constant::Deferred(_) => return None,
                    }
                    (dst, Kind::Other)
                }
                Op::Operator { args, .. } | Op::Call { args, .. } => {
                    self.operands(args)?;
                    self.open.clear();
                    (args.start, Kind::Other)
                }
                Op::Nested { act, args } => {
                    self.nested(act, args)?;
                    (args.start, Kind::Other)
                }
                Op::MakeTuple { items } | Op::MakeList { items } | Op::MakeSet { items } => {
                    self.operands(items)?;
                    (items.start, Kind::Other)
                }
                Op::MakeMap { entries } => {
                    self.operands(entries)?;
                    (entries.start, Kind::Other)
                }
                Op::MakeRecord { values, .. } => {
                    self.operands(values)?;
                    (values.start, Kind::Other)
                }
                Op::Field { at, .. } => {
                    self.take(at)?;
                    (at, Kind::Other)
                }
                Op::Index { at, .. } => {
                    self.take(at)?;
                    self.take(Reg(at.0 + 1))?;
                    (at, Kind::Other)
                }
                Op::Pin { src } => {
                    self.take(src)?;
                    (src, Kind::Other)
                }
                Op::LetBind { local, src } => {
                    self.take(src)?;
                    self.lets.push((local, src));
                    self.scope.push(local.at);
                    pc += 1;
                    continue;
                }
                Op::LetEnd { local } => {
                    let (bound, reg) = self.lets.pop()?;
                    self.scope.pop();
                    if bound.at != local.at {
                        return None;
                    }
                    self.take(reg)?;
                    (reg, Kind::Other)
                }
                Op::JumpIfFalse { cond, skip } => {
                    self.open.clear();
                    self.take(cond)?;
                    let then_end = pc.checked_add(1)?.checked_add(skip)?;
                    let Op::Jump { skip: rest } =
                        *self.stream.ops.get(then_end.checked_sub(1)? as usize)?
                    else {
                        return None;
                    };
                    self.define(cond, Kind::Other)?;
                    pc = then_end.checked_add(rest)?;
                    continue;
                }
                _ => return None,
            };
            self.define(reg, kind)?;
            pc += 1;
        }
        Some(())
    }
}
