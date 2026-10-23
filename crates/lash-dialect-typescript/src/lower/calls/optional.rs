//! Optional references retain receiver and evaluated member name until consumed.
use super::{Buf, Expr, Link, Lowerer, Lowering, Operand, Place, Ty, ast};

impl Lowerer<'_> {
    /// `base?.a.b?.(x)`: each `?.` ends the whole chain with `undefined`
    /// when what stands before it is `null` or `undefined`.
    pub(in crate::lower) fn lower_optional_chain(
        &mut self,
        base: &ast::Expr,
        operations: &[ast::OptionalOperation],
    ) -> Lowering<Operand> {
        self.optional_reference(base, operations, false)
            .map(|(value, _, _)| value)
    }

    pub(in crate::lower) fn lower_delete_chain(
        &mut self,
        base: &ast::Expr,
        operations: &[ast::OptionalOperation],
    ) -> Lowering<Operand> {
        self.optional_reference(base, operations, true)
            .map(|(value, _, _)| value)
    }

    /// Parentheses end short-circuiting but retain a member reference's receiver.
    pub(super) fn optional_reference(
        &mut self,
        base: &ast::Expr,
        operations: &[ast::OptionalOperation],
        deleting: bool,
    ) -> Lowering<(Operand, Operand, Operand)> {
        let deleting_base = deleting && operations.is_empty();
        let pair = self.expression_block(|this| {
            let link = match base {
                ast::Expr::Member {
                    object, property, ..
                } if this.global_path(base).is_none() => {
                    let object = this.lower_expr(object)?;
                    let object = this.pin(object);
                    let key = this.lower_key(property)?;
                    let key = this.reference_key(&object, key)?;
                    Link::Member { object, key }
                }
                ast::Expr::OptionalChain { base, operations } => {
                    let (value, receiver, name) =
                        this.optional_reference(base, operations, deleting_base)?;
                    Link::Value {
                        value,
                        receiver,
                        name,
                    }
                }
                _ => {
                    let value = this.lower_expr(base)?;
                    Link::Value {
                        value: this.pin(value),
                        receiver: Operand::undefined(),
                        name: Operand::undefined(),
                    }
                }
            };
            this.chain(link, operations, deleting)
        })?;
        let pair = self.pin(pair);
        let value = self.let_expr(Self::element(&pair, 0), Ty::Unknown);
        let receiver = self.let_expr(Self::element(&pair, 1), Ty::Unknown);
        let name = self.let_expr(Self::element(&pair, 2), Ty::Unknown);
        Ok((value, receiver, name))
    }

    fn chain(
        &mut self,
        link: Link,
        operations: &[ast::OptionalOperation],
        deleting: bool,
    ) -> Lowering<Operand> {
        let Some((operation, rest)) = operations.split_first() else {
            let (value, receiver, name) = match link {
                Link::Value {
                    value,
                    receiver,
                    name,
                } => {
                    let value = if deleting {
                        self.discard(value);
                        Operand::bool(true)
                    } else {
                        value
                    };
                    (value, receiver, name)
                }
                Link::Member { object, key } => {
                    let value = if deleting {
                        self.invoke("ts.delete", &[object.clone(), key.operand()], Ty::Bool)?
                    } else {
                        self.get_member(&object, &key)?
                    };
                    (value, object, key.operand())
                }
            };
            return Ok(self.let_expr(
                Expr::Tuple(vec![value.expr(), receiver.expr(), name.expr()]),
                Ty::Unknown,
            ));
        };
        match operation {
            ast::OptionalOperation::Member { property, optional } => {
                let object = match link {
                    Link::Value { value, .. } => value,
                    Link::Member { object, key } => self.get_member(&object, &key)?,
                };
                let object = self.pin(object);
                self.unless_nullish(*optional, &object, deleting, |this| {
                    let key = this.lower_key(property)?;
                    let key = this.reference_key(&object, key)?;
                    this.chain(
                        Link::Member {
                            object: object.clone(),
                            key,
                        },
                        rest,
                        deleting,
                    )
                })
            }
            ast::OptionalOperation::Call { args, optional } => {
                let (function, receiver, name) = match link {
                    Link::Value {
                        value,
                        receiver,
                        name,
                    } => (value, receiver, name),
                    Link::Member { object, key } => {
                        (self.get_member(&object, &key)?, object, key.operand())
                    }
                };
                let function = self.pin(function);
                self.unless_nullish(*optional, &function, deleting, |lowerer| {
                    let args = lowerer.arguments(args)?;
                    let value = lowerer.apply_method(function.clone(), receiver, name, args)?;
                    lowerer.chain(
                        Link::Value {
                            value,
                            receiver: Operand::undefined(),
                            name: Operand::undefined(),
                        },
                        rest,
                        deleting,
                    )
                })
            }
        }
    }

    /// Runs `lower` in place, or, for an optional link, only when `value`
    /// is neither `null` nor `undefined`.
    fn unless_nullish(
        &mut self,
        optional: bool,
        value: &Operand,
        deleting: bool,
        lower: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        if !optional {
            return lower(self);
        }
        let nullish = self.nullish_condition(value)?;
        let onward = self.value_block(lower)?;
        let absent = self.let_expr(
            Expr::Tuple(vec![
                if deleting {
                    Operand::bool(true).expr()
                } else {
                    Operand::undefined().expr()
                },
                Operand::undefined().expr(),
                Operand::undefined().expr(),
            ]),
            Ty::Unknown,
        );
        if self.branch_expression(&onward.0, onward.1.expr()).is_none() {
            let place = Place::Variable(super::super::statements::variable_of(&absent));
            let branch = self.store_branch(onward, place);
            self.emit_if(nullish, Buf::default(), branch);
            return Ok(absent);
        }
        Ok(self.choose(nullish, (Buf::default(), absent), onward))
    }
}
