//! A member read, scalar consumer and local write have named source exits.

use super::*;
use std::collections::BTreeSet;

pub(crate) struct Update {
    pub(in crate::machine::islands) read: HeapSite,
    pub(in crate::machine::islands) args: Box<[u32]>,
    pub(in crate::machine::islands) reads: Option<Box<[Read; 2]>>,
    pub(in crate::machine::islands) charge: super::super::MemberAccess,
    pub(in crate::machine::islands) scalar: Box<OperatorSite>,
    pub(in crate::machine::islands) left: Operand,
    pub(in crate::machine::islands) right: Operand,
    pub(in crate::machine::islands) covered: bool,
    pub(in crate::machine::islands) write: Box<WriteSite>,
    pub(in crate::machine::islands) consumed: Box<[u32]>,
}

impl Build<'_> {
    pub(in crate::machine::islands) fn updates(&mut self) {
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
        let source = std::mem::take(&mut self.ops);
        let mut relocation = vec![0; source.len() + 1];
        let mut source = source.into_iter().enumerate().peekable();
        while let Some((at, mut instruction)) = source.next() {
            relocation[at] = self.ops.len();
            let matches = match (&instruction.op, source.peek()) {
                (
                    Op::Heap {
                        site,
                        resident:
                            Some(HeapKernel::Read {
                                units,
                                to: Destination::Register,
                            }),
                        ..
                    },
                    Some((
                        next,
                        Instruction {
                            op:
                                Op::Operator {
                                    inputs:
                                        OperatorInputs::Scalar(ScalarInputs::Binary([left, right])),
                                    to: Destination::Write(write),
                                    ..
                                },
                            begin: None,
                            ..
                        },
                    )),
                ) => {
                    !targets.contains(next)
                        && !matches!(units, super::super::MemberAccess::Append { .. })
                        && (matches!(left, Operand::Register(reg) if *reg == site.reg)
                            || matches!(right, Operand::Register(reg) if *reg == site.reg))
                        && !write.bind
                        && !write.shared
                        && !write.accumulates
                        && write.close.is_none()
                }
                _ => false,
            };
            if matches {
                let Some((next, consumer)) = source.next() else {
                    unreachable!();
                };
                relocation[next] = self.ops.len();
                let Op::Heap {
                    site: read,
                    args,
                    resident:
                        Some(HeapKernel::Read {
                            units: charge,
                            to: Destination::Register,
                        }),
                    inputs,
                    ..
                } = instruction.op
                else {
                    unreachable!();
                };
                let reads = match inputs {
                    HeapInputs::Registers => None,
                    HeapInputs::Read(reads) => Some(reads),
                    HeapInputs::Write(_) => unreachable!(),
                };
                let Op::Operator {
                    site: scalar,
                    inputs: OperatorInputs::Scalar(ScalarInputs::Binary([left, right])),
                    covered,
                    to: Destination::Write(write),
                } = consumer.op
                else {
                    unreachable!();
                };
                instruction.op = Op::Update(Box::new(Update {
                    read,
                    args,
                    reads,
                    charge,
                    scalar,
                    left,
                    right,
                    covered,
                    write,
                    consumed: consumer.live,
                }));
            }
            self.ops.push(instruction);
        }
        let end = relocation.len() - 1;
        relocation[end] = self.ops.len();
        crate::machine::islands::code::relocate(&mut self.ops, &mut self.blocks, &relocation);
    }
}
