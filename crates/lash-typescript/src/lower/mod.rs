use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    AssignPathStep, AssignTarget, CatchClause, Expr as LashExpr, FunctionExpr, JavaScriptBinaryOp,
    JavaScriptLogicalOp, JavaScriptUnaryOp, Program as LashProgram, TryExpr,
};

use crate::adapter::{
    self, AssignTarget as TsAssignTarget, BinaryOp, Expr, Function, FunctionBody, LogicalOp,
    MemberProperty, Stmt, UnaryOp, VarKind,
};
use crate::{Diagnostic, DiagnosticCode};

pub(crate) fn lower(program: &adapter::Program) -> Result<LashProgram, Diagnostic> {
    let mut lowerer = Lowerer::default();
    lowerer.scopes.push(Scope::default());
    let expressions = lowerer.lower_statements(&program.statements, true)?;
    Ok(LashProgram::block(expressions))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingKind {
    Const,
    Let,
    Function,
    Parameter,
    Catch,
}

impl BindingKind {
    fn mutable(self) -> bool {
        self == Self::Let
    }
}

#[derive(Clone, Debug)]
struct Binding {
    internal: String,
    kind: BindingKind,
    initialized: bool,
    owner_function: usize,
}

#[derive(Clone, Debug, Default)]
struct Scope {
    bindings: BTreeMap<String, Binding>,
}

#[derive(Default)]
struct FunctionContext {
    id: usize,
    captures: BTreeSet<String>,
}

#[derive(Default)]
struct Lowerer {
    scopes: Vec<Scope>,
    functions: Vec<FunctionContext>,
    next_binding: usize,
    next_function: usize,
    loop_depth: usize,
}

impl Lowerer {
    fn current_function(&self) -> usize {
        self.functions.last().map_or(0, |function| function.id)
    }

    fn has_binding(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.bindings.contains_key(name))
    }

    fn lower_statements(
        &mut self,
        statements: &[Stmt],
        root: bool,
    ) -> Result<Vec<LashExpr>, Diagnostic> {
        if !root {
            self.scopes.push(Scope::default());
        }
        self.predeclare(statements, root)?;

        let mut output = Vec::new();
        for statement in statements {
            if let Stmt::Function { name, function } = statement {
                let binding = self.binding(name)?.clone();
                let function = self.lower_function(function, Some(binding.internal.clone()))?;
                output.push(LashExpr::Assign {
                    target: AssignTarget::variable(binding.internal.into()),
                    expr: Box::new(function),
                });
            } else {
                output.extend(self.lower_stmt(statement)?);
            }
        }
        if !root {
            self.scopes.pop();
        }
        Ok(output)
    }

    fn predeclare(&mut self, statements: &[Stmt], root: bool) -> Result<(), Diagnostic> {
        for statement in statements {
            match statement {
                Stmt::Var { kind, declarations } => {
                    for declaration in declarations {
                        self.declare(
                            &declaration.name,
                            match kind {
                                VarKind::Const => BindingKind::Const,
                                VarKind::Let => BindingKind::Let,
                            },
                            false,
                            root,
                        )?;
                    }
                }
                Stmt::Function { name, .. } => {
                    self.declare(name, BindingKind::Function, true, root)?
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn declare(
        &mut self,
        name: &str,
        kind: BindingKind,
        initialized: bool,
        preserve_name: bool,
    ) -> Result<(), Diagnostic> {
        let owner_function = self.current_function();
        let scope = self.scopes.last_mut().expect("a scope is always active");
        if scope.bindings.contains_key(name) {
            return Err(Diagnostic::new(
                DiagnosticCode::DuplicateBinding,
                format!("duplicate lexical binding `{name}`"),
                None,
            ));
        }
        let internal = if preserve_name {
            name.to_string()
        } else {
            let id = self.next_binding;
            self.next_binding += 1;
            format!("__typescript_{id}_{name}")
        };
        scope.bindings.insert(
            name.to_string(),
            Binding {
                internal,
                kind,
                initialized,
                owner_function,
            },
        );
        Ok(())
    }

    fn binding(&self, name: &str) -> Result<&Binding, Diagnostic> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .ok_or_else(|| {
                Diagnostic::new(
                    DiagnosticCode::UnknownBinding,
                    format!("unknown binding `{name}`"),
                    None,
                )
            })
    }

    fn resolve(&mut self, name: &str) -> Result<String, Diagnostic> {
        let Some(binding) = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .cloned()
        else {
            return Err(Diagnostic::new(
                DiagnosticCode::UnknownBinding,
                format!("unknown binding `{name}`"),
                None,
            ));
        };
        let current_function = self.current_function();
        if current_function == binding.owner_function && !binding.initialized {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!("`{name}` is read before initialization"),
                None,
            ));
        }
        if current_function != binding.owner_function {
            if !binding.initialized {
                return Err(Diagnostic::new(
                    DiagnosticCode::TemporalDeadZone,
                    format!(
                        "captured binding `{name}` is not initialized when the closure is created"
                    ),
                    None,
                ));
            }
            if binding.kind.mutable() {
                return Err(Diagnostic::new(
                    DiagnosticCode::MutableCaptureUnsupported,
                    format!(
                        "mutable binding `{name}` cannot be captured until live lexical cells are available"
                    ),
                    None,
                ));
            }
            if let Some(function) = self.functions.last_mut() {
                function.captures.insert(binding.internal.clone());
            }
        }
        Ok(binding.internal)
    }

    fn initialize(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.get_mut(name) {
                binding.initialized = true;
                return;
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> Result<Vec<LashExpr>, Diagnostic> {
        Ok(match stmt {
            Stmt::Empty => Vec::new(),
            Stmt::Expr(expr) => vec![self.lower_expr(expr)?],
            Stmt::Block(statements) => {
                vec![LashExpr::Block(self.lower_statements(statements, false)?)]
            }
            Stmt::Var { kind, declarations } => {
                let mut output = Vec::with_capacity(declarations.len());
                for declaration in declarations {
                    if *kind == VarKind::Const && declaration.init.is_none() {
                        return Err(Diagnostic::new(
                            DiagnosticCode::MissingInitializer,
                            format!("const `{}` requires an initializer", declaration.name),
                            None,
                        ));
                    }
                    let value = declaration
                        .init
                        .as_ref()
                        .map(|expr| self.lower_expr(expr))
                        .transpose()?
                        .unwrap_or(LashExpr::Undefined);
                    let target = self.binding(&declaration.name)?.internal.clone();
                    self.initialize(&declaration.name);
                    output.push(LashExpr::Assign {
                        target: AssignTarget::variable(target.into()),
                        expr: Box::new(value),
                    });
                }
                output
            }
            Stmt::Function { .. } => unreachable!("function declarations are hoisted"),
            Stmt::Return(value) => {
                if self.functions.is_empty() {
                    return Err(Diagnostic::new(
                        DiagnosticCode::ReturnOutsideFunction,
                        "return is only valid in a function",
                        None,
                    ));
                }
                vec![LashExpr::Return(Box::new(
                    value
                        .as_ref()
                        .map(|expr| self.lower_expr(expr))
                        .transpose()?
                        .unwrap_or(LashExpr::Undefined),
                ))]
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => vec![LashExpr::If {
                condition: Box::new(self.lower_expr(test)?),
                then_block: Box::new(self.lower_stmt_block(consequent)?),
                else_block: Box::new(
                    alternate
                        .as_deref()
                        .map(|stmt| self.lower_stmt_block(stmt))
                        .transpose()?
                        .unwrap_or(LashExpr::Undefined),
                ),
            }],
            Stmt::While { test, body } => {
                self.loop_depth += 1;
                let body = self.lower_stmt_block(body)?;
                self.loop_depth -= 1;
                vec![LashExpr::While {
                    condition: Box::new(self.lower_expr(test)?),
                    body: Box::new(body),
                }]
            }
            Stmt::Break => {
                if self.loop_depth == 0 {
                    return Err(Diagnostic::new(
                        DiagnosticCode::LoopControlOutsideLoop,
                        "break is only valid in a loop",
                        None,
                    ));
                }
                vec![LashExpr::Break]
            }
            Stmt::Continue => {
                if self.loop_depth == 0 {
                    return Err(Diagnostic::new(
                        DiagnosticCode::LoopControlOutsideLoop,
                        "continue is only valid in a loop",
                        None,
                    ));
                }
                vec![LashExpr::Continue]
            }
            Stmt::Throw(value) => vec![LashExpr::Throw(Box::new(self.lower_expr(value)?))],
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                let body = LashExpr::Block(self.lower_statements(body, false)?);
                let catch = catch
                    .as_ref()
                    .map(|catch| {
                        self.scopes.push(Scope::default());
                        self.declare(&catch.binding, BindingKind::Catch, true, false)?;
                        let binding = self.binding(&catch.binding)?.internal.clone();
                        let body = LashExpr::Block(self.lower_statements(&catch.body, false)?);
                        self.scopes.pop();
                        Ok(CatchClause {
                            binding: binding.into(),
                            body: Box::new(body),
                        })
                    })
                    .transpose()?;
                let finally = finally
                    .as_ref()
                    .map(|statements| {
                        self.lower_statements(statements, false)
                            .map(|body| Box::new(LashExpr::Block(body)))
                    })
                    .transpose()?;
                vec![LashExpr::Try(Box::new(TryExpr {
                    body: Box::new(body),
                    catch,
                    finally,
                }))]
            }
        })
    }

    fn lower_stmt_block(&mut self, stmt: &Stmt) -> Result<LashExpr, Diagnostic> {
        let mut expressions = self.lower_stmt(stmt)?;
        expressions.push(LashExpr::Undefined);
        Ok(LashExpr::Block(expressions))
    }

    fn lower_function(
        &mut self,
        function: &Function,
        internal_name: Option<String>,
    ) -> Result<LashExpr, Diagnostic> {
        let outer_loop_depth = std::mem::take(&mut self.loop_depth);
        self.next_function += 1;
        let id = self.next_function;
        self.functions.push(FunctionContext {
            id,
            ..FunctionContext::default()
        });
        self.scopes.push(Scope::default());
        if let (Some(source_name), Some(internal)) = (&function.name, &internal_name) {
            self.scopes.last_mut().unwrap().bindings.insert(
                source_name.clone(),
                Binding {
                    internal: internal.clone(),
                    kind: BindingKind::Function,
                    initialized: true,
                    owner_function: id,
                },
            );
        }
        let mut params = Vec::with_capacity(function.params.len());
        for param in &function.params {
            self.declare(param, BindingKind::Parameter, true, true)?;
            params.push(self.binding(param)?.internal.clone().into());
        }
        let body = match &function.body {
            FunctionBody::Expression(expr) => LashExpr::Return(Box::new(self.lower_expr(expr)?)),
            FunctionBody::Block(statements) => {
                let mut body = self.lower_statements(statements, true)?;
                body.push(LashExpr::Undefined);
                LashExpr::Block(body)
            }
        };
        self.scopes.pop();
        let context = self.functions.pop().expect("function context exists");
        self.loop_depth = outer_loop_depth;
        Ok(LashExpr::Function(Box::new(FunctionExpr {
            name: internal_name.map(Into::into),
            params,
            captures: context.captures.into_iter().map(Into::into).collect(),
            body: Box::new(body),
        })))
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<LashExpr, Diagnostic> {
        Ok(match expr {
            Expr::Undefined => LashExpr::Undefined,
            Expr::Null => LashExpr::Null,
            Expr::Bool(value) => LashExpr::Bool(*value),
            Expr::Number(value) => LashExpr::Number(*value),
            Expr::String(value) => LashExpr::String(value.as_str().into()),
            Expr::Ident(name) if name == "undefined" && !self.has_binding(name) => {
                LashExpr::Undefined
            }
            Expr::Ident(name) if name == "NaN" && !self.has_binding(name) => {
                LashExpr::Number(f64::NAN)
            }
            Expr::Ident(name) if name == "Infinity" && !self.has_binding(name) => {
                LashExpr::Number(f64::INFINITY)
            }
            Expr::Ident(name) => LashExpr::Variable(self.resolve(name)?.into()),
            Expr::Array(items) => LashExpr::List(
                items
                    .iter()
                    .map(|item| self.lower_expr(item))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Object(entries) => LashExpr::Record(
                entries
                    .iter()
                    .map(|(name, value)| Ok((name.as_str().into(), self.lower_expr(value)?)))
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Expr::Assign { target, value } => {
                let target = self.lower_assign_target(target)?;
                let value = self.lower_expr(value)?;
                LashExpr::Assign {
                    target,
                    expr: Box::new(value),
                }
            }
            Expr::Member { object, property } => self.lower_member(object, property)?,
            Expr::Unary { op, value } => match op {
                UnaryOp::Void => {
                    LashExpr::Block(vec![self.lower_expr(value)?, LashExpr::Undefined])
                }
                UnaryOp::Plus => js_unary(JavaScriptUnaryOp::Plus, self.lower_expr(value)?),
                UnaryOp::Minus => js_unary(JavaScriptUnaryOp::Negate, self.lower_expr(value)?),
                UnaryOp::Not => js_unary(JavaScriptUnaryOp::Not, self.lower_expr(value)?),
                UnaryOp::TypeOf if matches!(value.as_ref(), Expr::Ident(name) if !self.has_binding(name)) => {
                    LashExpr::String("undefined".into())
                }
                UnaryOp::TypeOf => js_unary(JavaScriptUnaryOp::TypeOf, self.lower_expr(value)?),
            },
            Expr::Binary { left, op, right } => LashExpr::JavaScriptBinary {
                left: Box::new(self.lower_expr(left)?),
                op: map_binary(*op),
                right: Box::new(self.lower_expr(right)?),
            },
            Expr::Logical { left, op, right } => LashExpr::JavaScriptLogical {
                left: Box::new(self.lower_expr(left)?),
                op: match op {
                    LogicalOp::And => JavaScriptLogicalOp::And,
                    LogicalOp::Or => JavaScriptLogicalOp::Or,
                    LogicalOp::Nullish => JavaScriptLogicalOp::NullishCoalesce,
                },
                right: Box::new(self.lower_expr(right)?),
            },
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => LashExpr::If {
                condition: Box::new(self.lower_expr(test)?),
                then_block: Box::new(self.lower_expr(consequent)?),
                else_block: Box::new(self.lower_expr(alternate)?),
            },
            Expr::Template {
                quasis,
                expressions,
            } => {
                let mut value = LashExpr::String(quasis.first().map_or("", String::as_str).into());
                for (index, expression) in expressions.iter().enumerate() {
                    value = js_add(value, self.lower_expr(expression)?);
                    value = js_add(
                        value,
                        LashExpr::String(quasis.get(index + 1).map_or("", String::as_str).into()),
                    );
                }
                value
            }
            Expr::Function(function) => self.lower_function(function, None)?,
            Expr::Call { callee, args } => self.lower_call(callee, args)?,
        })
    }

    fn lower_assign_target(&mut self, target: &TsAssignTarget) -> Result<AssignTarget, Diagnostic> {
        match target {
            TsAssignTarget::Ident(name) => {
                let Some(binding) = self
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.bindings.get(name))
                    .cloned()
                else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnknownBinding,
                        format!("unknown binding `{name}`"),
                        None,
                    ));
                };
                if binding.kind != BindingKind::Let {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AssignConst,
                        format!("cannot assign to `{name}`"),
                        None,
                    ));
                }
                if binding.owner_function != self.current_function() {
                    return Err(Diagnostic::new(
                        DiagnosticCode::MutableCaptureUnsupported,
                        format!(
                            "mutable binding `{name}` cannot be captured until live lexical cells are available"
                        ),
                        None,
                    ));
                }
                Ok(AssignTarget::variable(binding.internal.into()))
            }
            TsAssignTarget::Member { object, property } => {
                self.member_assign_target(object, property)
            }
        }
    }

    fn member_assign_target(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<AssignTarget, Diagnostic> {
        let (root, mut steps) = self.member_path(object)?;
        steps.push(match property {
            MemberProperty::Field(field) => AssignPathStep::Field(field.as_str().into()),
            MemberProperty::Index(index) => AssignPathStep::Index(self.lower_expr(index)?),
        });
        Ok(AssignTarget {
            root: root.into(),
            steps,
        })
    }

    fn member_path(&mut self, expr: &Expr) -> Result<(String, Vec<AssignPathStep>), Diagnostic> {
        match expr {
            Expr::Ident(name) => Ok((self.resolve(name)?, Vec::new())),
            Expr::Member { object, property } => {
                let (root, mut steps) = self.member_path(object)?;
                steps.push(match property {
                    MemberProperty::Field(field) => AssignPathStep::Field(field.as_str().into()),
                    MemberProperty::Index(index) => AssignPathStep::Index(self.lower_expr(index)?),
                });
                Ok((root, steps))
            }
            _ => Err(Diagnostic::new(
                DiagnosticCode::UnsupportedExpression,
                "assignment target must start at a lexical binding",
                None,
            )),
        }
    }

    fn lower_member(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<LashExpr, Diagnostic> {
        let target = Box::new(self.lower_expr(object)?);
        Ok(match property {
            MemberProperty::Field(field) => LashExpr::Field {
                target,
                field: field.as_str().into(),
            },
            MemberProperty::Index(index) => LashExpr::Index {
                target,
                index: Box::new(self.lower_expr(index)?),
            },
        })
    }

    fn lower_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<LashExpr, Diagnostic> {
        if let Expr::Ident(name) = callee {
            if !self.has_binding(name) {
                return match (name.as_str(), args) {
                    ("finish", [value]) => Ok(LashExpr::Finish(Box::new(self.lower_expr(value)?))),
                    ("print", [value]) => Ok(LashExpr::Print(Box::new(self.lower_expr(value)?))),
                    ("finish" | "print", _) => Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("{name} expects one argument"),
                        None,
                    )),
                    _ => Ok(LashExpr::Call {
                        function: Box::new(self.lower_expr(callee)?),
                        args: args
                            .iter()
                            .map(|arg| self.lower_expr(arg))
                            .collect::<Result<_, _>>()?,
                    }),
                };
            }
        }
        if let Expr::Member {
            object,
            property: MemberProperty::Field(method),
        } = callee
        {
            if matches!(object.as_ref(), Expr::Ident(name) if name == "console") && method == "log"
            {
                if !self.has_binding("console") && args.len() == 1 {
                    return Ok(LashExpr::Print(Box::new(self.lower_expr(&args[0])?)));
                }
                if self.has_binding("console") {
                    return Ok(LashExpr::Call {
                        function: Box::new(self.lower_expr(callee)?),
                        args: args
                            .iter()
                            .map(|arg| self.lower_expr(arg))
                            .collect::<Result<_, _>>()?,
                    });
                }
            }
            let target = self.lower_expr(object)?;
            let lowered = args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<Vec<_>, _>>()?;
            let (builtin, builtin_args) = match (method.as_str(), lowered.as_slice()) {
                ("toUpperCase", []) => ("upper", vec![target]),
                ("toLowerCase", []) => ("lower", vec![target]),
                ("trim", []) => ("trim", vec![target]),
                ("startsWith", [arg]) => ("starts_with", vec![target, arg.clone()]),
                ("endsWith", [arg]) => ("ends_with", vec![target, arg.clone()]),
                ("includes", [arg]) => ("contains", vec![target, arg.clone()]),
                ("split", []) => ("__typescript_split", vec![target, LashExpr::Undefined]),
                ("split", [arg]) => ("__typescript_split", vec![target, arg.clone()]),
                ("join", []) => (
                    "__typescript_join",
                    vec![target, LashExpr::String(",".into())],
                ),
                ("join", [arg]) => ("__typescript_join", vec![target, arg.clone()]),
                _ => {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("method `{method}` is not in the accepted dialect"),
                        None,
                    ));
                }
            };
            return Ok(LashExpr::BuiltinCall {
                name: builtin.into(),
                args: builtin_args,
            });
        }
        Ok(LashExpr::Call {
            function: Box::new(self.lower_expr(callee)?),
            args: args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }
}

fn js_unary(op: JavaScriptUnaryOp, expr: LashExpr) -> LashExpr {
    LashExpr::JavaScriptUnary {
        op,
        expr: Box::new(expr),
    }
}

fn js_add(left: LashExpr, right: LashExpr) -> LashExpr {
    LashExpr::JavaScriptBinary {
        left: Box::new(left),
        op: JavaScriptBinaryOp::Add,
        right: Box::new(right),
    }
}

fn map_binary(op: BinaryOp) -> JavaScriptBinaryOp {
    match op {
        BinaryOp::Add => JavaScriptBinaryOp::Add,
        BinaryOp::Subtract => JavaScriptBinaryOp::Subtract,
        BinaryOp::Multiply => JavaScriptBinaryOp::Multiply,
        BinaryOp::Divide => JavaScriptBinaryOp::Divide,
        BinaryOp::Remainder => JavaScriptBinaryOp::Remainder,
        BinaryOp::StrictEqual => JavaScriptBinaryOp::StrictEqual,
        BinaryOp::StrictNotEqual => JavaScriptBinaryOp::StrictNotEqual,
        BinaryOp::LooseEqual => JavaScriptBinaryOp::LooseEqual,
        BinaryOp::LooseNotEqual => JavaScriptBinaryOp::LooseNotEqual,
        BinaryOp::Less => JavaScriptBinaryOp::Less,
        BinaryOp::LessEqual => JavaScriptBinaryOp::LessEqual,
        BinaryOp::Greater => JavaScriptBinaryOp::Greater,
        BinaryOp::GreaterEqual => JavaScriptBinaryOp::GreaterEqual,
    }
}
