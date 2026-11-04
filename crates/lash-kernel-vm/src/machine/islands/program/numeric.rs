//! Lower scalar sites and relocate the remaining instruction inventory.
//! There is one final inventory: fused consumers are removed, and every
//! physical edge is projected through the same relocation table.

use super::*;
use crate::machine::islands::MAX_ARGS;
use std::collections::BTreeSet;

impl Build<'_> {
    pub(super) fn reads(&mut self, args: &[Expr]) -> Option<Box<[Read]>> {
        if !self.fused {
            return None;
        }
        let reads = args
            .iter()
            .map(|arg| match arg.kind {
                Kind::Const(value) => Some(Read::Literal {
                    reg: arg.reg,
                    value,
                    source: arg.read?,
                }),
                Kind::Local { local, miss, .. } => Some(Read::Local {
                    reg: arg.reg,
                    at: local.at as usize,
                    shared: self.shared[local.at as usize],
                    miss,
                    source: arg.read?,
                }),
                _ => None,
            })
            .collect::<Option<Box<[_]>>>()?;
        for read in &reads {
            self.registers = self.registers.max(read.reg() as usize + 1);
        }
        Some(reads)
    }

    pub(in crate::machine::islands) fn numeric(&mut self) {
        let mut targets = BTreeSet::new();
        for instruction in &mut self.ops {
            instruction.op.addresses(&mut |at| {
                targets.insert(*at);
            });
        }
        for block in &self.blocks {
            targets.extend(block.starts.iter().copied());
            targets.extend([block.end, block.after]);
        }
        let mut relocation = vec![0; self.ops.len() + 1];
        let source = std::mem::take(&mut self.ops);
        self.ops = Vec::with_capacity(source.len());
        let mut old = source.into_iter().enumerate().peekable();
        while let Some((at, mut instruction)) = old.next() {
            relocation[at] = self.ops.len();
            instruction.op = fixed(instruction.op, self.exe);
            if let Op::Store { finish, .. } = &mut instruction.op
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
                _ => None,
            };
            if let Some(reg) = candidate
                && let Some((next, following)) = old.peek()
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
                    Op::Operator { to, .. } => *to = Destination::Write(write),
                    _ => unreachable!(),
                }
            }
            self.ops.push(instruction);
        }
        let end = relocation.len() - 1;
        relocation[end] = self.ops.len();
        crate::machine::islands::code::relocate(&mut self.ops, &mut self.blocks, &relocation);
    }

    /// Source operands already carry evaluation order; runtime roles name
    /// target, index and value without rescanning physical instructions.
    pub(super) fn store_reads(
        &mut self,
        operands: &[Expr],
        registers: [u32; 3],
    ) -> Option<Box<StoreInputs>> {
        let reads = <Box<[Read; 3]>>::try_from(self.reads(operands)?).ok()?;
        let roles = registers.map(|reg| reads.iter().position(|read| read.reg() == reg));
        let [Some(target), Some(index), Some(value)] = roles else {
            return None;
        };
        if target == index || target == value || index == value {
            return None;
        }
        Some(Box::new(StoreInputs {
            reads: *reads,
            roles: [target, index, value],
        }))
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
        Destination::Write(write) => write.text.is_none() && !exe.lib(site.lib).kernels(exe).append,
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
            Self::LoopBranch { otherwise, .. } | Self::Branch { otherwise, .. } => visit(otherwise),
            Self::Jump(to) => visit(to),
            _ => {}
        }
    }
}

/// An operand's address, resolved once, including captured cell identity.
/// Registers retain values evaluated before any intervening guest call.
pub(crate) enum Operand {
    Register(u32),
    Literal {
        reg: u32,
        value: Value,
        source: crate::compile::expr::SourceRead,
    },
    Local {
        reg: u32,
        at: usize,
        miss: u32,
        source: crate::compile::expr::SourceRead,
    },
    Cell {
        reg: u32,
        at: usize,
        miss: u32,
        source: crate::compile::expr::SourceRead,
    },
}

impl Operand {
    pub(in crate::machine::islands) fn source(&self) -> Option<crate::compile::expr::SourceRead> {
        match self {
            Self::Register(_) => None,
            Self::Literal { source, .. }
            | Self::Local { source, .. }
            | Self::Cell { source, .. } => Some(*source),
        }
    }
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
            Read::Literal { reg, value, source } => Self::Literal { reg, value, source },
            Read::Local {
                reg,
                at,
                miss,
                shared: false,
                source,
            } => Self::Local {
                reg,
                at,
                miss,
                source,
            },
            Read::Local {
                reg,
                at,
                miss,
                shared: true,
                source,
            } => Self::Cell {
                reg,
                at,
                miss,
                source,
            },
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
    pub(in crate::machine::islands) callee: Option<Operand>,
    pub(in crate::machine::islands) args: Box<[Operand]>,
}

impl CallInputs {
    pub(in crate::machine::islands) fn reads(&self) -> impl Iterator<Item = &Operand> {
        self.callee.iter().chain(self.args.iter())
    }
}
