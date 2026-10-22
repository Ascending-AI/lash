//! Statements, and the statement runs the front end lowers one read to.

use lash_kernel_doc::{self as k};

use lash_vocabulary::Name;
use lash_vocabulary::sites::Site;

use super::lift::{Lift, Lifter, refuse};
use super::source::{self, Arm, Body, Expr, StmtKind};

impl Lifter<'_> {
    pub(super) fn statement(&mut self, statement: &k::Stmt, site: &Site) -> Lift<StmtKind> {
        Ok(match statement {
            k::Stmt::Let { name, value } => {
                let name = self.binding(name)?;
                if let k::Rhs::Expr(value) = value
                    && let Some(function) = self.closure(value, &site.child(0))?
                {
                    return Ok(match function.body {
                        Body::Block(_) => StmtKind::Function { name, function },
                        Body::Expr(_) => StmtKind::Const(name, Expr::arrow(function)),
                    });
                }
                let value = self.rhs(value)?;
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
            } if matches!(&action.kind, k::ActKind::Call(k::Callee::Import(slot))
                    if self.helper_import(slot, "ts.set"))
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
                via: Some(via),
                body,
                else_block,
            } if else_block.is_empty() && self.for_of(via) => StmtKind::ForOf {
                binding: self.binding(binding)?,
                iterable: self.expr(iterable)?,
                body: self.block(body, &site.child(1))?,
            },
            k::Stmt::While {
                metered: 0,
                condition,
                body,
                else_block,
            } if else_block.is_empty() => StmtKind::While {
                condition: self.condition(condition)?,
                body: self.block(body, &site.child(1))?,
            },
            k::Stmt::Break => StmtKind::Break,
            k::Stmt::Continue => StmtKind::Continue,
            k::Stmt::ReturnVia { value, policy } if self.is(policy, "ts.promise.adopt") => {
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
            k::Stmt::Throw { value } => StmtKind::Throw(self.expr(value)?),
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
        self.is(&via.open, "ts.for_of.open")
            && via
                .next
                .as_ref()
                .is_some_and(|next| self.is(next, "ts.for_of.next"))
            && via
                .item
                .as_ref()
                .is_some_and(|item| self.is(item, "ts.for_of.item"))
            && via.check.is_none()
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

    fn rhs(&mut self, value: &k::Rhs) -> Lift<Expr> {
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
        let k::Expr::Call { function, args } = condition else {
            return None;
        };
        let [
            k::Expr::Call {
                function: of,
                args: tested,
            },
            k::Expr::Literal(k::Literal::Text(written)),
        ] = args.as_slice()
        else {
            return None;
        };
        let [tested] = tested.as_slice() else {
            return None;
        };
        (self.is(function, "same") && self.is(of, "kind") && written == kind).then_some(tested)
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
    fn helper_read<'e>(&self, value: &'e k::Rhs, helper: &str) -> Option<&'e [k::Atom]> {
        match value {
            k::Rhs::Action(k::Action::Call {
                callee: k::Callee::Import(id),
                args,
            }) if self.is(id, helper) => Some(args),
            _ => None,
        }
    }

    /// `x.field`, its record read guarded by the brand check.
    fn field_ladder(
        &mut self,
        name: &Name,
        condition: &k::Expr,
        then_block: &[k::Stmt],
        else_block: &[k::Stmt],
    ) -> Lift<Option<Expr>> {
        let Some(target) = self.kind_test(condition, "record") else {
            return Ok(None);
        };
        let [
            k::Stmt::If {
                condition: unbranded,
                then_block: direct,
                else_block: guarded,
            },
        ] = then_block
        else {
            return Ok(None);
        };
        let k::Expr::Call {
            function: not,
            args,
        } = unbranded
        else {
            return Ok(None);
        };
        let [
            k::Expr::Call {
                function: contains,
                args: contained,
            },
        ] = args.as_slice()
        else {
            return Ok(None);
        };
        if !self.is(not, "bool.not")
            || !self.is(contains, "record.contains")
            || contained.as_slice()
                != [
                    target.clone(),
                    k::Expr::Literal(k::Literal::Text(lash_kernel_dialect::BRAND_FIELD.into())),
                ]
        {
            return Ok(None);
        }
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
        let helper = [
            atom_of(target),
            Some(k::Atom::Literal(k::Literal::Text(field.clone()))),
        ];
        let helper_args = |value: Option<&k::Rhs>| {
            value
                .and_then(|value| self.helper_read(value, "ts.read"))
                .is_some_and(|args| helper.iter().map(Option::as_ref).eq(args.iter().map(Some)))
        };
        if read != target
            || !helper_args(Self::sets(guarded, name))
            || !helper_args(Self::sets(else_block, name))
        {
            return Ok(None);
        }
        let target = self.expr(target)?;
        Ok(Some(Expr::Field(Box::new(target), field.clone())))
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
        let k::Expr::Call { function: lt, args } = bounds else {
            return Ok(None);
        };
        let [
            index,
            k::Expr::Call {
                function: len,
                args: measured,
            },
        ] = args.as_slice()
        else {
            return Ok(None);
        };
        let [target] = measured.as_slice() else {
            return Ok(None);
        };
        if !self.is(lt, "num.lt") || !self.is(len, "list.len") {
            return Ok(None);
        }
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
        let is_hole = matches!(hole, k::Expr::Call { function, args }
            if self.is(function, "same")
                && args.as_slice() == [k::Expr::Variable(name.clone()), k::Expr::Tuple(Vec::new())]);
        let reset = matches!(
            Self::sets(undefined, name),
            Some(k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)))
        );
        if set != name || read != target || at != index || !is_hole || !reset || !no_hole.is_empty()
        {
            return Ok(None);
        }
        let helper = [atom_of(target), atom_of(index)];
        let computed = |value: Option<&k::Rhs>| {
            value
                .and_then(|value| self.helper_read(value, "ts.get_numeric"))
                .is_some_and(|args| helper.iter().map(Option::as_ref).eq(args.iter().map(Some)))
        };
        if !computed(Self::sets(out_of_bounds, name))
            || otherwise.is_some_and(|otherwise| !computed(Self::sets(otherwise, name)))
        {
            return Ok(None);
        }
        let target = self.expr(target)?;
        let index = self.expr(index)?;
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
        let Some(k::Rhs::Expr(k::Expr::Call { function, args })) = Self::sets(then_block, name)
        else {
            return Ok(None);
        };
        let [
            k::Expr::Call {
                function: len,
                args: measured,
            },
        ] = args.as_slice()
        else {
            return Ok(None);
        };
        if !self.is(function, "num.to_float")
            || !self.is(len, "list.len")
            || measured.as_slice() != [target.clone()]
        {
            return Ok(None);
        }
        let Some(args) = Self::sets(else_block, name)
            .and_then(|value| self.helper_read(value, "ts.property.length"))
        else {
            return Ok(None);
        };
        if atom_of(target).as_ref().map(std::slice::from_ref) != Some(args) {
            return Ok(None);
        }
        let target = self.expr(target)?;
        Ok(Some(Expr::Field(Box::new(target), "length".to_owned())))
    }
}

/// The expression form of a statement action.
pub(super) fn act_of(action: &k::Action) -> k::ActExpr {
    let atoms = |args: &[k::Atom]| args.iter().map(expr_of).collect();
    match action {
        k::Action::Call { callee, args } => k::ActExpr {
            kind: k::ActKind::Call(callee.clone()),
            args: atoms(args),
        },
        k::Action::Perform {
            slot,
            args,
            result,
            then_yield,
        } => k::ActExpr {
            kind: k::ActKind::Perform {
                slot: *slot,
                result: result.clone(),
                then_yield: *then_yield,
            },
            args: atoms(args),
        },
        k::Action::Join { task } => k::ActExpr {
            kind: k::ActKind::Join,
            args: vec![expr_of(task)],
        },
        k::Action::JoinMany { mode, tasks } => k::ActExpr {
            kind: k::ActKind::JoinMany(*mode),
            args: vec![expr_of(tasks)],
        },
        k::Action::Yield => k::ActExpr {
            kind: k::ActKind::Yield,
            args: Vec::new(),
        },
        k::Action::Spawn { callee, args } => k::ActExpr {
            kind: k::ActKind::Spawn(callee.clone()),
            args: atoms(args),
        },
        k::Action::Cancel { task } => k::ActExpr {
            kind: k::ActKind::Cancel,
            args: vec![expr_of(task)],
        },
        k::Action::Await { policy, value } => k::ActExpr {
            kind: k::ActKind::Await(policy.clone()),
            args: vec![expr_of(value)],
        },
    }
}

fn expr_of(atom: &k::Atom) -> k::Expr {
    match atom {
        k::Atom::Variable(name) => k::Expr::Variable(name.clone()),
        k::Atom::Literal(literal) => k::Expr::Literal(literal.clone()),
    }
}

fn atom_of(expr: &k::Expr) -> Option<k::Atom> {
    match expr {
        k::Expr::Variable(name) => Some(k::Atom::Variable(name.clone())),
        k::Expr::Literal(literal) => Some(k::Atom::Literal(literal.clone())),
        _ => None,
    }
}
