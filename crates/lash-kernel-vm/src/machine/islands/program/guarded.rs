//! Compile a guarded member path from its source-owned instructions.

use super::*;

impl Build<'_> {
    pub(super) fn kind(&mut self, expr: &Expr, lib: LibId, args: &[Expr]) -> Option<bool> {
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

    /// Makes the kind test at `head`, an `if`'s first instruction, a
    /// compact read where its true arms have a certified member shape.
    pub(super) fn guard(&mut self, head: usize) {
        if !self.fused {
            return;
        }
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
                Op::DirectNative { site, .. } if site.direct(self.exe).is_some() => {
                    fixed += total(site.close);
                }
                Op::Operator {
                    site,
                    inputs: OperatorInputs::Registers(args),
                    ..
                } if pure(site) && args.len() <= 3 => {
                    fixed += site.fixed;
                }
                Op::Operator {
                    site,
                    inputs: OperatorInputs::Source(_),
                    ..
                } if pure(site) => fixed += site.fixed,
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
        if let Some(shape) = self
            .field_path(head, &path, &site, &reads)
            .or_else(|| self.index_path(&path, &site, &reads))
            .or_else(|| self.length_path(&path, &site, &reads))
        {
            let Op::Write { site: written } = &self.ops[write].op else {
                unreachable!()
            };
            let at = written.at;
            let leaves = written.leaves;
            let Some(target) = reads.into_vec().pop() else {
                unreachable!()
            };
            self.ops[head].op = Op::Access(Box::new(AccessSite {
                kind: site,
                target,
                shape,
                hole: None,
                consumer: None,
                short: None,
                at,
                after: write + 1,
                leaves,
                fixed,
                statements,
                arms: arms.into(),
                source: path.into(),
            }));
            return;
        }
        self.ops[head].op = Op::DirectKind { site, reads };
    }

    fn length_path(&self, path: &[usize], kind: &KindTest, reads: &[Read]) -> Option<AccessShape> {
        let [branch, length, convert, write] = path else {
            return None;
        };
        let Op::DirectNative {
            site: native,
            reads: target,
        } = &self.ops[*length].op
        else {
            return None;
        };
        let Some(Direct::Length(lash_kernel_doc::ValueKind::List, charge)) =
            native.direct(self.exe)
        else {
            return None;
        };
        let Op::Operator {
            site: conversion,
            inputs: OperatorInputs::Registers(args),
            ..
        } = &self.ops[*convert].op
        else {
            return None;
        };
        let (
            [Read::Local { at, shared, .. }],
            [
                Read::Local {
                    at: other,
                    shared: sharing,
                    ..
                },
            ],
        ) = (reads, &**target)
        else {
            return None;
        };
        if at != other
            || shared != sharing
            || args.as_ref() != [native.reg]
            || !matches!(&self.ops[*branch].op, Op::Branch { reg, .. } if *reg == kind.same.reg)
            || !matches!(&self.ops[*write].op, Op::Write { site } if site.reg == conversion.reg && site.reads.is_none())
        {
            return None;
        }
        Some(AccessShape::Length {
            native: charge,
            convert: *convert,
        })
    }

    /// The same source facts for a length-guarded resident item read.
    fn index_path(&self, path: &[usize], kind: &KindTest, reads: &[Read]) -> Option<AccessShape> {
        let (path, validation) = match path {
            [first, left, right, compare, branch, rest @ ..] if path.len() == 13 => {
                let Op::Operator {
                    site: a,
                    inputs: OperatorInputs::Source(x),
                    ..
                } = &self.ops[*left].op
                else {
                    return None;
                };
                let Op::Operator {
                    site: b,
                    inputs: OperatorInputs::Source(y),
                    ..
                } = &self.ops[*right].op
                else {
                    return None;
                };
                let Op::Operator {
                    site: predicate,
                    inputs: OperatorInputs::Registers(args),
                    ..
                } = &self.ops[*compare].op
                else {
                    return None;
                };
                if x.len() != 1
                    || y.len() != 1
                    || args.as_ref() != [a.reg, b.reg]
                    || !matches!(&self.ops[*branch].op, Op::Branch { reg, .. } if *reg == predicate.reg)
                {
                    return None;
                }
                (
                    std::iter::once(*first)
                        .chain(rest.iter().copied())
                        .collect::<Vec<_>>(),
                    Some([*left, *right, *compare]),
                )
            }
            _ => (path.to_vec(), None),
        };
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
        ] = path.as_slice()
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
        if !matches!(
            len.direct(self.exe),
            Some(Direct::Length(lash_kernel_doc::ValueKind::List, _))
        ) {
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
        if let Some([left, right, _]) = validation {
            let same_operand = |read: &Read| match (read, &index) {
                (
                    Read::Local {
                        at: a, shared: x, ..
                    },
                    Read::Local {
                        at: b, shared: y, ..
                    },
                ) => a == b && x == y,
                _ => false,
            };
            for at in [left, right] {
                let Op::Operator {
                    inputs: OperatorInputs::Source(reads),
                    ..
                } = &self.ops[at].op
                else {
                    unreachable!()
                };
                if !same_operand(&reads[0]) {
                    return None;
                }
            }
        }
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
            inputs: OperatorInputs::Registers(args),
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
        let Some(Direct::Length(lash_kernel_doc::ValueKind::List, native)) = len.direct(self.exe)
        else {
            return None;
        };
        let literal = matches!(index, Read::Literal { .. });
        Some(AccessShape::Index {
            index,
            test: *test,
            native,
            validation,
            memo: literal.then(std::sync::OnceLock::new),
        })
    }

    /// Project the common field-read shape once. The source instructions
    /// remain the authority for every operand, formula and miss.
    fn field_path(
        &self,
        head: usize,
        path: &[usize],
        kind: &KindTest,
        reads: &[Read],
    ) -> Option<AccessShape> {
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
        if at != other
            || shared != sharing
            || !matches!(
                contains.direct(self.exe),
                Some(Direct::Member(lash_kernel_doc::plan::Table::Record))
            )
        {
            return None;
        }
        let Op::Operator {
            site: test,
            inputs: OperatorInputs::Registers(args),
            ..
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
        let Value::Text(key) = key else { return None };
        let (no, no_units) = super::super::unary(test, Value::Bool(false), self.exe)?;
        let (yes, yes_units) = super::super::unary(test, Value::Bool(true), self.exe)?;
        let (Value::Bool(no), Value::Bool(yes)) = (no, yes) else {
            return None;
        };
        let formula = &self.exe.lib(contains.lib).charge;
        let charge = |present| {
            formula.length_charge(
                0,
                &[None, Some(Value::Text(*key))],
                Some(Value::Bool(present)),
            )
        };
        Some(AccessShape::Field {
            statement: self.ops[head].begin.as_ref()?.id,
            key: lash_kernel_state::arena::Key::of_text(key.as_str()),
            accepts: [no, yes],
            charges: [charge(false)?, charge(true)?],
            negated: [no_units?, yes_units?],
            name: *field,
        })
    }
}
