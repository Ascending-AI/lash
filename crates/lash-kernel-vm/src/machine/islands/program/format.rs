//! A formatted parts list, join and append share one execution family.
//! The list and joined text remain ordinary source allocations.

use super::*;
use std::collections::BTreeSet;

pub(crate) struct Format {
    pub(in crate::machine::islands) parts: crate::machine::islands::code::sites::Make,
    pub(in crate::machine::islands) separator: (u32, Value),
    pub(in crate::machine::islands) join: NativeSite,
    pub(in crate::machine::islands) join_args: [u32; 2],
    pub(in crate::machine::islands) joined: Box<[u32]>,
    pub(in crate::machine::islands) append: Box<OperatorSite>,
    pub(in crate::machine::islands) append_args: [u32; 2],
    pub(in crate::machine::islands) appending: Box<[u32]>,
    pub(in crate::machine::islands) covered: bool,
    pub(in crate::machine::islands) write: Box<WriteSite>,
    pub(in crate::machine::islands) writing: Box<[u32]>,
}

impl Build<'_> {
    pub(in crate::machine::islands) fn formats(&mut self) {
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
        let starts: BTreeSet<_> = self
            .ops
            .windows(5)
            .enumerate()
            .filter_map(|(at, group)| {
                if (1..5).any(|n| targets.contains(&(at + n)) || group[n].begin.is_some()) {
                    return None;
                }
                match (
                    &group[0].op,
                    &group[1].op,
                    &group[2].op,
                    &group[3].op,
                    &group[4].op,
                ) {
                    (
                        Op::Make {
                            make: Make::List,
                            reg,
                            ..
                        },
                        Op::Constant { dst, .. },
                        Op::Native {
                            site: join,
                            inputs: NativeInputs::Registers(joined),
                        },
                        Op::Operator {
                            site: append,
                            inputs: OperatorInputs::Registers(args),
                            ..
                        },
                        Op::Write { site: write },
                    ) if matches!(join.direct(self.exe), Some(Direct::TextJoin))
                        && joined.as_ref() == [*reg, *dst]
                        && args.len() == 2
                        && args[1] == join.reg
                        && append.concat(self.exe).is_some()
                        && append.counted.is_none()
                        && append.act.is_none()
                        && append.root.is_none()
                        && write.reg == append.reg
                        && !write.shared
                        && !write.bind
                        && write.close.is_none() =>
                    {
                        Some(at)
                    }
                    _ => None,
                }
            })
            .collect();
        let source = std::mem::take(&mut self.ops);
        let mut relocation = vec![0; source.len() + 1];
        let mut source = source.into_iter().enumerate();
        while let Some((at, mut instruction)) = source.next() {
            relocation[at] = self.ops.len();
            if starts.contains(&at) {
                let Some((_, separator)) = source.next() else {
                    unreachable!("a matched format graph has five positions");
                };
                let Some((_, joined)) = source.next() else {
                    unreachable!("a matched format graph has five positions");
                };
                let Some((_, appending)) = source.next() else {
                    unreachable!("a matched format graph has five positions");
                };
                let Some((_, writing)) = source.next() else {
                    unreachable!("a matched format graph has five positions");
                };
                for mapped in &mut relocation[at + 1..at + 5] {
                    *mapped = self.ops.len();
                }
                let Op::Make {
                    ahead,
                    replay_guard,
                    make,
                    items,
                    reg,
                    members,
                    miss,
                } = instruction.op
                else {
                    unreachable!();
                };
                let Op::Constant { dst, value } = separator.op else {
                    unreachable!();
                };
                let Op::Native {
                    site: join,
                    inputs: NativeInputs::Registers(joined_args),
                } = joined.op
                else {
                    unreachable!();
                };
                let Op::Operator {
                    site: append,
                    inputs: OperatorInputs::Registers(append_args),
                    covered,
                    to: Destination::Register,
                } = appending.op
                else {
                    unreachable!();
                };
                let Op::Write { site: write } = writing.op else {
                    unreachable!();
                };
                instruction.op = Op::Format(Box::new(Format {
                    parts: crate::machine::islands::code::sites::Make {
                        ahead,
                        replay_guard,
                        make,
                        items,
                        reg,
                        members,
                        miss,
                    },
                    separator: (dst, value),
                    join,
                    join_args: [joined_args[0], joined_args[1]],
                    joined: joined.live,
                    append,
                    append_args: [append_args[0], append_args[1]],
                    appending: appending.live,
                    covered,
                    write,
                    writing: writing.live,
                }));
            }
            self.ops.push(instruction);
        }
        let end = relocation.len() - 1;
        relocation[end] = self.ops.len();
        crate::machine::islands::code::relocate(&mut self.ops, &mut self.blocks, &relocation);
    }
}
