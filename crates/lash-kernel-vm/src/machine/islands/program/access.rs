//! Completion edges retain only the guard blocks still open after a read.

use super::*;

/// A compact read, whose source instructions remain its miss path.
pub(in crate::machine::islands) struct AccessSite {
    pub(in crate::machine::islands) kind: Box<KindTest>,
    pub(in crate::machine::islands) target: Read,
    pub(in crate::machine::islands) shape: AccessShape,
    pub(in crate::machine::islands) hole: Option<AccessHole>,
    pub(in crate::machine::islands) consumer: Option<AccessConsumer>,
    pub(in crate::machine::islands) short: Option<AccessEnd>,
    pub(in crate::machine::islands) at: usize,
    pub(in crate::machine::islands) after: usize,
    pub(in crate::machine::islands) leaves: u32,
    pub(in crate::machine::islands) fixed: u64,
    pub(in crate::machine::islands) statements: u64,
    pub(in crate::machine::islands) arms: Box<[usize]>,
    pub(in crate::machine::islands) source: Box<[usize]>,
}

/// The access alone remains a valid commit when its optional consumer declines.
pub(in crate::machine::islands) struct AccessEnd {
    pub(in crate::machine::islands) after: usize,
    pub(in crate::machine::islands) leaves: u32,
    pub(in crate::machine::islands) fixed: u64,
    pub(in crate::machine::islands) statements: u64,
    pub(in crate::machine::islands) arms: Box<[usize]>,
}

pub(in crate::machine::islands) struct AccessHole {
    pub(in crate::machine::islands) lib: LibId,
    pub(in crate::machine::islands) floats: Option<u64>,
}

pub(in crate::machine::islands) struct AccessConsumer {
    pub(in crate::machine::islands) operator: usize,
    pub(in crate::machine::islands) reads: [Read; 2],
    pub(in crate::machine::islands) at: usize,
    pub(in crate::machine::islands) next: usize,
    pub(in crate::machine::islands) accumulates: bool,
    pub(in crate::machine::islands) floats: Option<(lash_kernel_doc::scalar::ScalarOp, u64)>,
}

pub(in crate::machine::islands) enum AccessShape {
    Length {
        native: u64,
        convert: usize,
    },
    Field {
        statement: StmtId,
        key: lash_kernel_state::arena::Key,
        accepts: [bool; 2],
        charges: [crate::compile::LengthCharge; 2],
        negated: [u64; 2],
        name: usize,
    },
    Index {
        index: Read,
        test: usize,
        native: u64,
        validation: Option<[usize; 3]>,
        /// Only a literal index qualifies. The actual prepared comparison
        /// certifies both its successful answer and its exact charge.
        memo: Option<std::sync::OnceLock<(u64, u64)>>,
    },
}

impl Build<'_> {
    pub(super) fn member_write(
        &self,
        site: &CallSite,
        reads: &[Read],
    ) -> Option<Box<super::super::MemberWrite>> {
        self.property(
            site.library?,
            site.root,
            site.occurrence,
            site.answers?,
            Some(reads),
        )
    }

    fn property(
        &self,
        lib: LibId,
        root: Option<StmtId>,
        counted: Option<usize>,
        depth: u32,
        reads: Option<&[Read]>,
    ) -> Option<Box<super::super::MemberWrite>> {
        let key = reads
            .and_then(|reads| reads.get(1))
            .and_then(|read| match read {
                Read::Literal {
                    value: Value::Text(key),
                    ..
                } => Some(*key),
                _ => None,
            });
        let function = self.exe.lib(lib);
        let signature = &function.header.signature;
        if signature.params.len() != 3
            || signature.result != lash_kernel_value::Type::Any
            || signature
                .params
                .iter()
                .any(|param| param.ty != lash_kernel_value::Type::Any)
        {
            return None;
        }
        let (entry, _) = function.prepared.as_ref()?;
        let element = ((entry.element() == Some(lash_kernel_doc::plan::Element::Write))
            && key.is_none())
        .then(|| {
            function.charge.known_charge(
                &[
                    None,
                    Some(Value::Float(lash_kernel_doc::Float::new(0.0))),
                    None,
                ],
                None,
            )
        })
        .flatten();
        let (guard, accepts, values) = entry.record_write()?;
        let charge = match key {
            Some(key) => super::super::MemberCharge::Known(
                function
                    .charge
                    .known_charge(&[None, Some(Value::Text(key)), None], None)?,
            ),
            None => super::super::MemberCharge::Text(function.charge.short_text_charge(1)?),
        };
        let (before, root) = match root {
            Some(id) => {
                let crate::compile::Form::Action(action) = &self.exe.stmt(id).form else {
                    return None;
                };
                let crate::compile::ActionKind::Call { args, .. } = &action.kind else {
                    return None;
                };
                (args.len() as u64, action.guest.then_some(action.occurrence))
            }
            None => (0, None),
        };
        Some(Box::new(super::super::MemberWrite {
            element,
            guard: lash_kernel_state::arena::Key::of_text(guard),
            accepts,
            values,
            lookup: key.map(|key| lash_kernel_state::arena::Key::of_text(key.as_str())),
            charge,
            before,
            occurrence: counted.or(root),
            depth,
        }))
    }

    pub(super) fn member_inputs(
        &mut self,
        targets: &std::collections::BTreeSet<usize>,
    ) -> std::collections::BTreeSet<usize> {
        let mut removed = std::collections::BTreeSet::new();
        for at in 0..self.ops.len() {
            let Op::Heap { site, args } = &self.ops[at].op else {
                continue;
            };
            if args.len() == 2 {
                let function = self.exe.lib(site.lib);
                if function.prepared.as_ref().is_some_and(|(entry, _)| {
                    entry.element() == Some(lash_kernel_doc::plan::Element::Read)
                }) && let Some(units) = function.charge.known_charge(
                    &[None, Some(Value::Float(lash_kernel_doc::Float::new(0.0)))],
                    None,
                ) {
                    let original = std::mem::replace(&mut self.ops[at].op, Op::Jump(at + 1));
                    let Op::Heap { site, args } = original else {
                        unreachable!()
                    };
                    self.ops[at].op = Op::ElementRead { site, args, units };
                }
                continue;
            }
            if args.len() != 3 {
                continue;
            }
            let start = at.saturating_sub(3);
            let fold = at >= 3
                && !(start + 1..=at)
                    .any(|at| targets.contains(&at) || self.ops[at].begin.is_some());
            let reads = if fold {
                self.ops[start..at]
                    .iter()
                    .zip(args)
                    .map(|(instruction, reg)| match instruction.op {
                        Op::Constant { dst, value } if dst == *reg => {
                            Some(Read::Literal { reg: dst, value })
                        }
                        Op::Local {
                            dst,
                            at,
                            shared,
                            miss,
                        } if dst == *reg => Some(Read::Local {
                            reg: dst,
                            at,
                            shared,
                            miss,
                        }),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()
                    .and_then(|reads| reads.try_into().ok())
                    .map(Box::new)
            } else {
                None
            };
            let depth = match site.depth {
                Depth::None => 0,
                Depth::Operation(n) | Depth::Template(n) => n,
            };
            let Some(property) = self.property(
                site.lib,
                site.root,
                site.counted,
                depth,
                reads.as_deref().map(|reads: &[Read; 3]| reads.as_slice()),
            ) else {
                continue;
            };
            let fold = reads.is_some();
            let original = std::mem::replace(&mut self.ops[at].op, Op::Jump(at + 1));
            let Op::Heap { site, args } = original else {
                unreachable!()
            };
            self.ops[at].op = Op::MemberStore {
                site,
                args,
                reads,
                property,
                finish: None,
            };
            if fold {
                self.ops[at].begin = self.ops[start].begin.take();
                self.ops[at].live = self.ops[start].live.clone();
                removed.extend(start..at);
            }
        }
        removed
    }

    fn edge(&self, mut at: usize) -> usize {
        while let Some(Instruction {
            op: Op::Jump(to),
            begin: None,
            ..
        }) = self.ops.get(at)
        {
            at = *to;
        }
        at
    }

    pub(in crate::machine::islands) fn access_edges(&mut self) {
        for head in 0..self.ops.len() {
            if !matches!(self.ops[head].op, Op::Access(_)) {
                continue;
            }
            let Op::Access(mut site) =
                std::mem::replace(&mut self.ops[head].op, Op::Jump(head + 1))
            else {
                unreachable!()
            };
            let closed = (site.leaves as usize).min(site.arms.len());
            let keep = site.arms.len() - closed;
            if closed != 0
                && site.arms[keep..]
                    .iter()
                    .all(|block| self.blocks[*block].declares.is_empty())
            {
                site.after = self.blocks[site.arms[keep]].after;
                site.leaves -= closed as u32;
                site.arms = site.arms[..keep].into();
            }
            site.after = self.edge(site.after);
            let short = AccessEnd {
                after: site.after,
                leaves: site.leaves,
                fixed: site.fixed,
                statements: site.statements,
                arms: site.arms.clone(),
            };
            self.access_hole(&mut site);
            self.access_consumer(&mut site);
            if site.hole.is_some() || site.consumer.is_some() {
                site.short = Some(short);
            }
            self.ops[head].op = Op::Access(site);
            if head > 0
                && let Op::Access(site) = &self.ops[head].op
                && let Op::Write { site: init } = &self.ops[head - 1].op
                && init.at == site.at
                && !matches!(site.target, Read::Local { at, .. } if at == init.at)
                && !matches!(&site.shape, AccessShape::Index { index: Read::Local { at, .. }, .. } if *at == init.at)
                && self.ops[head].begin.is_some()
                && init.bind
                && !init.shared
                && !init.accumulates
                && init.leaves == 0
                && init.begin.is_some()
                && matches!(
                    init.reads.as_deref(),
                    Some([Read::Literal {
                        value: Value::Absent,
                        ..
                    }])
                )
            {
                let Op::Write { site: original } =
                    std::mem::replace(&mut self.ops[head - 1].op, Op::Jump(head))
                else {
                    unreachable!()
                };
                self.ops[head - 1].op = Op::AccessInit {
                    head,
                    original,
                    held: None,
                };
                if head >= 2
                    && let Op::Write { site: held } = &self.ops[head - 2].op
                    && let Op::Access(site) = &self.ops[head].op
                    && held.bind
                    && !held.shared
                    && !held.accumulates
                    && held.leaves == 0
                    && held.begin.is_some()
                    && self.ops[head - 2].begin.is_none()
                    && held.reads.as_deref().is_some_and(|reads| reads.len() == 1)
                    && held.at != site.at
                    && !matches!(site.target, Read::Local { at, .. } if at == held.at)
                    && !matches!(&site.shape, AccessShape::Index { index: Read::Local { at, .. }, .. } if *at == held.at)
                    && site.consumer.as_ref().is_some_and(|consumer| consumer.reads.iter().any(|read| matches!(read, Read::Local { at, shared: false, .. } if *at == held.at)))
                {
                    let held = held.clone();
                    let Op::AccessInit { original, .. } = &self.ops[head - 1].op else {
                        unreachable!()
                    };
                    self.ops[head - 2].op = Op::AccessInit {
                        head,
                        original: original.clone(),
                        held: Some(held),
                    };
                }
            }
        }
    }

    fn access_hole(&self, site: &mut AccessSite) {
        if site.leaves != 0 || site.arms.is_empty() {
            return;
        }
        let start = site.after;
        let Some(Instruction {
            op:
                Op::Local {
                    dst,
                    at,
                    shared: false,
                    ..
                },
            begin: Some(_),
            ..
        }) = self.ops.get(start)
        else {
            return;
        };
        if *at != site.at {
            return;
        }
        let Some(Instruction {
            op: Op::Unit { site: unit, src },
            begin: None,
            ..
        }) = self.ops.get(start + 1)
        else {
            return;
        };
        if src != dst {
            return;
        }
        let mut fixed = 1 + unit.close.map_or(0, |close| u64::from(close.total));
        let mut branch = start + 2;
        while let Some(Instruction {
            op: Op::Close { close, .. },
            begin: None,
            ..
        }) = self.ops.get(branch)
        {
            let Some(total) = fixed.checked_add(u64::from(close.total)) else {
                return;
            };
            fixed = total;
            branch += 1;
        }
        let Some(Instruction {
            op:
                Op::Branch {
                    reg,
                    otherwise,
                    opens: [_, None],
                    ..
                },
            begin: None,
            ..
        }) = self.ops.get(branch)
        else {
            return;
        };
        if *reg != unit.reg {
            return;
        }
        let Some(Instruction {
            op: Op::Done {
                leaves,
                empty: Some(_),
            },
            begin: None,
            ..
        }) = self.ops.get(*otherwise)
        else {
            return;
        };
        if *leaves as usize != site.arms.len()
            || site
                .arms
                .iter()
                .any(|block| !self.blocks[*block].declares.is_empty())
        {
            return;
        }
        let Some(total) = site.fixed.checked_add(fixed) else {
            return;
        };
        site.fixed = total;
        site.hole = Some(AccessHole {
            lib: unit.lib,
            // An empty construction and a float both have size 1 and
            // nested size 0. ScalarTable refuses magnitude formulas.
            floats: self
                .exe
                .lib(unit.lib)
                .prepared
                .as_ref()
                .and_then(|(_, fast)| fast.scalar())
                .and_then(|table| table.units(0)),
        });
        site.after = self.edge(self.blocks[site.arms[0]].after);
        site.arms = Box::default();
        site.statements += 1;
        let mut source = site.source.to_vec();
        source.extend(start..=branch);
        source.push(*otherwise);
        site.source = source.into();
    }

    fn access_consumer(&self, site: &mut AccessSite) {
        if site.leaves != 0 || !site.arms.is_empty() {
            return;
        }
        let at = site.after;
        let Some(instruction) = self.ops.get(at) else {
            return;
        };
        let (operator, reads, write, after) = match &instruction.op {
            Op::Accumulation {
                site: operator,
                reads,
                write,
                ..
            } => (&**operator, &**reads, &**write, at + 1),
            Op::DirectOperator {
                site: operator,
                reads,
                ..
            } => {
                let Some(Instruction {
                    op: Op::Write { site: write },
                    begin: None,
                    ..
                }) = self.ops.get(at + 1)
                else {
                    return;
                };
                (&**operator, &**reads, &**write, at + 2)
            }
            _ => return,
        };
        let [left, right] = reads else { return };
        let reads_value =
            |read: &Read| matches!(read, Read::Local { at, shared: false, .. } if *at == site.at);
        if !reads_value(left) && !reads_value(right) {
            return;
        }
        if operator.checked
            || operator.counted.is_some()
            || operator.act.is_some()
            || operator.root.is_some()
            || write.bind
            || write.shared
            || write.reg != operator.reg
            || write.reads.is_some()
            || write.begin.is_some()
        {
            return;
        }
        let Some(begin) = &instruction.begin else {
            return;
        };
        let Some(fixed) = site
            .fixed
            .checked_add(1)
            .and_then(|n| n.checked_add(operator.fixed))
            .and_then(|n| n.checked_add(write.close.map_or(0, |close| u64::from(close.total))))
        else {
            return;
        };
        site.fixed = fixed;
        site.consumer = Some(AccessConsumer {
            operator: at,
            reads: [*left, *right],
            at: write.at,
            next: begin.index + 1,
            accumulates: write.accumulates,
            floats: operator
                .pair
                .and_then(|pair| pair.floats)
                .zip(operator.floats),
        });
        site.after = after;
        site.leaves = write.leaves;
        site.statements += 1;
        let mut source = site.source.to_vec();
        source.extend(at..after);
        site.source = source.into();
    }
}
