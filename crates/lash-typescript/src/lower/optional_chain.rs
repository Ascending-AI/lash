//! Optional chains: `a?.b`, `a?.()`, and the member calls inside one, which
//! keep the object the callee was read from as the call's receiver.

use super::*;

impl Lowerer {
    pub(super) fn lower_optional_chain(
        &mut self,
        base: &Expr,
        operations: &[OptionalOperation],
    ) -> Result<LashExpr, Diagnostic> {
        // `object.name?.(args)`: the member read is the chain's first step, so
        // the call keeps `object` as its receiver.
        if let (
            Expr::Member {
                object, property, ..
            },
            Some(OptionalOperation::Call { .. }),
        ) = (base, operations.first())
            && !matches!(object.as_ref(), Expr::Ident(owner, _)
                if (owner == "globalThis" || is_ecma_global_namespace(owner))
                    && !self.has_binding(owner))
        {
            let mut steps = vec![OptionalOperation::Member {
                property: property.clone(),
                optional: false,
            }];
            steps.extend(operations.iter().cloned());
            return self.lower_optional_chain(object, &steps);
        }
        // `(a?.b)(args)`: parentheses end the inner chain but keep its
        // reference, so the call's receiver is the object `b` was read from
        // (`undefined` when the inner chain short-circuited).
        if let (
            Expr::OptionalChain {
                base: inner_base,
                operations: inner_operations,
            },
            Some((
                OptionalOperation::Call {
                    args,
                    optional: optional_call,
                },
                rest,
            )),
        ) = (base, operations.split_first())
            && matches!(
                inner_operations.last(),
                Some(OptionalOperation::Member { .. })
            )
        {
            let receiver = self.temporary("chain_receiver");
            let callee = self.temporary("chain_callee");
            let outer_capture = self.optional_receiver_capture.replace(receiver.clone());
            let inner = self.lower_optional_chain(inner_base, inner_operations);
            self.optional_receiver_capture = outer_capture;
            let inner = inner?;
            let arguments = self.lower_argument_list(args)?;
            let next = self.temporary("optional_value");
            let call = LashExpr::Block(vec![
                Self::temp_assignment(
                    &next,
                    LashExpr::BuiltinCall {
                        name: "__typescript_call_method_dynamic".into(),
                        args: vec![
                            Self::variable(&receiver),
                            Self::variable(&callee),
                            arguments,
                        ],
                    },
                ),
                self.lower_optional_operations(Self::variable(&next), rest)?,
            ]);
            let call = if *optional_call {
                LashExpr::If {
                    condition: Box::new(Self::nullish(Self::variable(&callee))),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(call),
                }
            } else {
                call
            };
            return Ok(LashExpr::Block(vec![
                Self::temp_assignment(&receiver, LashExpr::Undefined),
                Self::temp_assignment(&callee, inner),
                call,
            ]));
        }
        // Only this chain's own final member read stores the receiver; a
        // chain nested in one of its keys or arguments does not.
        let capture = self.optional_receiver_capture.take();
        let current = self.temporary("optional_chain");
        let base = self.lower_expr(base)?;
        let tail = self.lower_optional_steps(Self::variable(&current), operations, capture)?;
        Ok(LashExpr::Block(vec![
            Self::temp_assignment(&current, base),
            tail,
        ]))
    }

    fn lower_optional_operations(
        &mut self,
        current: LashExpr,
        operations: &[OptionalOperation],
    ) -> Result<LashExpr, Diagnostic> {
        self.lower_optional_steps(current, operations, None)
    }

    /// `capture` names the slot the chain's final member read stores its
    /// object in, for a parenthesized chain that is then called.
    fn lower_optional_steps(
        &mut self,
        current: LashExpr,
        operations: &[OptionalOperation],
        capture: Option<String>,
    ) -> Result<LashExpr, Diagnostic> {
        let Some((operation, tail)) = operations.split_first() else {
            return Ok(current);
        };
        let optional = match operation {
            OptionalOperation::Member { optional, .. }
            | OptionalOperation::Call { optional, .. } => *optional,
        };
        // `value?.method(args)` and `value?.a.method(args)` call a method of
        // the chain's current value: lowered as the ordinary
        // `receiver.method(args)` on that value, so a builtin method
        // dispatches as it does outside a chain rather than being read as a
        // field (which a string or array does not have) and called.
        if let (
            OptionalOperation::Member {
                property: MemberProperty::Field(method),
                ..
            },
            Some((
                OptionalOperation::Call {
                    args,
                    optional: false,
                },
                rest,
            )),
        ) = (operation, tail.split_first())
        {
            let apply = self.lower_chain_method_call(&current, method, args)?;
            let next = self.temporary("optional_value");
            let continuation = LashExpr::Block(vec![
                Self::temp_assignment(&next, apply),
                self.lower_optional_steps(Self::variable(&next), rest, capture)?,
            ]);
            return Ok(if optional {
                LashExpr::If {
                    condition: Box::new(Self::nullish(current)),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(continuation),
                }
            } else {
                continuation
            });
        }
        // `value?.[key](args)`, `value.name?.(args)`: any other member read
        // followed by a call calls the member with the chain's current value
        // as its receiver, exactly as the same call outside a chain.
        if let (
            OptionalOperation::Member { property, .. },
            Some((
                OptionalOperation::Call {
                    args,
                    optional: optional_call,
                },
                rest,
            )),
        ) = (operation, tail.split_first())
        {
            let callee = self.temporary("optional_callee");
            let read = match property {
                MemberProperty::Field(field) => LashExpr::Field {
                    target: Box::new(current.clone()),
                    field: field.as_str().into(),
                },
                MemberProperty::Index(index) => LashExpr::Index {
                    target: Box::new(current.clone()),
                    index: Box::new(self.lower_expr(index)?),
                },
            };
            let arguments = self.lower_argument_list(args)?;
            let next = self.temporary("optional_value");
            let call = LashExpr::Block(vec![
                Self::temp_assignment(
                    &next,
                    LashExpr::BuiltinCall {
                        name: "__typescript_call_method_dynamic".into(),
                        args: vec![current.clone(), Self::variable(&callee), arguments],
                    },
                ),
                self.lower_optional_steps(Self::variable(&next), rest, capture)?,
            ]);
            let call = if *optional_call {
                LashExpr::If {
                    condition: Box::new(Self::nullish(Self::variable(&callee))),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(call),
                }
            } else {
                call
            };
            let continuation = LashExpr::Block(vec![Self::temp_assignment(&callee, read), call]);
            return Ok(if optional {
                LashExpr::If {
                    condition: Box::new(Self::nullish(current)),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(continuation),
                }
            } else {
                continuation
            });
        }
        let apply = match operation {
            OptionalOperation::Member { property, .. } => match property {
                MemberProperty::Field(field) => LashExpr::Field {
                    target: Box::new(current.clone()),
                    field: field.as_str().into(),
                },
                MemberProperty::Index(index) => LashExpr::Index {
                    target: Box::new(current.clone()),
                    index: Box::new(self.lower_expr(index)?),
                },
            },
            OptionalOperation::Call { args, .. } => {
                self.lower_dynamic_call_value(current.clone(), args)?
            }
        };
        let next = self.temporary("optional_value");
        let mut continuation = Vec::new();
        let capture = match (tail.is_empty(), operation, capture) {
            (true, OptionalOperation::Member { .. }, Some(receiver)) => {
                continuation.push(Self::temp_assignment(&receiver, current.clone()));
                None
            }
            (_, _, capture) => capture,
        };
        continuation.push(Self::temp_assignment(&next, apply));
        continuation.push(self.lower_optional_steps(Self::variable(&next), tail, capture)?);
        let continuation = LashExpr::Block(continuation);
        if optional {
            Ok(LashExpr::If {
                condition: Box::new(Self::nullish(current)),
                then_block: Box::new(LashExpr::Undefined),
                else_block: Box::new(continuation),
            })
        } else {
            Ok(continuation)
        }
    }
}
