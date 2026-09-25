//! Function values and calls: closures, plain calls, member calls with their
//! receiver, explicit-receiver calls, and the builtin-driven `map`.

use super::*;

impl<'module> Linker<'module> {
    pub(super) fn lower_function(
        &self,
        function: &crate::ast::FunctionExpr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        // A capture naming a cell local that was bound to a process literal is
        // not a capture at all: that literal lifted to a module-level
        // declaration, and a read of the name resolves statically to its
        // `ProcessRef` (`lower_variable`). Inside a *lifted* body the cell's
        // locals are gone, so the name is out of scope here — dropping it from
        // the closure's capture list is what lets one process literal name
        // another (FIG-2998). Where the name is still in scope — the cell's own
        // closures — nothing changes.
        let captures = function
            .captures
            .iter()
            .filter(|capture| {
                scope.get(capture).is_some()
                    || !self
                        .lifted_process_aliases
                        .borrow()
                        .contains_key(capture.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        for capture in &captures {
            if scope.get(capture).is_none() {
                return Err(LinkError::UnknownName {
                    name: capture.to_string(),
                    span: scope.span,
                });
            }
        }
        let mut function_scope = Scope::new(scope.process_body, scope.span);
        for capture in &captures {
            // A closure body sees its captures as `Any`: the value can be
            // reassigned between the closure's construction and its call, so
            // the type it had at construction proves nothing. A process value
            // is the exception — it is immutable by construction and its slot
            // type is what a trigger target and a start slot are checked
            // against — so that one type survives the boundary.
            let binding = match scope.get(capture) {
                Some(binding) if matches!(binding_type(&binding), TypeExpr::Process(_)) => binding,
                _ => any_binding(),
            };
            function_scope.bind(capture, binding);
        }
        for param in &function.params {
            function_scope.bind(param, any_binding());
        }
        if let Some(name) = &function.name {
            function_scope.bind(name, any_binding());
        }
        if let Some(receiver) = &function.receiver {
            function_scope.bind(receiver, any_binding());
        }
        let body = self
            .lower_expr(&function.body, &path.child(0), &mut function_scope)?
            .0;
        Ok((
            Expr::Function(Box::new(crate::ast::FunctionExpr {
                name: function.name.clone(),
                js_name: function.js_name.clone(),
                receiver: function.receiver.clone(),
                params: function.params.clone(),
                captures,
                body: Box::new(body),
            })),
            any_binding(),
        ))
    }

    pub(super) fn lower_call(
        &self,
        function: &Expr,
        args: &[Expr],
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        Ok((
            Expr::Call {
                function: Box::new(self.lower_expr(function, &path.child(0), scope)?.0),
                args: args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| {
                        self.lower_expr(arg, &path.child(index as u32 + 1), scope)
                            .map(|value| value.0)
                    })
                    .collect::<Result<_, _>>()?,
            },
            any_binding(),
        ))
    }

    /// A method call's children are, in [`Expr::children`] order, the
    /// receiver, a computed key when there is one, then the arguments.
    pub(super) fn lower_method_call(
        &self,
        receiver: &Expr,
        method: &crate::ast::MethodKey,
        args: &[Expr],
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        let (receiver, receiver_binding) = self.lower_expr(receiver, &path.child(0), scope)?;
        let (method, first_arg) = match method {
            crate::ast::MethodKey::Field(field) => {
                self.field_type(&binding_type(&receiver_binding), field.as_str(), scope.span)?;
                (crate::ast::MethodKey::Field(field.clone()), 1)
            }
            crate::ast::MethodKey::Index(key) => (
                crate::ast::MethodKey::Index(Box::new(
                    self.lower_expr(key, &path.child(1), scope)?.0,
                )),
                2,
            ),
        };
        Ok((
            Expr::MethodCall {
                receiver: Box::new(receiver),
                method,
                args: args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| {
                        self.lower_expr(arg, &path.child(index as u32 + first_arg), scope)
                            .map(|value| value.0)
                    })
                    .collect::<Result<_, _>>()?,
            },
            any_binding(),
        ))
    }

    pub(super) fn lower_this_call(
        &self,
        this: &Expr,
        function: &Expr,
        args: &[Expr],
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        Ok((
            Expr::ThisCall {
                this: Box::new(self.lower_expr(this, &path.child(0), scope)?.0),
                function: Box::new(self.lower_expr(function, &path.child(1), scope)?.0),
                args: args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| {
                        self.lower_expr(arg, &path.child(index as u32 + 2), scope)
                            .map(|value| value.0)
                    })
                    .collect::<Result<_, _>>()?,
            },
            any_binding(),
        ))
    }

    pub(super) fn lower_map(
        &self,
        items: &Expr,
        function: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        Ok((
            Expr::Map {
                items: Box::new(self.lower_expr(items, &path.child(0), scope)?.0),
                function: Box::new(self.lower_expr(function, &path.child(1), scope)?.0),
            },
            any_binding(),
        ))
    }
}
