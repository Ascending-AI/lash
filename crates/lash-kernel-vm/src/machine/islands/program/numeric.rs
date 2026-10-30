//! Lower scalar sites after guarded paths have claimed their instructions.
//! There is one final inventory: fused consumers are removed, and every
//! physical edge is projected through the same relocation table.

use super::*;
use crate::machine::islands::MAX_ARGS;
use std::collections::BTreeSet;

impl Build<'_> {
    pub(in crate::machine::islands) fn numeric(&mut self) {
        let mut protected = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for instruction in &mut self.ops {
            instruction.op.addresses(&mut |at| {
                targets.insert(*at);
            });
            if let Op::Access(site) = &instruction.op {
                protected.extend(site.source.iter().copied());
            }
        }
        for block in &self.blocks {
            targets.extend(block.starts.iter().copied());
            targets.extend([block.end, block.after]);
        }
        let removed = self.source_inputs(&targets, &protected);
        let mut relocation = vec![0; self.ops.len() + 1];
        let source = std::mem::take(&mut self.ops);
        self.ops = Vec::with_capacity(source.len() - removed.len());
        let mut old = source.into_iter().enumerate().peekable();
        while let Some((at, mut instruction)) = old.next() {
            relocation[at] = self.ops.len();
            if removed.contains(&at) {
                continue;
            }
            if !protected.contains(&at) {
                instruction.op = fixed(instruction.op, self.exe);
                if let Op::Heap {
                    resident: Some(HeapKernel::Write { finish, .. }),
                    ..
                }
                | Op::Store { finish, .. } = &mut instruction.op
                    && let Some((next, following)) = old.peek()
                    && !targets.contains(next)
                    && following.begin.is_none()
                    && matches!(following.op, Op::Done { empty: None, .. })
                {
                    let Some((next, following)) = old.next() else {
                        unreachable!()
                    };
                    relocation[next] = self.ops.len();
                    let Op::Done { leaves, .. } = following.op else {
                        unreachable!()
                    };
                    *finish = Some(leaves);
                }
                let candidate = match &instruction.op {
                    Op::Operator {
                        site,
                        inputs: OperatorInputs::Scalar(_),
                        to: Destination::Register,
                        ..
                    } => Some(site.reg),
                    Op::Heap {
                        site,
                        resident:
                            Some(HeapKernel::Read {
                                to: Destination::Register,
                                ..
                            }),
                        ..
                    } => Some(site.reg),
                    _ => None,
                };
                if let Some(reg) = candidate
                    && let Some((next, following)) = old.peek()
                    && !protected.contains(next)
                    && !targets.contains(next)
                    && following.begin.is_none()
                    && matches!(&following.op, Op::Write { site } if site.reg == reg && site.begin.is_none() && site.reads.is_none())
                {
                    let Some((next, following)) = old.next() else {
                        unreachable!("the write was matched");
                    };
                    relocation[next] = self.ops.len();
                    let Op::Write { site: write } = following.op else {
                        unreachable!()
                    };
                    match &mut instruction.op {
                        Op::Operator { to, .. }
                        | Op::Heap {
                            resident: Some(HeapKernel::Read { to, .. }),
                            ..
                        } => *to = Destination::Write(write),
                        _ => unreachable!(),
                    }
                }
            }
            self.ops.push(instruction);
        }
        let end = relocation.len() - 1;
        relocation[end] = self.ops.len();
        crate::machine::islands::code::relocate(&mut self.ops, &mut self.blocks, &relocation);
    }

    fn source_inputs(
        &mut self,
        targets: &BTreeSet<usize>,
        protected: &BTreeSet<usize>,
    ) -> BTreeSet<usize> {
        let mut removed = BTreeSet::new();
        for at in 0..self.ops.len() {
            if let Some(range) = self.value_input(at, targets, protected) {
                removed.extend(range);
            }
        }
        removed
    }

    fn value_input(
        &mut self,
        at: usize,
        targets: &BTreeSet<usize>,
        protected: &BTreeSet<usize>,
    ) -> Option<std::ops::Range<usize>> {
        let count = match &self.ops[at].op {
            Op::Store {
                index: Some(_),
                field: None,
                ..
            } => 3,
            _ => return None,
        };
        let start = at.checked_sub(count)?;
        if !self.pure_group(start, at, targets, protected) {
            return None;
        }
        let reads = self.ops[start..at]
            .iter()
            .map(|instruction| Read::source(&instruction.op))
            .collect::<Option<Vec<_>>>()?;
        match &mut self.ops[at].op {
            Op::Store {
                target,
                index: Some(index),
                value,
                field: None,
                inputs,
                ..
            } => {
                let reads = <[Read; 3]>::try_from(reads).ok()?;
                let roles = [*target, *index, *value]
                    .map(|reg| reads.iter().position(|read| read.reg() == reg));
                let [Some(target), Some(index), Some(value)] = roles else {
                    return None;
                };
                if target == index || target == value || index == value {
                    return None;
                }
                *inputs = Some(Box::new(StoreInputs {
                    reads,
                    roles: [target, index, value],
                }));
            }
            _ => unreachable!(),
        }
        let begin = self.ops[start].begin.take();
        let live = self.ops[start].live.clone();
        self.ops[at].begin = begin;
        self.ops[at].live = live;
        Some(start..at)
    }

    fn pure_group(
        &self,
        start: usize,
        at: usize,
        targets: &BTreeSet<usize>,
        protected: &BTreeSet<usize>,
    ) -> bool {
        !(start..=at).any(|ix| protected.contains(&ix))
            && !(start + 1..=at).any(|ix| targets.contains(&ix) || self.ops[ix].begin.is_some())
    }
    /// Pure call inputs retain source order. Only the call uses their
    /// values; no operator, charge or external callback moves across them.
    pub(super) fn call_reads(
        &mut self,
        callee: &Callee,
        args: &[Expr],
        reg: u32,
    ) -> Option<(Option<Read>, Box<[Read]>)> {
        let count = args.len() + usize::from(matches!(callee, Callee::Read(_)));
        if count == 0 || count > MAX_ARGS {
            return None;
        }
        let callee = match callee {
            Callee::Read(read) if read.reg == reg => Some(
                self.reads(std::slice::from_ref(read.as_ref()))?
                    .into_vec()
                    .pop()?,
            ),
            Callee::Read(_) => return None,
            Callee::Declared(_) => None,
        };
        Some((callee, self.reads(args)?))
    }
}

fn eligible(site: &OperatorSite, exe: &Executable) -> bool {
    !site.checked
        && site.counted.is_none()
        && site.act.is_none()
        && site.root.is_none()
        && site.entry(exe).immediate()
}

fn fixed(op: Op, exe: &Executable) -> Op {
    let Op::Operator {
        site,
        inputs,
        covered,
        to,
    } = op
    else {
        return op;
    };
    let count = match &inputs {
        OperatorInputs::Registers(args) => args.len(),
        OperatorInputs::Source(reads) => reads.len(),
        OperatorInputs::Scalar(_) => {
            return Op::Operator {
                site,
                inputs,
                covered,
                to,
            };
        }
    };
    let destination = match &to {
        Destination::Register => true,
        Destination::Write(write) => !write.accumulates && !exe.lib(site.lib).kernels(exe).append,
    };
    if !eligible(&site, exe)
        || !destination
        || !(count == 1 || (count == 2 && site.pair(exe).is_some()))
    {
        return Op::Operator {
            site,
            inputs,
            covered,
            to,
        };
    }
    let args = match inputs {
        OperatorInputs::Registers(args) => args
            .into_vec()
            .into_iter()
            .map(Operand::Register)
            .collect::<Vec<_>>(),
        OperatorInputs::Source(reads) => reads.into_vec().into_iter().map(Operand::from).collect(),
        OperatorInputs::Scalar(_) => unreachable!("already selected scalar inputs returned above"),
    };
    let mut args = args.into_iter();
    let left = args
        .next()
        .unwrap_or_else(|| unreachable!("a fixed scalar has an operand"));
    let scalar = match args.next() {
        Some(right) => ScalarInputs::Binary([left, right]),
        None => ScalarInputs::Unary(left),
    };
    Op::Operator {
        site,
        inputs: OperatorInputs::Scalar(scalar),
        covered,
        to,
    }
}

impl Op {
    pub(in crate::machine::islands) fn addresses(&mut self, visit: &mut impl FnMut(&mut usize)) {
        match self {
            Self::Try { finally, after, .. } => {
                if let Some(at) = finally {
                    visit(at);
                }
                visit(after);
            }
            Self::Loop { header, .. } | Self::For { header, .. } => visit(header),
            Self::Header {
                otherwise, after, ..
            } => {
                visit(otherwise);
                visit(after);
            }
            Self::LoopBranch { otherwise, .. }
            | Self::Cond { otherwise, .. }
            | Self::Branch { otherwise, .. } => visit(otherwise),
            Self::Jump(to) => visit(to),
            Self::Write { site } => {
                if let Some(access) = &mut site.access {
                    visit(&mut access.head);
                    if let Some(init) = &mut access.init {
                        visit(init);
                    }
                }
            }
            Self::Access(site) => {
                visit(&mut site.after);
                if let Some(short) = &mut site.short {
                    visit(&mut short.after);
                }
                for at in &mut site.source {
                    visit(at);
                }
                if let AccessShape::Length { convert, .. } = &mut site.shape {
                    visit(convert);
                }
                if let AccessShape::Index {
                    test, validation, ..
                } = &mut site.shape
                {
                    visit(test);
                    if let Some(checks) = validation {
                        for at in checks {
                            visit(at);
                        }
                    }
                }
                if let Some(consumer) = &mut site.consumer {
                    visit(&mut consumer.operator);
                }
            }
            _ => {}
        }
    }
}

/// An operand's address, resolved once, including captured cell identity.
/// Registers retain values evaluated before any intervening guest call.
pub(crate) enum Operand {
    Register(u32),
    Literal { reg: u32, value: Value },
    Local { reg: u32, at: usize, miss: u32 },
    Cell { reg: u32, at: usize, miss: u32 },
}

impl Operand {
    pub(in crate::machine::islands) fn reg(&self) -> u32 {
        match self {
            Self::Register(reg)
            | Self::Literal { reg, .. }
            | Self::Local { reg, .. }
            | Self::Cell { reg, .. } => *reg,
        }
    }
}

impl From<Read> for Operand {
    fn from(read: Read) -> Self {
        match read {
            Read::Literal { reg, value } => Self::Literal { reg, value },
            Read::Local {
                reg,
                at,
                miss,
                shared: false,
            } => Self::Local { reg, at, miss },
            Read::Local {
                reg,
                at,
                miss,
                shared: true,
            } => Self::Cell { reg, at, miss },
        }
    }
}

pub(crate) enum Destination {
    Register,
    Write(Box<WriteSite>),
}

/// Reads evaluated inside a call, before its close and admission. Their
/// values become caller roots only when the call suspends that caller.
pub(crate) struct CallInputs {
    pub(super) member: Option<Box<super::super::MemberWrite>>,
    pub(in crate::machine::islands) callee: Option<Operand>,
    pub(in crate::machine::islands) args: Box<[Operand]>,
}

impl CallInputs {
    pub(in crate::machine::islands) fn member(&self) -> Option<&super::super::MemberWrite> {
        self.member.as_deref()
    }

    pub(in crate::machine::islands) fn reads(&self) -> impl Iterator<Item = &Operand> {
        self.callee.iter().chain(self.args.iter())
    }
}
