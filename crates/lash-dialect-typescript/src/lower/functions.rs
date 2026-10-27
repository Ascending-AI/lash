//! Functions as closures of the dialect's calling convention,
//! `fn(this, args)`.

use lash_kernel_dialect::Written;
use lash_kernel_doc::{Expr, Float, Literal, Place, SourceLanguage, Stmt};

use super::patterns::Mode;
use super::{BindingKind, Buf, FunctionFrame, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;

impl Lowerer<'_> {
    /// What the source wrote of `function`: its signature as text. A call
    /// places its arguments by position, so it states no call shape.
    pub(super) fn written(&self, function: &ast::Function) -> Written {
        let text = |span: crate::SourceSpan| self.source.get(span.start..span.end).unwrap_or("");
        let mut params = Vec::with_capacity(function.params.len());
        for param in &function.params {
            params.push(match param {
                ast::Pattern::Ident(name, Some(annotation)) => {
                    format!("{name}: {}", text(annotation.span))
                }
                ast::Pattern::Ident(name, None) => name.clone(),
                ast::Pattern::Rest(_) => "...rest".to_owned(),
                _ => "_".to_owned(),
            });
        }
        let mut signature = format!("({})", params.join(", "));
        if let Some(annotation) = &function.return_ty {
            signature.push_str(" => ");
            signature.push_str(text(annotation.span));
        }
        Written {
            signature_origin: Some(self.entry_declaration(function).1),
            implicit_text_conversions: Vec::new(),
            captions: Vec::new(),
            language: SourceLanguage::TypeScript,
            signature,
            call: None,
        }
    }

    /// A function's value, whose `name` is `name`. Inside its own body a
    /// named function expression is reachable by its name.
    pub(super) fn lower_function(
        &mut self,
        function: &ast::Function,
        name: &str,
    ) -> Lowering<Operand> {
        if function.is_async {
            return self.lower_async_function(function, name, self.span);
        }
        let closure = self.closure(function, name)?;
        Ok(self.function_token(function, name, closure))
    }

    /// A function the program made, as the token a built-in function is
    /// (`builtins/mod.rs`): `("ts.function", "", name, length, closure,
    /// deleted)`. Its own `name` and `length` (ECMA-262 SetFunctionName and
    /// SetFunctionLength) are `name`, its own or the one named evaluation
    /// gave it, and the count of its parameters before the first default or
    /// rest. Its path is empty: `ts.identity` compares it by its closure.
    /// `deleted` is the record of the two the program has deleted, which
    /// then read Function.prototype's.
    pub(super) fn function_token(
        &mut self,
        function: &ast::Function,
        name: &str,
        closure: Operand,
    ) -> Operand {
        let (closure_expr, note) = match self.take_binding_of(&closure) {
            Some((lash_kernel_doc::Rhs::Expr(expr), note)) => (expr, Some(note)),
            _ => (closure.expr(), None),
        };
        let length = function
            .params
            .iter()
            .take_while(|param| {
                !matches!(param, ast::Pattern::Rest(_) | ast::Pattern::Assign { .. })
            })
            .count();
        #[expect(clippy::cast_precision_loss, reason = "a function's parameter count")]
        let length = Float::new(length as f64);
        let text = |value: &str| Expr::Literal(Literal::Text(value.to_owned()));
        let token = Expr::Tuple(vec![
            text(crate::FUNCTION_TAG),
            text(""),
            text(name),
            Expr::Literal(Literal::Float(length)),
            closure_expr,
            Expr::Record(Vec::new()),
        ]);
        if let Some(mut note) = note {
            if let Some(origin) = note
                .written
                .as_mut()
                .and_then(|written| written.signature_origin.as_mut())
                && origin.name.is_none()
                && !name.is_empty()
            {
                origin.name = Some(name.to_owned());
            }
            let value = self.let_expr(token, closure.ty);
            if let Some(last) = self.buf.last_note_mut() {
                last.blocks = note.blocks;
                last.nested_written = note.nested_written;
                if let Some(written) = note.written {
                    last.nested_written.push((vec![0, 4], written));
                }
            }
            value
        } else {
            self.inline(token, closure.ty)
        }
    }

    /// Anonymous function definitions receive a name only in named evaluation.
    pub(super) fn named_expression(
        &mut self,
        expression: &ast::Expr,
        name: &str,
    ) -> Lowering<Operand> {
        if let ast::Expr::Function(function) = expression
            && function.name.is_none()
        {
            // An inferred display name does not create the internal binding
            // of a named function expression. The function stays the
            // source's own, so its spellings name the bindings the type
            // analysis resolved.
            self.lower_function(function, name)
        } else {
            self.lower_expr(expression)
        }
    }

    /// A function expression: its name, if it has one, is a binding only
    /// its own body sees.
    pub(super) fn lower_function_expression(
        &mut self,
        function: &ast::Function,
    ) -> Lowering<Operand> {
        let Some(name) = function.name.as_deref().filter(|_| !function.is_arrow) else {
            return self.lower_function(function, function.name.as_deref().unwrap_or_default());
        };
        self.push_scope();
        let kernel = self.declare(name, BindingKind::Function);
        let result = self.lower_function(function, name).map(|closure| {
            let named = self
                .binding_mut(name)
                .is_some_and(|(_, binding)| binding.predeclared);
            if named {
                self.initialise(name, closure);
                Operand::variable(kernel, Ty::Unknown)
            } else {
                closure
            }
        });
        self.pop_scope();
        result
    }

    /// A function's parameters and body as a closure, whatever runs it.
    pub(super) fn closure(&mut self, function: &ast::Function, name: &str) -> Lowering<Operand> {
        let this = self.fresh("this");
        let args = self.fresh("args");
        let arguments = (!function.is_arrow).then(|| self.fresh("arguments"));
        self.functions.push(FunctionFrame {
            implicit_text_conversions: Vec::new(),
            arrow: function.is_arrow,
            async_body: function.is_async,
            this: Some(this.clone()),
            args: arguments.clone(),
            arguments_used: false,
            context_used: false,
            controls: Vec::new(),
            labels: Vec::new(),
        });
        let outer_span = self.span;
        let body = self.scoped_block(|lowerer| {
            if let ast::FunctionBody::Expression(expr) = &function.body {
                let value = lowerer.expression_block(|this| {
                    this.parameters(&function.params, &args)?;
                    this.lower_expr(expr)
                })?;
                lowerer.source_return(value)?;
            } else {
                lowerer.parameters(&function.params, &args)?;
                lowerer.function_body(function)?;
            }
            if lowerer.frame().arguments_used {
                let prefix = lowerer.block(|this| {
                    let object = this.invoke(
                        "ts.arguments",
                        &[Operand::variable(args.clone(), Ty::Unknown)],
                        Ty::Unknown,
                    )?;
                    let Some(arguments) = &arguments else {
                        unreachable!("only a non-arrow has arguments");
                    };
                    this.bind(arguments.clone(), object);
                    Ok(())
                })?;
                lowerer.buf.splice(0, prefix);
            }
            Ok(())
        });
        self.span = outer_span;
        let Some(frame) = self.functions.pop() else {
            unreachable!("function frame is active");
        };
        let body = body?;
        let mut written = self.written(function);
        written.implicit_text_conversions = frame.implicit_text_conversions;
        self.written = Some(written);
        let start = self.callable_presentation(function);
        let closure = self.emit_named_closure(vec![this, args], body, start, name);
        Ok(Operand {
            plain: None,
            text_prefix: None,
            expr: closure.expr,
            stable: closure.stable,
            ty: self.facts.function(function.return_ty.as_ref()),
        })
    }

    /// A workflow's declared function is its source body, with no closure
    /// creation or call around it. Only the packed convention needs the
    /// parameter-list bindings an ordinary JavaScript call uses.
    pub(super) fn entry_body(
        &mut self,
        function: &ast::Function,
    ) -> Lowering<(Vec<lash_kernel_value::names::Name>, Buf, bool)> {
        let this = self.fresh("this");
        let args = self.fresh("args");
        let arguments = self.fresh("arguments");
        self.functions.push(FunctionFrame {
            implicit_text_conversions: Vec::new(),
            arrow: false,
            async_body: function.is_async,
            this: Some(this.clone()),
            args: Some(arguments.clone()),
            arguments_used: false,
            context_used: false,
            controls: Vec::new(),
            labels: Vec::new(),
        });
        let span = self.span;
        let mut params = Vec::new();
        let defaults = function
            .params
            .iter()
            .any(|param| matches!(param, ast::Pattern::Assign { .. }));
        let body = self.scoped_block(|lowerer| {
            if defaults {
                lowerer.parameters(&function.params, &args)?;
                params = vec![this.clone(), args.clone()];
            } else {
                for param in &function.params {
                    let ast::Pattern::Ident(name, _) = param else {
                        unreachable!("an admitted entry has plain parameters");
                    };
                    params.push(lowerer.declare(name, BindingKind::Local));
                }
            }
            lowerer.function_body(function)?;
            if defaults && lowerer.frame().arguments_used {
                let prefix = lowerer.block(|lowerer| {
                    let object = lowerer.invoke(
                        "ts.arguments",
                        &[Operand::variable(args.clone(), Ty::Unknown)],
                        Ty::Unknown,
                    )?;
                    lowerer.bind(arguments.clone(), object);
                    Ok(())
                })?;
                lowerer.buf.splice(0, prefix);
            }
            if !defaults && (lowerer.frame().context_used || lowerer.frame().arguments_used) {
                let prefix = lowerer.block(|lowerer| {
                    lowerer.parameters(&function.params, &args)?;
                    if lowerer.frame().arguments_used {
                        let object = lowerer.invoke(
                            "ts.arguments",
                            &[Operand::variable(args.clone(), Ty::Unknown)],
                            Ty::Unknown,
                        )?;
                        lowerer.bind(arguments, object);
                    }
                    Ok(())
                })?;
                lowerer.buf.splice(0, prefix);
                params = vec![this, args];
            }
            Ok(())
        });
        self.span = span;
        let Some(frame) = self.functions.pop() else {
            unreachable!("entry frame is active");
        };
        Ok((
            params,
            body?,
            defaults || frame.context_used || frame.arguments_used,
        ))
    }

    pub(super) fn source_return(&mut self, value: Operand) -> Lowering<()> {
        let (value, note) = match self.take_binding_of(&value) {
            Some((rhs, note)) => (
                match rhs {
                    lash_kernel_doc::Rhs::Expr(expr) => expr,
                    lash_kernel_doc::Rhs::Action(action) => action.into(),
                },
                Some(note),
            ),
            None => (value.expr(), None),
        };
        let stmt = if self.frame().async_body {
            Stmt::ReturnVia {
                value,
                policy: self.function_target("ts.promise.adopt")?,
            }
        } else {
            Stmt::Return { value }
        };
        self.push(
            stmt,
            note.as_ref()
                .map_or_else(Vec::new, |note| note.blocks.clone()),
        );
        if let (Some(note), Some(last)) = (note, self.buf.last_note_mut()) {
            last.nested_written = note.nested_written;
            last.written = note.written;
        }
        Ok(())
    }

    /// The shared source body, after its parameter bindings.
    fn function_body(&mut self, function: &ast::Function) -> Lowering<()> {
        match &function.body {
            ast::FunctionBody::Block(statements) => {
                self.declare_vars(statements);
                self.declare_block(statements)?;
                self.lower_statements(statements)?;
            }
            ast::FunctionBody::Expression(value) => {
                let value = self.lower_expr(value)?;
                self.source_return(value)?;
            }
        }
        if !matches!(
            self.buf.last_stmt(),
            Some(Stmt::Return { .. } | Stmt::ReturnVia { .. })
        ) {
            self.source_return(Operand::undefined())?;
            self.buf.implicit_return = true;
        }
        Ok(())
    }

    /// A fixed callback's body takes the helper's operands directly. Extra
    /// helper operands have named unused slots so island calls need no adapter;
    /// missing source parameters are filled with absent by the kernel call.
    pub(super) fn callback_closure(
        &mut self,
        function: &ast::Function,
        arity: usize,
    ) -> Lowering<(Operand, bool, Written)> {
        let this = self.fresh("this");
        let mut params = vec![this.clone()];
        self.functions.push(FunctionFrame {
            implicit_text_conversions: Vec::new(),
            arrow: function.is_arrow,
            async_body: function.is_async,
            this: Some(this),
            args: None,
            arguments_used: false,
            context_used: false,
            controls: Vec::new(),
            labels: Vec::new(),
        });
        let outer_span = self.span;
        let body = self.scoped_block(|lowerer| {
            for pattern in &function.params {
                if let ast::Pattern::Ident(name, _) = pattern {
                    params.push(lowerer.declare(name, BindingKind::Local));
                } else {
                    let argument = lowerer.fresh("argument");
                    params.push(argument.clone());
                    lowerer.destructure(
                        pattern,
                        Operand::variable(argument, Ty::Unknown),
                        Mode::Local,
                    )?;
                }
            }
            lowerer.function_body(function)
        });
        self.span = outer_span;
        let Some(frame) = self.functions.pop() else {
            unreachable!("positional function frame is active");
        };
        let body = body?;
        params.resize_with(params.len().max(arity + 1), || self.fresh("unused"));
        let mut written = self.written(function);
        written.implicit_text_conversions = frame.implicit_text_conversions;
        self.written = Some(written.clone());
        Ok((
            self.emit_named_closure(
                params,
                body,
                None,
                function.name.as_deref().unwrap_or_default(),
            ),
            frame.context_used,
            written,
        ))
    }

    /// A packed exported entry normalizes arguments into one positional body.
    /// The body is a private captured closure, which saved-function freezing
    /// includes as an unnamed helper rather than a second exported function.
    pub(super) fn positional_closure(
        &mut self,
        function: &ast::Function,
        entry: &super::Positional,
    ) -> Lowering<Operand> {
        let (made, _, written) = self.callback_closure(function, entry.arity)?;
        self.store(Place::Variable(entry.name.clone()), made);

        let this = self.fresh("this");
        let args = self.fresh("args");
        let body = self.block(|lowerer| {
            let result = lowerer.expression_block(|lowerer| {
                let supplied = lowerer.supplied_arguments(entry.arity, &args)?;
                let mut passed = vec![Expr::Variable(this.clone())];
                for index in 0..entry.arity {
                    passed.push(Self::element(&supplied, index));
                }
                Ok(lowerer.action(
                    lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Value(
                        entry.name.clone(),
                    )),
                    passed,
                    Ty::Unknown,
                ))
            })?;
            lowerer.emit(Stmt::Return {
                value: result.expr(),
            });
            Ok(())
        })?;
        self.written = Some(written);
        let start = self.callable_presentation(function);
        let closure = self.emit_named_closure(
            vec![this, args],
            body,
            start,
            function.name.as_deref().unwrap_or_default(),
        );
        let closure = Operand {
            ty: self.facts.function(function.return_ty.as_ref()),
            ..closure
        };
        Ok(self.function_token(
            function,
            function.name.as_deref().unwrap_or_default(),
            closure,
        ))
    }

    /// Binds a function's parameters from its argument list. A missing
    /// argument is `undefined`; an extra one is ignored.
    fn parameters(
        &mut self,
        params: &[ast::Pattern],
        args: &lash_kernel_value::names::Name,
    ) -> Lowering<()> {
        let positional = params
            .iter()
            .take_while(|param| !matches!(param, ast::Pattern::Rest(_)))
            .count();
        let args_value = Operand::variable(args.clone(), Ty::Unknown);
        let supplied = self.supplied_arguments(positional, args)?;
        #[expect(clippy::cast_precision_loss, reason = "a function's parameter count")]
        let count = Operand::number(positional as f64);
        for (index, param) in params.iter().enumerate() {
            let value = match param {
                ast::Pattern::Rest(_) => {
                    self.invoke("ts.rest", &[args_value.clone(), count.clone()], Ty::Array)?
                }
                _ => self.let_expr(Self::element(&supplied, index), Ty::Unknown),
            };
            self.destructure(param, value, Mode::Local)?;
        }
        Ok(())
    }

    fn supplied_arguments(
        &mut self,
        positional: usize,
        args: &lash_kernel_value::names::Name,
    ) -> Lowering<Operand> {
        let args = Operand::variable(args.clone(), Ty::Unknown);
        #[expect(clippy::cast_precision_loss, reason = "a function's parameter count")]
        let count = Operand::number(positional as f64);
        if positional == 0 {
            return Ok(args);
        }
        let length = self.native("list.len", vec![args.expr()])?;
        let missing = self.native("num.lt", vec![length, count.expr()])?;
        let padded = self.invoke("ts.pad", &[args.clone(), count], Ty::Unknown)?;
        let supplied = self.inline(
            Expr::If {
                cond: Box::new(missing),
                then: Box::new(padded.expr()),
                r#else: Box::new(args.expr()),
            },
            Ty::Unknown,
        );
        Ok(self.pin(supplied))
    }

    /// The enclosing function's `this`, which an arrow shares with the
    /// function around it.
    pub(super) fn this_binding(&mut self) -> Option<lash_kernel_value::names::Name> {
        let binding = self
            .functions
            .iter_mut()
            .rev()
            .find(|frame| !frame.arrow)
            .and_then(|frame| {
                frame.context_used = true;
                frame.this.clone()
            });
        self.frame_mut().context_used = true;
        binding
    }

    /// The enclosing function's argument list, which is its `arguments`.
    pub(super) fn arguments_binding(&mut self) -> Option<lash_kernel_value::names::Name> {
        let binding = self
            .functions
            .iter_mut()
            .rev()
            .find(|frame| !frame.arrow)
            .and_then(|frame| {
                frame.arguments_used = true;
                frame.args.clone()
            });
        if binding.is_some() {
            self.frame_mut().context_used = true;
        }
        binding
    }
}
