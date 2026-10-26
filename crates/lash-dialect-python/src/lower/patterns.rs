//! Match subjects and captures are evaluated once. Tentative captures live
//! in private slots until the whole pattern succeeds, before its guard.

use lash_kernel_doc::{Expr, Literal, Place, Rhs};
use lash_kernel_value::Name;
use ruff_python_ast::{self as ast, Pattern};
use ruff_text_size::Ranged;
use std::collections::{BTreeMap, BTreeSet};

use super::{Buf, Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::{self, Ty};

type Captures = BTreeMap<String, Name>;

impl Lowerer<'_> {
    pub(super) fn match_statement(&mut self, node: &ast::StmtMatch) -> Lowering<()> {
        let subject = self.expr(&node.subject)?;
        let subject = self.pin(subject);
        let done = self.let_rhs(Rhs::Expr(Expr::Literal(Literal::Bool(false))), Ty::Bool);
        let Expr::Variable(done) = done.expr else {
            unreachable!("temporary")
        };
        for case in &node.cases {
            validate(&case.pattern)?;
            let mut names = BTreeSet::new();
            scope::pattern_names(&case.pattern, &mut names);
            let mut captures = Captures::new();
            for name in names {
                let slot = self.let_rhs(Rhs::Expr(Expr::Literal(Literal::Absent)), Ty::Unknown);
                let Expr::Variable(slot) = slot.expr else {
                    unreachable!("temporary")
                };
                captures.insert(name, slot);
            }
            let attempt = self.block(|this| {
                let ok = this.let_rhs(Rhs::Expr(Expr::Literal(Literal::Bool(true))), Ty::Bool);
                let Expr::Variable(ok) = ok.expr else {
                    unreachable!("temporary")
                };
                this.pattern(&case.pattern, subject.clone(), &ok, &captures)?;
                let matched = this.block(|this| {
                    for (name, slot) in &captures {
                        let variable = this.written(name, case.pattern.range())?;
                        this.store(
                            Place::Variable(variable),
                            Operand::temp(slot.clone(), Ty::Unknown),
                        );
                        this.mark_bound(name);
                    }
                    let body = this.block(|this| {
                        this.store(Place::Variable(done.clone()), boolean(true));
                        this.statements(&case.body)
                    })?;
                    if let Some(guard) = &case.guard {
                        let guard = this.expr(guard)?;
                        let guard = this.truth(guard)?;
                        this.emit_if(guard, body, Buf::default());
                    } else {
                        this.append(body);
                    }
                    Ok(())
                })?;
                this.emit_if(Expr::Variable(ok), matched, Buf::default());
                Ok(())
            })?;
            let pending = self.not(Expr::Variable(done.clone()))?;
            self.emit_if(pending, attempt, Buf::default());
        }
        Ok(())
    }

    fn pattern(
        &mut self,
        pattern: &Pattern,
        value: Operand,
        ok: &Name,
        captures: &Captures,
    ) -> Lowering<()> {
        let body = self.block(|this| this.pattern_inner(pattern, value, ok, captures))?;
        self.emit_if(Expr::Variable(ok.clone()), body, Buf::default());
        Ok(())
    }

    fn pattern_inner(
        &mut self,
        pattern: &Pattern,
        value: Operand,
        ok: &Name,
        captures: &Captures,
    ) -> Lowering<()> {
        let place = Place::Variable(ok.clone());
        match pattern {
            Pattern::MatchValue(p) => {
                let expected = self.expr(&p.value)?;
                let test = self.native("eq", vec![value.expr, expected.expr])?;
                self.store(place, Operand::inline(test, Ty::Bool));
            }
            Pattern::MatchSingleton(p) => {
                let literal = match p.value {
                    ast::Singleton::None => Literal::Null,
                    ast::Singleton::True => Literal::Bool(true),
                    ast::Singleton::False => Literal::Bool(false),
                };
                let test = self.native("same", vec![value.expr, Expr::Literal(literal)])?;
                self.store(place, Operand::inline(test, Ty::Bool));
            }
            Pattern::MatchAs(p) => {
                if let Some(pattern) = &p.pattern {
                    self.pattern(pattern, value.clone(), ok, captures)?;
                }
                if let Some(name) = &p.name {
                    self.store(Place::Variable(captures[name.id.as_str()].clone()), value);
                }
            }
            Pattern::MatchStar(p) => {
                if let Some(name) = &p.name {
                    self.store(Place::Variable(captures[name.id.as_str()].clone()), value);
                }
            }
            Pattern::MatchSequence(p) => {
                let star = p
                    .patterns
                    .iter()
                    .position(|p| matches!(p, Pattern::MatchStar(_)));
                let test = self.invoke(
                    "py.match_sequence",
                    &[
                        value.clone(),
                        integer(p.patterns.len()),
                        boolean(star.is_some()),
                    ],
                    Ty::Bool,
                )?;
                self.store(place, test);
                for (index, pattern) in p.patterns.iter().enumerate() {
                    let part = self.block(|this| {
                        let item = if Some(index) == star {
                            let tail = p.patterns.len() - index - 1;
                            let end = if tail == 0 {
                                Operand::none()
                            } else {
                                let length = this.invoke(
                                    "py.length",
                                    std::slice::from_ref(&value),
                                    Ty::Int,
                                )?;
                                let end =
                                    this.native("sub", vec![length.expr, integer(tail).expr])?;
                                Operand::inline(end, Ty::Int)
                            };
                            let sliced = this.invoke(
                                "py.slice",
                                &[value.clone(), integer(index), end, Operand::none()],
                                Ty::Unknown,
                            )?;
                            this.invoke("py.to_list", &[sliced], Ty::List)?
                        } else {
                            let position = if star.is_some_and(|star| index > star) {
                                Operand::literal(
                                    Literal::Int(
                                        (-(p.patterns.len() as i64 - index as i64)).into(),
                                    ),
                                    Ty::Int,
                                )
                            } else {
                                integer(index)
                            };
                            this.invoke("py.getitem", &[value.clone(), position], Ty::Unknown)?
                        };
                        let item = this.pin(item);
                        this.pattern(pattern, item, ok, captures)
                    })?;
                    self.emit_if(Expr::Variable(ok.clone()), part, Buf::default());
                }
            }
            Pattern::MatchMapping(p) => {
                let test =
                    self.invoke("py.match_mapping", std::slice::from_ref(&value), Ty::Bool)?;
                self.store(place, test);
                let used = self.let_rhs(Rhs::Expr(Expr::List(Vec::new())), Ty::List);
                for (key, pattern) in p.keys.iter().zip(&p.patterns) {
                    let part = self.block(|this| {
                        let key = this.expr(key)?;
                        let key = this.pin(key);
                        let duplicate =
                            this.invoke("py.contains", &[used.clone(), key.clone()], Ty::Bool)?;
                        let duplicate = this.truth(duplicate)?;
                        let failure = this.block(|this| {
                            this.invoke_do(
                                "py.fail",
                                &[
                                    Operand::text("ValueError"),
                                    Operand::text("mapping pattern checks duplicate key"),
                                ],
                            )
                        })?;
                        this.emit_if(duplicate, failure, Buf::default());
                        let end = this.native("list.len", vec![used.expr.clone()])?;
                        this.store(
                            Place::Member(lash_kernel_doc::Member::Index {
                                target: used.expr.clone(),
                                index: end,
                            }),
                            key.clone(),
                        );
                        let present =
                            this.invoke("py.contains", &[value.clone(), key.clone()], Ty::Bool)?;
                        this.store(Place::Variable(ok.clone()), present);
                        let nested = this.block(|this| {
                            let item =
                                this.invoke("py.getitem", &[value.clone(), key], Ty::Unknown)?;
                            let item = this.pin(item);
                            this.pattern(pattern, item, ok, captures)
                        })?;
                        this.emit_if(Expr::Variable(ok.clone()), nested, Buf::default());
                        Ok(())
                    })?;
                    self.emit_if(Expr::Variable(ok.clone()), part, Buf::default());
                }
                if let Some(name) = &p.rest {
                    let rest = self.block(|this| {
                        let rest = this.invoke("py.match_rest", &[value, used], Ty::Dict)?;
                        this.store(Place::Variable(captures[name.id.as_str()].clone()), rest);
                        Ok(())
                    })?;
                    self.emit_if(Expr::Variable(ok.clone()), rest, Buf::default());
                }
            }
            Pattern::MatchOr(p) => {
                self.store(place.clone(), boolean(false));
                for alternative in &p.patterns {
                    let body = self.block(|this| {
                        this.store(place.clone(), boolean(true));
                        this.pattern(alternative, value.clone(), ok, captures)
                    })?;
                    let pending = self.not(Expr::Variable(ok.clone()))?;
                    self.emit_if(pending, body, Buf::default());
                }
            }
            Pattern::MatchClass(p) => {
                return Err(diagnostics::with_repair(
                    Code::MatchUnsupported,
                    "class patterns require a guest class model",
                    p.range,
                    "match existing values with literal, sequence or mapping patterns",
                ));
            }
        }
        Ok(())
    }
}

fn integer(value: usize) -> Operand {
    Operand::literal(
        Literal::Int(i64::try_from(value).unwrap_or(i64::MAX).into()),
        Ty::Int,
    )
}
fn boolean(value: bool) -> Operand {
    Operand::literal(Literal::Bool(value), Ty::Bool)
}

/// Captures occur once per pattern; alternatives must bind the same names.
fn validate(pattern: &Pattern) -> Lowering<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let mut insert = |other: BTreeSet<String>| -> Lowering<()> {
        for name in other {
            if !names.insert(name.clone()) {
                return Err(diagnostics::diagnostic(
                    Code::Syntax,
                    format!("multiple assignments to `{name}` in pattern"),
                    pattern.range(),
                ));
            }
        }
        Ok(())
    };
    match pattern {
        Pattern::MatchOr(p) => {
            let mut alternatives = p.patterns.iter();
            let expected = alternatives
                .next()
                .map(validate)
                .transpose()?
                .unwrap_or_default();
            for alternative in alternatives {
                if validate(alternative)? != expected {
                    return Err(diagnostics::diagnostic(
                        Code::Syntax,
                        "alternative patterns bind different names",
                        p.range,
                    ));
                }
            }
            insert(expected)?;
        }
        Pattern::MatchSequence(p) => {
            if p.patterns
                .iter()
                .filter(|p| matches!(p, Pattern::MatchStar(_)))
                .count()
                > 1
            {
                return Err(diagnostics::diagnostic(
                    Code::Syntax,
                    "multiple starred names in sequence pattern",
                    p.range,
                ));
            }
            for p in &p.patterns {
                insert(validate(p)?)?;
            }
        }
        Pattern::MatchMapping(p) => {
            for p in &p.patterns {
                insert(validate(p)?)?;
            }
            if let Some(name) = &p.rest {
                insert(BTreeSet::from([name.id.to_string()]))?;
            }
        }
        Pattern::MatchAs(p) => {
            if let Some(p) = &p.pattern {
                insert(validate(p)?)?;
            }
            if let Some(name) = &p.name {
                insert(BTreeSet::from([name.id.to_string()]))?;
            }
        }
        _ => {
            let mut captured = BTreeSet::new();
            scope::pattern_names(pattern, &mut captured);
            insert(captured)?;
        }
    }
    Ok(names)
}
