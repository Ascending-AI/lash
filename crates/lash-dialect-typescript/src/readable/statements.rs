//! Statements, and the statement runs the front end lowers one read to.

use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;
use lash_kernel_value::sites::Site;

use super::lift::{Lift, Lifter, refuse};
use super::source::{self, Arm, Expr, StmtKind};

impl Lifter<'_> {
    pub(super) fn statement(&mut self, statement: &k::Stmt, site: &Site) -> Lift<StmtKind> {
        Ok(match statement {
            k::Stmt::Let {
                name: kernel,
                value,
            } => {
                let name = self.binding(kernel)?;
                if let k::Rhs::Expr(value) = value
                    && let Some(function) = self.closure(value)?
                {
                    return Ok(StmtKind::Const(name, Expr::arrow(function)));
                }
                let value = self.rhs(value)?;
                // A local's declared type is spelled again where its source
                // evidence names one an annotation writes; readback proves
                // it lowers alike.
                if let Some(ty) = self.evidence.get(kernel).cloned()
                    && let Ok(ty) = self.ts_type(None, &ty, &k::FieldRoot::Result, &mut Vec::new())
                {
                    let constant = !self.reassigned(&name);
                    return Ok(StmtKind::Declared {
                        constant,
                        name,
                        ty,
                        value: match value {
                            Expr::Undefined if !constant => None,
                            value => Some(value),
                        },
                    });
                }
                if self.reassigned(&name) {
                    match value {
                        Expr::Undefined => StmtKind::Let(name, None),
                        value => StmtKind::Let(name, Some(value)),
                    }
                } else {
                    StmtKind::Const(name, value)
                }
            }
            k::Stmt::Assign {
                place,
                value,
                order: k::AssignOrder::ValueFirst,
            } => self.assign(place, value)?,
            // A discarded dynamic member write is the exact ts.set import
            // call; retain the source's place-before-value evaluation order.
            k::Stmt::Do {
                value: k::Expr::Act(action),
            } if matches!(&action.kind, k::ActKind::Call(k::Callee::Local(function))
                    if self.is(function, "ts.set"))
                && action.args.len() == 3 =>
            {
                let target = self.expr(&action.args[0])?;
                let index = self.expr(&action.args[1])?;
                let value = self.expr(&action.args[2])?;
                StmtKind::Assign(Expr::Index(Box::new(target), Box::new(index)), "=", value)
            }
            k::Stmt::Do { value } => StmtKind::Expr(self.expr(value)?),
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                if self.style.exact
                    && let Some([target, index, value]) =
                        self.list_write(condition, then_block, else_block)
                {
                    return Ok(StmtKind::Expr(super::expressions::constructor(
                        "list.set",
                        vec![self.expr(&target)?, self.expr(&index)?, self.expr(&value)?],
                    )));
                }
                let condition = self.condition(condition)?;
                let then = self.block(then_block, &site.child(1))?;
                let arm = site.child(2);
                let caption = self.arm_caption(&arm);
                let otherwise = if else_block.is_empty() && caption.is_none() {
                    None
                } else {
                    Some(Arm {
                        caption,
                        body: self.block(else_block, &arm)?,
                    })
                };
                StmtKind::If {
                    condition,
                    then,
                    otherwise,
                }
            }
            k::Stmt::For {
                metered: 0,
                binding,
                iterable,
                via: None,
                body,
                else_block,
            } if else_block.is_empty() => {
                let Some(enumeration) = crate::constructs::iteration::FOR_IN.recognize(
                    iterable,
                    binding,
                    body,
                    &|id, name| self.is(id, name),
                ) else {
                    return refuse("a property enumeration the dialect does not lower to");
                };
                StmtKind::For {
                    operator: "in",
                    binding: self.binding(enumeration.binding)?,
                    iterable: self.operand(enumeration.subject, "value.operand")?,
                    body: self.block_at(enumeration.body, &site.child(1).child(0).child(1), 1)?,
                }
            }
            k::Stmt::For {
                metered: 0,
                binding,
                iterable,
                via: Some(via),
                body,
                else_block,
            } if else_block.is_empty() && self.for_of(via) => {
                let kind = StmtKind::For {
                    operator: "of",
                    binding: self.binding(binding)?,
                    iterable: self.expr(iterable)?,
                    body: self.block(body, &site.child(1))?,
                };
                self.labeled(kind)
            }
            k::Stmt::While {
                metered: 0,
                condition,
                body,
                else_block,
            } if else_block.is_empty() => {
                let kind = StmtKind::While {
                    condition: self.condition(condition)?,
                    body: self.block(body, &site.child(1))?,
                };
                self.labeled(kind)
            }
            k::Stmt::Break => StmtKind::Break(None),
            k::Stmt::Continue => StmtKind::Continue(None),
            k::Stmt::ReturnVia { value, .. }
                if crate::constructs::conventions::RETURNS
                    .is_statement(statement, |target, name| self.is(target, name)) =>
            {
                match value {
                    k::Expr::Literal(k::Literal::Absent) => StmtKind::Return(None),
                    value => StmtKind::Return(Some(self.expr(value)?)),
                }
            }
            // `await control.finish(value)` ends the cell with its outcome.
            k::Stmt::Finish {
                value:
                    k::Expr::Let {
                        name,
                        value: finished,
                        body,
                    },
            } if super::lift::temporary(name)
                && matches!(&**body, k::Expr::Literal(k::Literal::Null))
                && matches!(&**finished, k::Expr::Act(act)
                    if matches!(act.kind, k::ActKind::Perform { .. })) =>
            {
                StmtKind::Expr(self.expr(finished)?)
            }
            // A function value's own return; an async function's is adopted.
            k::Stmt::Return { value } => match value {
                k::Expr::Literal(k::Literal::Absent) => StmtKind::Return(None),
                value => StmtKind::Return(Some(self.expr(value)?)),
            },
            k::Stmt::Throw { value } => match self.exit(value) {
                Some(exit) => exit,
                None => StmtKind::Throw(self.expr(value)?),
            },
            k::Stmt::Try(scope) if scope.catch.is_some() || scope.finally.is_some() => {
                let body = self.block(&scope.body, &site.child(0))?;
                let mut child = 1;
                let catch = match &scope.catch {
                    Some(catch) => {
                        let arm = site.child(child);
                        child += 1;
                        let binding = self.binding(&catch.binding)?;
                        Some((
                            Arm {
                                caption: self.arm_caption(&arm),
                                body: self.block(&catch.body, &arm)?,
                            },
                            Some(binding),
                        ))
                    }
                    None => None,
                };
                let finally = match &scope.finally {
                    Some(finally) => {
                        let arm = site.child(child);
                        Some(Arm {
                            caption: self.arm_caption(&arm),
                            body: self.block(finally, &arm)?,
                        })
                    }
                    None => None,
                };
                StmtKind::Try {
                    body,
                    catch,
                    finally,
                }
            }
            _ => return refuse("a statement the authored dialect does not lower to"),
        })
    }

    fn arm_caption(&self, arm: &Site) -> Option<k::Caption> {
        self.site_caption(arm)
    }

    /// Whether `via` is the protocol a `for…of` loop lowers to.
    fn for_of(&self, via: &k::Protocol) -> bool {
        crate::constructs::conventions::FOR_OF.matches(via, |id, name| self.is(id, name))
    }

    pub(super) fn binding(&self, name: &Name) -> Lift<String> {
        if let Some(original) = self.bindings.get(name) {
            Ok(original.clone())
        } else if source::identifier(name.as_str()) {
            Ok(name.as_str().to_owned())
        } else {
            refuse("a name TypeScript cannot spell")
        }
    }

    pub(super) fn rhs(&mut self, value: &k::Rhs) -> Lift<Expr> {
        match value {
            k::Rhs::Expr(value) => self.expr(value),
            k::Rhs::Action(action) => self.act(&act_of(action)),
        }
    }

    fn assign(&mut self, place: &k::Place, value: &k::Rhs) -> Lift<StmtKind> {
        match place {
            k::Place::Variable(name) => {
                let name = self.binding(name)?;
                Ok(StmtKind::Assign(Expr::Ident(name), "=", self.rhs(value)?))
            }
            // `list.push(value)` appends at the list's length.
            k::Place::Member(member) => match member {
                k::Member::Index {
                    target: k::Expr::Variable(list),
                    index: k::Expr::Call { function, args },
                } if self.is(function, "list.len")
                    && matches!(args.as_slice(), [k::Expr::Variable(of)] if of == list) =>
                {
                    let list = Expr::Ident(self.binding(list)?);
                    let value = self.rhs(value)?;
                    // A binding of no known kind (a capture cell) spells the
                    // list's own push.
                    if self.style.exact {
                        return Ok(StmtKind::Expr(super::expressions::constructor(
                            "list.method",
                            vec![list, Expr::Text("push".to_owned()), value],
                        )));
                    }
                    Ok(StmtKind::Expr(Expr::Call(
                        Box::new(Expr::Field(Box::new(list), "push".to_owned())),
                        vec![value],
                    )))
                }
                k::Member::Field { target, field } => {
                    let target = self.expr(target)?;
                    let value = self.rhs(value)?;
                    Ok(StmtKind::Assign(
                        Expr::Field(Box::new(target), field.clone()),
                        "=",
                        value,
                    ))
                }
                k::Member::Index { target, index } => {
                    let target = self.expr(target)?;
                    let index = self.expr(index)?;
                    let value = self.rhs(value)?;
                    Ok(StmtKind::Assign(
                        Expr::Index(Box::new(target), Box::new(index)),
                        "=",
                        value,
                    ))
                }
            },
        }
    }

    /// The temporary a run of statements starting `block` computes, its
    /// value as the source read it, and how many statements the run is:
    /// the field, index and `length` reads the front end checks by kind.
    pub(super) fn read_ladder(&mut self, block: &[k::Stmt]) -> Lift<Option<(Name, Expr, usize)>> {
        let [
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
            },
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        let computed = &crate::constructs::reads::COMPUTED;
        if let Some(access) = computed.recognize(
            name,
            &block[1..2],
            &|id, helper| self.is(id, helper),
            |value, helper| self.helper_read(value, helper),
        ) {
            let target = self.expr(access.target)?;
            // A local's annotation is erased from the document. Retain the
            // record test even when its initializer would infer `object`.
            let target = if access.tests_record {
                Expr::As(Box::new(target), source::Type::Word("any"))
            } else {
                target
            };
            let read = Expr::Index(Box::new(target), Box::new(self.expr(access.index)?));
            return Ok(Some((name.clone(), read, 2)));
        }
        if let Some(read) = self.field_ladder(name, condition, then_block, else_block)? {
            return Ok(Some((name.clone(), read, 2)));
        }
        if let Some(read) = self.index_ladder(name, condition, then_block, else_block)? {
            return Ok(Some((name.clone(), read, 2)));
        }
        if let Some(read) = self.length_ladder(name, condition, then_block, else_block)? {
            return Ok(Some((name.clone(), read, 2)));
        }
        Ok(None)
    }

    /// `same(kind(x), kind)`.
    pub(super) fn kind_test<'e>(&self, condition: &'e k::Expr, kind: &str) -> Option<&'e k::Expr> {
        crate::constructs::reads::kind(condition, kind, &|id, name| self.is(id, name))
    }

    /// `set name = value` alone in a block.
    fn sets<'e>(block: &'e [k::Stmt], name: &Name) -> Option<&'e k::Rhs> {
        match block {
            [
                k::Stmt::Assign {
                    place: k::Place::Variable(set),
                    value,
                    order: k::AssignOrder::ValueFirst,
                },
            ] if set == name => Some(value),
            _ => None,
        }
    }

    /// `invoke helper(target, key)` with `helper` the named library function.
    /// The element write `xs[i] = v` of a list the source built, as the
    /// front end guards it: `i`'s position test unless it is a literal, the
    /// bound that admits an append, the element written in place, and the
    /// `ts.set` helper wherever a test fails. A binding of no known kind (a
    /// capture cell) adds a kind test to that write, so it spells the
    /// list's own write, `list.set(xs, i, v)`.
    fn list_write(
        &self,
        condition: &k::Expr,
        then_block: &[k::Stmt],
        else_block: &[k::Stmt],
    ) -> Option<[k::Expr; 3]> {
        use crate::constructs::reads::{APPEND_BOUNDS, POSITION};
        let is = |id: &lash_kernel_value::FunctionId, name: &str| self.is(id, name);
        let mut guards = vec![(condition, else_block)];
        let mut block = then_block;
        if let [
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = block
        {
            guards.push((condition, else_block.as_slice()));
            block = then_block;
        }
        let [
            k::Stmt::Assign {
                place: k::Place::Member(member),
                value: k::Rhs::Expr(value),
                order: k::AssignOrder::ValueFirst,
            },
        ] = block
        else {
            return None;
        };
        let k::Member::Index { target, index } = member else {
            return None;
        };
        if !matches!(target, k::Expr::Variable(_)) {
            return None;
        }
        let (bound, position) = match guards.as_slice() {
            [bound] => (bound, None),
            [position, bound] => (bound, Some(position)),
            _ => return None,
        };
        if APPEND_BOUNDS.recognize(bound.0, &is)? != [index, target] {
            return None;
        }
        match position {
            Some(position) if POSITION.recognize(position.0, &is)? != [index] => return None,
            Some(_) => {}
            None if !matches!(index, k::Expr::Literal(_)) => return None,
            None => {}
        }
        let helper = [target.clone(), index.clone(), value.clone()];
        for (_, otherwise) in &guards {
            let [k::Stmt::Do { value: written }] = otherwise else {
                return None;
            };
            if self.helper_read(&k::Rhs::Expr(written.clone()), "ts.set")? != helper {
                return None;
            }
        }
        Some(helper)
    }

    pub(super) fn helper_read(&self, value: &k::Rhs, helper: &str) -> Option<Vec<k::Expr>> {
        match value {
            k::Rhs::Action(k::Action::Call {
                callee: k::Callee::Local(id),
                args,
            }) if self.is(id, helper) => Some(args.iter().map(expr_of).collect()),
            k::Rhs::Expr(k::Expr::Act(action)) if matches!(&action.kind, k::ActKind::Call(k::Callee::Local(id)) if self.is(id, helper)) => {
                Some(action.args.clone())
            }
            k::Rhs::Expr(k::Expr::Call { function, args }) if self.is(function, helper) => {
                Some(args.clone())
            }
            _ => None,
        }
    }

    fn unbranded_branch<'a>(
        &self,
        condition: &'a k::Expr,
        then_block: &'a [k::Stmt],
        else_block: &'a [k::Stmt],
    ) -> Option<(&'a k::Expr, &'a [k::Stmt], &'a [k::Stmt])> {
        for (test, direct, guarded) in [
            (crate::constructs::reads::UNBRANDED, then_block, else_block),
            (crate::constructs::reads::BRANDED, else_block, then_block),
        ] {
            if let Some(values) = test.recognize(condition, &|id, name| self.is(id, name))
                && let [target] = values.as_slice()
            {
                return Some((*target, direct, guarded));
            }
        }
        None
    }

    /// `x.field`, its record read guarded by the brand check.
    fn field_ladder(
        &mut self,
        name: &Name,
        condition: &k::Expr,
        then_block: &[k::Stmt],
        else_block: &[k::Stmt],
    ) -> Lift<Option<Expr>> {
        // A record the source built takes no kind test: its unbranded test
        // alone guards the direct read, and its brand the helper's.
        let (target, direct, guarded, otherwise) = match self.kind_test(condition, "record") {
            Some(target) => {
                let [
                    k::Stmt::If {
                        condition: test,
                        then_block: direct,
                        else_block: guarded,
                    },
                ] = then_block
                else {
                    return Ok(None);
                };
                let Some((held, direct, guarded)) = self.unbranded_branch(test, direct, guarded)
                else {
                    return Ok(None);
                };
                if held != target {
                    return Ok(None);
                }
                (target, direct, guarded, Some(else_block))
            }
            None => {
                let Some((target, direct, guarded)) =
                    self.unbranded_branch(condition, then_block, else_block)
                else {
                    return Ok(None);
                };
                (target, direct, guarded, None)
            }
        };
        // A name some built-in has is the record's own only where the record
        // holds a field by it; otherwise its built-in helper reads it.
        let (direct, owned) = match direct {
            [
                k::Stmt::If {
                    condition: k::Expr::Call { function, args },
                    then_block,
                    else_block,
                },
            ] if self.is(function, "record.contains") => match args.as_slice() {
                [held, k::Expr::Literal(k::Literal::Text(field))] if held == target => {
                    (then_block.as_slice(), Some((field, else_block.as_slice())))
                }
                _ => return Ok(None),
            },
            direct => (direct, None),
        };
        let Some(k::Rhs::Expr(k::Expr::Member(member))) = Self::sets(direct, name) else {
            return Ok(None);
        };
        let k::Member::Field {
            target: read,
            field,
        } = &**member
        else {
            return Ok(None);
        };
        let read_helper = [
            target.clone(),
            k::Expr::Literal(k::Literal::Text(field.clone())),
        ];
        let builtin = |helper: &str| format!("{helper}.{field}");
        let helper_args = |value: Option<&k::Rhs>| {
            value.is_some_and(|value| {
                if owned.is_none() {
                    return self
                        .helper_read(value, "ts.read")
                        .is_some_and(|args| args == read_helper);
                }
                ["ts.property", "ts.member"].iter().any(|helper| {
                    self.helper_read(value, &builtin(helper))
                        .is_some_and(|args| args.as_slice() == std::slice::from_ref(target))
                })
            })
        };
        if read != target
            || owned.is_some_and(|(held, otherwise)| {
                held != field || !helper_args(Self::sets(otherwise, name))
            })
            || !helper_args(Self::sets(guarded, name))
            || otherwise.is_some_and(|otherwise| !helper_args(Self::sets(otherwise, name)))
        {
            return Ok(None);
        }
        let exact = otherwise.is_none() && self.style.exact && self.reads_binding(target);
        let target = match (exact, otherwise) {
            (true, _) => self.expr(target)?,
            (false, None) => self.operand(target, "record.operand")?,
            (false, Some(_)) => self.operand(target, "value.operand")?,
        };
        Ok(Some(if exact {
            super::expressions::constructor("record.field", vec![target, Expr::Text(field.clone())])
        } else {
            Expr::Field(Box::new(target), field.clone())
        }))
    }

    /// `x[i]`: a list's element, a hole read as undefined, or any other
    /// value's computed read.
    fn index_ladder(
        &mut self,
        name: &Name,
        condition: &k::Expr,
        then_block: &[k::Stmt],
        else_block: &[k::Stmt],
    ) -> Lift<Option<Expr>> {
        // A known list skips the kind test.
        let (bounds, in_bounds, out_of_bounds, otherwise) = match self.kind_test(condition, "list")
        {
            Some(_) => {
                let [
                    k::Stmt::If {
                        condition: bounds,
                        then_block: in_bounds,
                        else_block: out_of_bounds,
                    },
                ] = then_block
                else {
                    return Ok(None);
                };
                (
                    bounds,
                    in_bounds.as_slice(),
                    out_of_bounds.as_slice(),
                    Some(else_block),
                )
            }
            None => (condition, then_block, else_block, None),
        };
        // A variable numeric index first proves it is whole and nonnegative.
        let (bounds, in_bounds, out_of_bounds, position) = if let Some(index) =
            crate::lower::position_guard(bounds, |id, name| self.is(id, name))
        {
            let [
                k::Stmt::If {
                    condition,
                    then_block,
                    else_block,
                },
            ] = in_bounds
            else {
                return Ok(None);
            };
            (
                condition,
                then_block.as_slice(),
                else_block.as_slice(),
                Some((index, out_of_bounds)),
            )
        } else {
            (bounds, in_bounds, out_of_bounds, None)
        };
        let Some(values) =
            crate::constructs::reads::BOUNDS.recognize(bounds, &|id, name| self.is(id, name))
        else {
            return Ok(None);
        };
        let [index, target] = values.as_slice() else {
            return Ok(None);
        };
        let (index, target) = (*index, *target);
        if let Some(listed) = self.kind_test(condition, "list")
            && listed != target
        {
            return Ok(None);
        }
        let [
            k::Stmt::Assign {
                place: k::Place::Variable(set),
                value: k::Rhs::Expr(k::Expr::Member(member)),
                order: k::AssignOrder::ValueFirst,
            },
            k::Stmt::If {
                condition: hole,
                then_block: undefined,
                else_block: no_hole,
            },
        ] = in_bounds
        else {
            return Ok(None);
        };
        let k::Member::Index {
            target: read,
            index: at,
        } = &**member
        else {
            return Ok(None);
        };
        let is_hole = crate::constructs::reads::HOLE
            .recognize(hole, &|id, name| self.is(id, name))
            .is_some_and(|values| values.as_slice() == [&k::Expr::Variable(name.clone())]);
        let reset = matches!(
            Self::sets(undefined, name),
            Some(k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)))
        );
        if set != name || read != target || at != index || !is_hole || !reset || !no_hole.is_empty()
        {
            return Ok(None);
        }
        let helper = [target.clone(), index.clone()];
        let computed = |value: Option<&k::Rhs>| {
            value
                .and_then(|value| self.helper_read(value, "ts.get_numeric"))
                .is_some_and(|args| args == helper)
        };
        if position
            .is_some_and(|(held, fallback)| held != index || !computed(Self::sets(fallback, name)))
            || !computed(Self::sets(out_of_bounds, name))
            || otherwise.is_some_and(|otherwise| !computed(Self::sets(otherwise, name)))
        {
            return Ok(None);
        }
        let read = target;
        let target = self.expr(target)?;
        let index = self.expr(index)?;
        // An optional chain's link keeps its `?.`: the element read of its
        // base's operand.
        if otherwise.is_none() && self.style.exact && !matches!(target, Expr::Optional(_)) {
            return Ok(Some(super::expressions::constructor(
                "list.element",
                vec![target, index],
            )));
        }
        let spelling = if otherwise.is_none() {
            "list.operand"
        } else {
            "value.operand"
        };
        let target = self.spell_operand(read, target, spelling);
        Ok(Some(Expr::Index(Box::new(target), Box::new(index))))
    }

    /// `x.length`: a list's length, or any other value's property.
    fn length_ladder(
        &mut self,
        name: &Name,
        condition: &k::Expr,
        then_block: &[k::Stmt],
        else_block: &[k::Stmt],
    ) -> Lift<Option<Expr>> {
        let Some(target) = self.kind_test(condition, "list") else {
            return Ok(None);
        };
        let Some(k::Rhs::Expr(value)) = Self::sets(then_block, name) else {
            return Ok(None);
        };
        if !crate::constructs::reads::LENGTH
            .recognize(value, &|id, name| self.is(id, name))
            .is_some_and(|values| values.as_slice() == [target])
        {
            return Ok(None);
        }
        let Some(args) = Self::sets(else_block, name)
            .and_then(|value| self.helper_read(value, "ts.property.length"))
        else {
            return Ok(None);
        };
        if args != std::slice::from_ref(target) {
            return Ok(None);
        }
        let target = self.expr(target)?;
        Ok(Some(Expr::Field(Box::new(target), "length".to_owned())))
    }
}

/// The expression form of a statement action.
pub(super) fn act_of(action: &k::Action) -> k::ActExpr {
    let k::Expr::Act(act) = k::Expr::from(action.clone()) else {
        unreachable!("action expression");
    };
    *act
}

fn expr_of(atom: &k::Atom) -> k::Expr {
    match atom {
        k::Atom::Variable(name) => k::Expr::Variable(name.clone()),
        k::Atom::Literal(literal) => k::Expr::Literal(literal.clone()),
    }
}
