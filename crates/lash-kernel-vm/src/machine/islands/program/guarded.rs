//! Compile a guarded member path from its source-owned instructions.

use super::*;

impl Build<'_> {
    /// Makes the kind test at `head`, an `if`'s first instruction, a
    /// guarded read ([`GuardedPath`]) where its true arms have that shape.
    pub(super) fn guard(&mut self, head: usize) {
        if !matches!(
            self.ops.get(head).map(|op| &op.op),
            Some(Op::DirectKind { .. })
        ) {
            return;
        }
        let (mut path, mut arms, mut fixed, mut statements) = (Vec::new(), Vec::new(), 0, 0);
        let total = |close: Option<Close>| close.map_or(0, |close| u64::from(close.total));
        for (p, instruction) in self.ops.iter().enumerate().skip(head + 1) {
            let pure = |site: &OperatorSite| {
                site.counted.is_none() && !site.checked && site.root.is_none()
            };
            if instruction.begin.is_some() {
                fixed += 1;
                statements += 1;
            }
            match &instruction.op {
                Op::Close { close, .. } => {
                    fixed += u64::from(close.total);
                    continue;
                }
                Op::Constant { .. }
                | Op::Local { .. }
                | Op::Field { .. }
                | Op::Index { .. }
                | Op::DirectKind { .. } => {}
                Op::DirectNative { site, .. } if site.direct.is_some() => {
                    fixed += total(site.close);
                }
                Op::Operator { site, args, .. } if pure(site) && args.len() <= 3 => {
                    fixed += site.fixed;
                }
                Op::DirectOperator { site, .. } if pure(site) => fixed += site.fixed,
                Op::Branch {
                    opens: [Some(block), _],
                    ..
                } if arms.len() < GUARDED_ARMS
                    && self.blocks[*block].starts.first() == Some(&(p + 1)) =>
                {
                    arms.push(*block);
                }
                Op::Write { site }
                    if !arms.is_empty() && !site.bind && !site.shared && !site.accumulates =>
                {
                    fixed += total(site.close);
                    path.push(p);
                    break;
                }
                Op::Store { .. }
                    if !arms.is_empty()
                        && matches!(
                            self.ops.get(p + 1).map(|op| &op.op),
                            Some(Op::Done { empty: None, .. })
                        ) =>
                {
                    path.push(p);
                    break;
                }
                _ => return,
            }
            path.push(p);
        }
        let Some(&write) = path
            .last()
            .filter(|p| matches!(self.ops[**p].op, Op::Write { .. } | Op::Store { .. }))
        else {
            return;
        };
        let op = std::mem::replace(&mut self.ops[head].op, Op::Jump(head + 1));
        let Op::DirectKind { site, reads } = op else {
            unreachable!("a guarded read's head is a kind test");
        };
        let field = self.field_path(&path, &site, &reads);
        let index = self.index_path(&path, &site, &reads);
        self.ops[head].op = Op::Guarded(Box::new(GuardedPath {
            field,
            index,
            kind: site,
            reads,
            write,
            path: path.into(),
            fixed,
            statements,
            arms: arms.into(),
        }));
    }

    /// The same source facts for a length-guarded resident item read.
    fn index_path(&self, path: &[usize], kind: &KindTest, reads: &[Read]) -> Option<GuardedIndex> {
        let [
            first,
            before,
            length,
            test,
            second,
            target,
            after,
            item,
            write,
        ] = path
        else {
            return None;
        };
        let [Read::Local { at, shared, .. }] = reads else {
            return None;
        };
        if !matches!(&self.ops[*first].op, Op::Branch { reg, .. } if *reg == kind.same.reg) {
            return None;
        }
        let Op::DirectNative {
            site: len,
            reads: operands,
        } = &self.ops[*length].op
        else {
            return None;
        };
        if !matches!(len.direct, Some(Direct::Length(_))) {
            return None;
        }
        let [
            Read::Local {
                at: other,
                shared: sharing,
                ..
            },
        ] = &**operands
        else {
            return None;
        };
        if at != other || shared != sharing {
            return None;
        }
        let leaf = |p: usize| match self.ops[p].op {
            Op::Constant { dst, value } => Some(Read::Literal { reg: dst, value }),
            Op::Local {
                dst,
                at,
                shared,
                miss,
            } => Some(Read::Local {
                reg: dst,
                at,
                shared,
                miss,
            }),
            _ => None,
        };
        let index = leaf(*before)?;
        let later = leaf(*after)?;
        let same = match (&index, &later) {
            (
                Read::Literal {
                    value: Value::Int(x),
                    ..
                },
                Read::Literal {
                    value: Value::Int(y),
                    ..
                },
            ) => x == y,
            (
                Read::Literal {
                    value: Value::Float(x),
                    ..
                },
                Read::Literal {
                    value: Value::Float(y),
                    ..
                },
            ) => x.get().to_bits() == y.get().to_bits(),
            (
                Read::Local {
                    at: x, shared: a, ..
                },
                Read::Local {
                    at: y, shared: b, ..
                },
            ) => x == y && a == b,
            _ => false,
        };
        if !same {
            return None;
        }
        let Op::Operator {
            site: comparison,
            args,
            ..
        } = &self.ops[*test].op
        else {
            return None;
        };
        if args.as_ref() != [index.reg(), len.reg]
            || !matches!(&self.ops[*second].op, Op::Branch { reg, .. } if *reg == comparison.reg)
        {
            return None;
        }
        let Op::Local {
            dst,
            at: other,
            shared: sharing,
            ..
        } = &self.ops[*target].op
        else {
            return None;
        };
        if at != other || shared != sharing || later.reg() != *dst + 1 {
            return None;
        }
        if !matches!(&self.ops[*item].op, Op::Index { dst: result, .. } if result == dst) {
            return None;
        }
        if !matches!(&self.ops[*write].op, Op::Write { site } if site.reg == *dst && site.reads.is_none())
        {
            return None;
        }
        Some(GuardedIndex {
            length: *length,
            test: *test,
            index,
        })
    }

    /// Project the common field-read shape once. The source instructions
    /// remain the authority for every operand, formula and miss.
    fn field_path(&self, path: &[usize], kind: &KindTest, reads: &[Read]) -> Option<GuardedField> {
        let [first, member, negate, second, local, field, write] = path else {
            return None;
        };
        let [Read::Local { at, shared, .. }] = reads else {
            return None;
        };
        let Op::Branch { reg, .. } = &self.ops[*first].op else {
            return None;
        };
        if *reg != kind.same.reg {
            return None;
        }
        let Op::DirectNative {
            site: contains,
            reads: operands,
        } = &self.ops[*member].op
        else {
            return None;
        };
        let [
            Read::Local {
                at: other,
                shared: sharing,
                ..
            },
            Read::Literal { value: key, .. },
        ] = &**operands
        else {
            return None;
        };
        if at != other || shared != sharing || !matches!(contains.direct, Some(Direct::Member(_))) {
            return None;
        }
        let Op::Operator {
            site: test, args, ..
        } = &self.ops[*negate].op
        else {
            return None;
        };
        if args.as_ref() != [contains.reg] {
            return None;
        }
        let Op::Branch { reg, .. } = &self.ops[*second].op else {
            return None;
        };
        if *reg != test.reg {
            return None;
        }
        let Op::Local {
            dst,
            at: other,
            shared: sharing,
            ..
        } = &self.ops[*local].op
        else {
            return None;
        };
        if at != other || shared != sharing {
            return None;
        }
        let Op::Field {
            dst: result, field, ..
        } = &self.ops[*field].op
        else {
            return None;
        };
        if dst != result {
            return None;
        }
        let Op::Write { site: written } = &self.ops[*write].op else {
            return None;
        };
        if written.reg != *result || written.reads.is_some() {
            return None;
        }
        Some(GuardedField {
            member: *member,
            negate: *negate,
            name: *field,
            key: *key,
        })
    }
}
