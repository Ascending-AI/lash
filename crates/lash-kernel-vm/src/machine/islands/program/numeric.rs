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
            instruction.op.addresses(|at| {
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
        let mut removed = self.member_inputs(&targets);
        removed.extend(self.call_inputs(&targets));
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
                instruction.op = fixed(instruction.op);
                if let Op::MemberStore { finish, .. } = &mut instruction.op
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
                    Op::Unary {
                        site,
                        to: Destination::Register,
                        ..
                    }
                    | Op::Binary {
                        site,
                        to: Destination::Register,
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
                        Op::Unary { to, .. } | Op::Binary { to, .. } => {
                            *to = Destination::Write(write)
                        }
                        _ => unreachable!(),
                    }
                }
            }
            self.ops.push(instruction);
        }
        let end = relocation.len() - 1;
        relocation[end] = self.ops.len();
        for instruction in &mut self.ops {
            instruction.op.addresses(|at| *at = relocation[*at]);
        }
        for block in &mut self.blocks {
            for start in &mut block.starts {
                *start = relocation[*start];
            }
            block.end = relocation[block.end];
            block.after = relocation[block.after];
        }
    }
    /// Pure call inputs retain source order. Only the call uses their
    /// values; no operator, charge or external callback moves across them.
    fn call_inputs(&mut self, targets: &BTreeSet<usize>) -> BTreeSet<usize> {
        let mut removed = BTreeSet::new();
        for call in 0..self.ops.len() {
            let Op::Call { site, read, args } = &self.ops[call].op else {
                continue;
            };
            let count = args.len() + usize::from(*read);
            if count == 0 || count > MAX_ARGS || call < count {
                continue;
            }
            let mut start = call - count;
            if (start + 1..=call).any(|at| targets.contains(&at) || self.ops[at].begin.is_some()) {
                continue;
            }
            let wanted = read
                .then_some(site.reg)
                .into_iter()
                .chain(args.iter().copied());
            let reads = self.ops[start..call]
                .iter()
                .zip(wanted)
                .map(|(instruction, reg)| match instruction.op {
                    Op::Constant { dst, value } if dst == reg => Some(Read::Literal { reg, value }),
                    Op::Local {
                        dst,
                        at,
                        shared,
                        miss,
                    } if dst == reg => Some(Read::Local {
                        reg,
                        at,
                        shared,
                        miss,
                    }),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            let Some(mut reads) = reads else {
                continue;
            };
            let callee = if *read { Some(reads.remove(0)) } else { None };
            let held = if start > 0 && !targets.contains(&start) && self.ops[start].begin.is_none()
            {
                match self.ops[start - 1].op {
                    Op::Local {
                        dst,
                        at,
                        shared,
                        miss,
                    } if self.ops[start].live.contains(&dst)
                        && !reads.iter().any(|read| read.reg() == dst)
                        && callee.as_ref().is_none_or(|read| read.reg() != dst) =>
                    {
                        start -= 1;
                        Some(Operand::from(Read::Local {
                            reg: dst,
                            at,
                            shared,
                            miss,
                        }))
                    }
                    _ => None,
                }
            } else {
                None
            };
            let property = if callee.is_none() && held.is_none() {
                self.member_write(site, &reads)
            } else {
                None
            };
            let begin = self.ops[start].begin.take();
            let live = self.ops[start].live.clone();
            let Op::Call { site, .. } = &mut self.ops[call].op else {
                unreachable!()
            };
            site.inputs = Some(Box::new(CallInputs {
                prefix: match (held, property) {
                    (Some(held), None) => CallPrefix::Held(held),
                    (_, Some(member)) => CallPrefix::Member(member),
                    (None, None) => CallPrefix::None,
                },
                callee: callee.map(Operand::from),
                args: reads.into_iter().map(Operand::from).collect(),
            }));
            self.ops[call].begin = begin;
            self.ops[call].live = live;
            removed.extend(start..call);
        }
        removed
    }
}

fn eligible(site: &OperatorSite) -> bool {
    !site.checked
        && site.counted.is_none()
        && site.act.is_none()
        && site.root.is_none()
        && site.entry.immediate()
}

fn fixed(op: Op) -> Op {
    let (site, args, covered, to) = match op {
        Op::DirectOperator {
            site,
            reads,
            covered,
        } if eligible(&site) && (reads.len() == 1 || (reads.len() == 2 && site.pair.is_some())) => {
            (
                site,
                reads
                    .into_vec()
                    .into_iter()
                    .map(Operand::from)
                    .collect::<Vec<_>>(),
                covered,
                Destination::Register,
            )
        }
        Op::Operator {
            site,
            args,
            covered,
        } if eligible(&site) && (args.len() == 1 || (args.len() == 2 && site.pair.is_some())) => (
            site,
            args.into_vec().into_iter().map(Operand::Register).collect(),
            covered,
            Destination::Register,
        ),
        Op::Accumulation {
            site,
            append: None,
            reads,
            covered,
            write,
        } if eligible(&site) && reads.len() == 2 && site.pair.is_some() && !write.accumulates => (
            site,
            reads.into_vec().into_iter().map(Operand::from).collect(),
            covered,
            Destination::Write(write),
        ),
        other => return other,
    };
    let mut args = args.into_iter();
    let Some(left) = args.next() else {
        unreachable!("a fixed scalar has an operand");
    };
    match args.next() {
        Some(right) => {
            let Some(kernel) = site.pair else {
                unreachable!("a binary site was certified");
            };
            Op::Binary {
                kernel,
                site,
                left,
                right,
                covered,
                to,
            }
        }
        None => Op::Unary {
            site,
            arg: left,
            covered,
            to,
        },
    }
}

impl Op {
    fn addresses(&mut self, mut visit: impl FnMut(&mut usize)) {
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
            Self::AccessInit { head, .. } => visit(head),
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
pub(in crate::machine::islands) enum Operand {
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

pub(in crate::machine::islands) enum Destination {
    Register,
    Write(Box<WriteSite>),
}

/// Reads evaluated inside a call, before its close and admission. Their
/// values become caller roots only when the call suspends that caller.
pub(in crate::machine::islands) struct CallInputs {
    prefix: CallPrefix,
    pub(in crate::machine::islands) callee: Option<Operand>,
    pub(in crate::machine::islands) args: Box<[Operand]>,
}

enum CallPrefix {
    None,
    Held(Operand),
    Member(Box<super::super::MemberWrite>),
}

impl CallInputs {
    pub(in crate::machine::islands) fn held(&self) -> Option<&Operand> {
        match &self.prefix {
            CallPrefix::Held(held) => Some(held),
            _ => None,
        }
    }

    pub(in crate::machine::islands) fn member(&self) -> Option<&super::super::MemberWrite> {
        match &self.prefix {
            CallPrefix::Member(member) => Some(member),
            _ => None,
        }
    }

    pub(in crate::machine::islands) fn reads(&self) -> impl Iterator<Item = &Operand> {
        self.held()
            .into_iter()
            .chain(self.callee.iter())
            .chain(self.args.iter())
    }
}
