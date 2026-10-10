//! Functions as closures of the dialect's calling convention,
//! `fn(this, args)`.

use lash_kernel_doc::{Expr, Float, Literal, RecordEntry, Rhs, Stmt};

use super::patterns::Mode;
use super::{BindingKind, Buf, FunctionFrame, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;

impl Lowerer<'_> {
    /// What the source wrote of `function`: its signature as text, and the
    /// signature a host starts it under when every parameter is a plain
    /// name of a durable type.
    pub(super) fn written(&self, function: &ast::Function) -> serde_json::Value {
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
        let mut written = serde_json::Map::new();
        written.insert("signature".to_owned(), signature.into());
        if let Ok(start) = self.process_signature(function)
            && let Ok(start) = serde_json::to_value(start)
        {
            written.insert("start".to_owned(), start);
        }
        serde_json::Value::Object(written)
    }

    /// A function's value. Inside its own body a named function expression
    /// is reachable by its name.
    pub(super) fn lower_function(&mut self, function: &ast::Function) -> Lowering<Operand> {
        if function.is_async {
            return self.lower_async_function(function, self.span);
        }
        let own = self.own_properties(function);
        self.closure(function, Some(&own))
    }

    /// A function's own `name` and `length` (ECMA-262 SetFunctionName and
    /// SetFunctionLength), made with the function: its name, or the one
    /// named evaluation gave it, and the count of its parameters before the
    /// first default or rest. The function gives this record when it is
    /// called with no argument list, which no JavaScript call does
    /// (`ts.own`); the helpers read, test and delete the two through it.
    pub(super) fn own_properties(&mut self, function: &ast::Function) -> Operand {
        let length = function
            .params
            .iter()
            .take_while(|param| {
                !matches!(param, ast::Pattern::Rest(_) | ast::Pattern::Assign { .. })
            })
            .count();
        #[expect(clippy::cast_precision_loss, reason = "a function's parameter count")]
        let length = Float::new(length as f64);
        let record = Expr::Record(vec![
            RecordEntry {
                field: "name".to_owned(),
                value: Expr::Literal(Literal::Text(function.name.clone().unwrap_or_default())),
            },
            RecordEntry {
                field: "length".to_owned(),
                value: Expr::Literal(Literal::Float(length)),
            },
        ]);
        let own = self.fresh("own");
        self.emit(Stmt::Let {
            name: own.clone(),
            value: Rhs::Expr(record),
        });
        Operand::variable(own, Ty::Unknown)
    }

    /// Opens a function's `body` with its answer to a call with no argument
    /// list: `own`, its own properties, before anything else runs. The
    /// argument list is tested by taking its length, which is charged the
    /// same whatever the arguments hold and raises only for no list.
    pub(super) fn answer_own(
        &mut self,
        body: &mut Buf,
        args: &lash_kernel_doc::Name,
        own: &Operand,
    ) -> Lowering<()> {
        let prologue = self.block(|this| {
            let probe = this.block(|this| {
                let counted = this.native("list.len", vec![Expr::Variable(args.clone())])?;
                let name = this.fresh("arity");
                this.emit(Stmt::Let {
                    name,
                    value: Rhs::Expr(counted),
                });
                Ok(())
            })?;
            let answer = this.block(|this| {
                this.emit(Stmt::Return { value: own.expr() });
                Ok(())
            })?;
            let asked = this.fresh("asked");
            this.emit_try(probe, Some((asked, answer)), None);
            Ok(())
        })?;
        body.stmts.splice(0..0, prologue.stmts);
        body.notes.splice(0..0, prologue.notes);
        Ok(())
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
            let mut function = function.clone();
            function.name = Some(name.to_owned());
            // An inferred display name does not create the internal binding
            // of a named function expression.
            self.lower_function(&function)
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
            return self.lower_function(function);
        };
        self.push_scope();
        let kernel = self.declare(name, BindingKind::Function);
        let result = self.lower_function(function).map(|closure| {
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
    /// The closure that is the function's value answers with `own`.
    pub(super) fn closure(
        &mut self,
        function: &ast::Function,
        own: Option<&Operand>,
    ) -> Lowering<Operand> {
        let this = self.fresh("this");
        let args = self.fresh("args");
        let arguments = (!function.is_arrow).then(|| self.fresh("arguments"));
        self.functions.push(FunctionFrame {
            arrow: function.is_arrow,
            this: Some(this.clone()),
            args: arguments.clone(),
            arguments_used: false,
            controls: Vec::new(),
        });
        let outer_span = self.span;
        let body = self.scoped_block(|lowerer| {
            lowerer.parameters(&function.params, &args)?;
            match &function.body {
                ast::FunctionBody::Block(statements) => {
                    lowerer.declare_vars(statements);
                    lowerer.declare_block(statements)?;
                    lowerer.lower_statements(statements)?;
                }
                ast::FunctionBody::Expression(value) => {
                    let value = lowerer.lower_expr(value)?;
                    lowerer.emit(Stmt::Return {
                        value: value.expr(),
                    });
                }
            }
            if !matches!(lowerer.buf.stmts.last(), Some(Stmt::Return { .. })) {
                // A kernel function that ends without `return` gives null;
                // a JavaScript one gives `undefined`.
                lowerer.emit(Stmt::Return {
                    value: Expr::Literal(Literal::Absent),
                });
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
                lowerer.buf.stmts.splice(0..0, prefix.stmts);
                lowerer.buf.notes.splice(0..0, prefix.notes);
            }
            Ok(())
        });
        self.span = outer_span;
        self.functions.pop();
        let mut body = body?;
        if let Some(own) = own {
            self.answer_own(&mut body, &args, own)?;
        }
        if self.in_cell_code() {
            self.written = Some(self.written(function));
        }
        let closure = self.emit_closure(vec![this, args], body);
        Ok(Operand {
            atom: closure.atom,
            ty: self.facts.function(function.return_ty.as_ref()),
        })
    }

    /// Binds a function's parameters from its argument list. A missing
    /// argument is `undefined`; an extra one is ignored.
    fn parameters(
        &mut self,
        params: &[ast::Pattern],
        args: &lash_kernel_doc::Name,
    ) -> Lowering<()> {
        let args = Operand::variable(args.clone(), Ty::Unknown);
        let positional = params
            .iter()
            .take_while(|param| !matches!(param, ast::Pattern::Rest(_)))
            .count();
        #[expect(clippy::cast_precision_loss, reason = "a function's parameter count")]
        let count = Operand::number(positional as f64);
        let padded = if positional == 0 {
            args.clone()
        } else {
            self.invoke("ts.pad", &[args.clone(), count.clone()], Ty::Unknown)?
        };
        for (index, param) in params.iter().enumerate() {
            let value = match param {
                ast::Pattern::Rest(_) => {
                    self.invoke("ts.rest", &[args.clone(), count.clone()], Ty::Unknown)?
                }
                _ => self.let_expr(Self::element(&padded, index), Ty::Unknown),
            };
            self.destructure(param, value, Mode::Local)?;
        }
        Ok(())
    }

    /// The enclosing function's `this`, which an arrow shares with the
    /// function around it.
    pub(super) fn this_binding(&self) -> Option<lash_kernel_doc::Name> {
        self.functions
            .iter()
            .rev()
            .find(|frame| !frame.arrow)
            .and_then(|frame| frame.this.clone())
    }

    /// The enclosing function's argument list, which is its `arguments`.
    pub(super) fn arguments_binding(&mut self) -> Option<lash_kernel_doc::Name> {
        self.functions
            .iter_mut()
            .rev()
            .find(|frame| !frame.arrow)
            .and_then(|frame| {
                frame.arguments_used = true;
                frame.args.clone()
            })
    }
}
