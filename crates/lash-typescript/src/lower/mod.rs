use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    AssignPathStep, AssignTarget, CatchClause, Declaration, Expr as LashExpr, FunctionExpr,
    JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, ProcessDecl, ProcessParam,
    ProcessSignalDecl, ProcessStartExpr, Program as LashProgram, ResourceRefExpr, TryExpr,
    TypeExpr,
};

use crate::adapter::{
    self, AssignTarget as TsAssignTarget, BinaryOp, Expr, Function, FunctionBody, LogicalOp,
    MemberProperty, Stmt, UnaryOp, VarKind,
};
use crate::{Diagnostic, DiagnosticCode};

mod stdlib;
use stdlib::*;
mod loops;
use loops::*;
mod array_map;
mod graph;
use graph::{shortest_cycle_through, strongly_connected_components};

pub(crate) fn accepts_instance_method(method: &str) -> bool {
    stdlib::is_instance_stdlib_method(method)
}

pub(crate) fn accepted_instance_methods() -> &'static [&'static str] {
    stdlib::INSTANCE_STDLIB_METHODS
}

/// Every binding the lowerer generates carries this prefix, which the dialect
/// reserves so a source identifier can never collide with one.
pub(crate) const GENERATED_BINDING_PREFIX: &str = "__typescript_";

pub(crate) fn lower(program: &adapter::Program) -> Result<LashProgram, Diagnostic> {
    let mut lowerer = Lowerer::default();
    lowerer.scopes.push(Scope::default());
    let expressions = lowerer.lower_statements(&program.statements, true)?;
    Ok(LashProgram {
        declarations: lowerer.declarations,
        main: LashExpr::Block(expressions),
        declaration_spans: Vec::new(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    })
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

/// A hoisted function declaration awaiting a place in the emission order.
struct PendingFunction {
    internal: String,
    captures: BTreeSet<String>,
    definition: FunctionExpr,
}

/// A binding whose assignment is emitted once every name it captures holds a
/// value.
struct PendingBinding {
    internal: String,
    captures: BTreeSet<String>,
    assignment: LashExpr,
}

#[derive(Default)]
struct Lowerer {
    scopes: Vec<Scope>,
    functions: Vec<FunctionContext>,
    next_binding: usize,
    next_function: usize,
    loop_depth: usize,
    continue_epilogues: Vec<Option<LashExpr>>,
    process_depth: usize,
    await_depth: usize,
    declarations: Vec<Declaration>,
    process_bindings: BTreeMap<String, String>,
    process_handle_bindings: BTreeSet<String>,
    allow_uninitialized_declaration_capture: bool,
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

        let local_function_internals = statements
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Function { name, .. } => {
                    Some(self.binding(name).map(|binding| binding.internal.clone()))
                }
                _ => None,
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let current_function = self.current_function();
        let mut available = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .filter(|binding| {
                // A binding owned by an enclosing function reaches this frame as
                // a capture or a parameter, so it already holds a value whenever
                // these statements run. Only bindings this frame declares itself
                // have to be ordered against the declarations that fill them.
                (binding.initialized || binding.owner_function != current_function)
                    && !local_function_internals.contains(&binding.internal)
            })
            .map(|binding| binding.internal.clone())
            .collect::<BTreeSet<_>>();
        let mut pending = Vec::with_capacity(local_function_internals.len());
        let previous_capture_mode = self.allow_uninitialized_declaration_capture;
        self.allow_uninitialized_declaration_capture = true;
        for statement in statements {
            if let Stmt::Function { name, function } = statement {
                let binding = self.binding(name)?.clone();
                let function = self.lower_function(function, Some(binding.internal.clone()))?;
                let LashExpr::Function(definition) = function else {
                    unreachable!("function lowering returns a function expression")
                };
                pending.push(PendingFunction {
                    internal: binding.internal.clone(),
                    captures: definition
                        .captures
                        .iter()
                        .map(|capture| capture.as_str().to_string())
                        .collect(),
                    definition: *definition,
                });
            }
        }
        self.allow_uninitialized_declaration_capture = previous_capture_mode;
        reject_mutual_recursion(&pending, statements, self)?;
        let mut pending = pending
            .into_iter()
            .map(|function| PendingBinding {
                internal: function.internal.clone(),
                captures: function.captures,
                assignment: LashExpr::Assign {
                    target: AssignTarget::variable(function.internal.into()),
                    expr: Box::new(LashExpr::Function(Box::new(function.definition))),
                },
            })
            .collect::<Vec<_>>();

        let flush_ready = |pending: &mut Vec<PendingBinding>,
                           available: &mut BTreeSet<String>,
                           output: &mut Vec<LashExpr>| {
            while let Some(index) = pending.iter().position(|binding| {
                binding
                    .captures
                    .iter()
                    .all(|capture| available.contains(capture))
            }) {
                let binding = pending.remove(index);
                available.insert(binding.internal);
                output.push(binding.assignment);
            }
        };
        let mut output = Vec::new();
        for statement in statements {
            flush_ready(&mut pending, &mut available, &mut output);
            match statement {
                Stmt::Function { .. } => {}
                Stmt::Var { declarations, .. } => {
                    output.extend(self.lower_stmt(statement)?);
                    for declaration in declarations {
                        available.insert(self.binding(&declaration.name)?.internal.clone());
                    }
                }
                _ => output.extend(self.lower_stmt(statement)?),
            }
        }
        flush_ready(&mut pending, &mut available, &mut output);
        if let Some(function) = pending.first() {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!(
                    "function `{}` captures a binding that is unavailable at declaration time",
                    function.internal
                ),
                None,
            ));
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
        if name.starts_with(GENERATED_BINDING_PREFIX) {
            return Err(reserved_identifier(name));
        }
        // Mangling exists to stop an inner scope from overwriting an outer slot
        // of the same name. Where nothing of that name is visible there is
        // nothing to protect, and a mangled root-level binding would publish a
        // generated name into the durable globals and the bound-variables
        // prompt, so keep the author's name in that case.
        let preserve_name = preserve_name || !self.has_binding(name);
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
            format!("{GENERATED_BINDING_PREFIX}{id}_{name}")
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
            if !binding.initialized && !self.allow_uninitialized_declaration_capture {
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
            let first_capturing_function = self
                .functions
                .iter()
                .position(|function| function.id == binding.owner_function)
                .map_or(0, |owner| owner + 1);
            for function in &mut self.functions[first_capturing_function..] {
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
            Stmt::Expr(Expr::Update { name, delta, .. }) => {
                vec![self.lower_update_statement(name, *delta)?]
            }
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
                    let value = if let Some(init) = declaration.init.as_ref()
                        && is_define_process_call(init)
                    {
                        if *kind != VarKind::Const {
                            return Err(Diagnostic::new(
                                DiagnosticCode::ProcessDefinitionNotTopLevel,
                                "defineProcess must initialize a top-level const binding",
                                None,
                            ));
                        }
                        if self.scopes.len() != 1 || !self.functions.is_empty() {
                            return Err(Diagnostic::new(
                                DiagnosticCode::ProcessDefinitionNotTopLevel,
                                "defineProcess must initialize a top-level binding",
                                None,
                            ));
                        }
                        self.lower_process_definition(&declaration.name, init)?
                    } else {
                        declaration
                            .init
                            .as_ref()
                            .map(|expr| self.lower_expr(expr))
                            .transpose()?
                            .unwrap_or(LashExpr::Undefined)
                    };
                    let target = self.binding(&declaration.name)?.internal.clone();
                    if *kind == VarKind::Const && matches!(&value, LashExpr::StartProcess(_)) {
                        self.process_handle_bindings.insert(target.clone());
                    }
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
                self.continue_epilogues.push(None);
                let body = self.lower_stmt_block(body)?;
                self.continue_epilogues.pop();
                self.loop_depth -= 1;
                vec![LashExpr::While {
                    condition: Box::new(self.lower_expr(test)?),
                    body: Box::new(body),
                }]
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => vec![self.lower_classic_for(
                init.as_deref(),
                test.as_ref(),
                update.as_ref(),
                body,
            )?],
            Stmt::ForOf {
                binding,
                iterable,
                body,
            } => {
                if let Some(reason) = body_may_mutate_iterable(iterable, body) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::ForOfUnsupported,
                        format!(
                            "this for-of body {reason}; the v1 iterator walks a snapshot, so mutating the iterable mid-loop is not supported"
                        ),
                        None,
                    ));
                }
                self.scopes.push(Scope::default());
                self.declare(binding, BindingKind::Const, true, false)?;
                let internal = self.binding(binding)?.internal.clone();
                let iterable = LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![
                        LashExpr::String("Lash.ArrayFromIterable".into()),
                        self.lower_expr(iterable)?,
                    ],
                };
                self.loop_depth += 1;
                self.continue_epilogues.push(None);
                let body = self.lower_stmt_block(body)?;
                self.continue_epilogues.pop();
                self.loop_depth -= 1;
                self.scopes.pop();
                vec![LashExpr::For {
                    binding: internal.into(),
                    iterable: Box::new(iterable),
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
                vec![
                    self.continue_epilogues
                        .last()
                        .and_then(Clone::clone)
                        .map_or(LashExpr::Continue, |epilogue| {
                            LashExpr::Block(vec![epilogue, LashExpr::Continue])
                        }),
                ]
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
        let mut expressions = match stmt {
            Stmt::Block(statements) => self.lower_statements(statements, false)?,
            _ => self.lower_stmt(stmt)?,
        };
        expressions.push(LashExpr::Undefined);
        Ok(LashExpr::Block(expressions))
    }

    fn lower_function(
        &mut self,
        function: &Function,
        internal_name: Option<String>,
    ) -> Result<LashExpr, Diagnostic> {
        if function.is_async && self.process_depth == 0 {
            return Err(Diagnostic::new(
                DiagnosticCode::AsyncUnsupported,
                "async function authoring is limited to defineProcess run bodies in v1",
                None,
            ));
        }
        let outer_loop_depth = std::mem::take(&mut self.loop_depth);
        self.next_function += 1;
        let id = self.next_function;
        self.functions.push(FunctionContext {
            id,
            ..FunctionContext::default()
        });
        self.scopes.push(Scope::default());
        // ECMA binds a function's own name inside its body. A declaration
        // already owns an outer binding to reuse; a named function expression
        // needs a fresh one, visible only here, which the VM's self-slot fills.
        let internal_name = match (&function.name, internal_name) {
            (Some(source_name), None) => {
                if source_name.starts_with(GENERATED_BINDING_PREFIX) {
                    self.scopes.pop();
                    self.functions.pop();
                    self.loop_depth = outer_loop_depth;
                    return Err(reserved_identifier(source_name));
                }
                let generated = self.next_binding;
                self.next_binding += 1;
                Some(format!(
                    "{GENERATED_BINDING_PREFIX}{generated}_{source_name}"
                ))
            }
            (_, internal_name) => internal_name,
        };
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
            Expr::Await(inner) => self.lower_await(inner)?,
            Expr::Update { .. } => {
                return Err(Diagnostic::new(
                    DiagnosticCode::UpdateUnsupported,
                    "update expressions are supported only as standalone statements and classic-for updates in v1",
                    None,
                ));
            }
        })
    }

    fn lower_process_definition(
        &mut self,
        binding_name: &str,
        expression: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        let Expr::Call { args, .. } = expression else {
            unreachable!("caller identifies defineProcess calls")
        };
        let [Expr::Object(entries)] = args.as_slice() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessConfigLiteralRequired,
                "defineProcess expects one object literal",
                None,
            ));
        };
        let field = |name: &str| {
            entries
                .iter()
                .find_map(|(key, value)| (key == name).then_some(value))
        };
        let mut seen_fields = BTreeSet::new();
        if entries.iter().any(|(key, _)| {
            !matches!(key.as_str(), "name" | "signals" | "run") || !seen_fields.insert(key.as_str())
        }) {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessConfigFieldUnsupported,
                "defineProcess accepts only name, signals, and run",
                None,
            ));
        }
        let Some(Expr::String(process_name)) = field("name") else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessNameLiteralRequired,
                "defineProcess.name must be a string literal",
                None,
            ));
        };
        let signals = match field("signals") {
            None => Vec::new(),
            Some(Expr::Object(signals)) => signals
                .iter()
                .map(|(name, _)| ProcessSignalDecl {
                    name: name.as_str().into(),
                    ty: TypeExpr::Any,
                })
                .collect(),
            Some(_) => {
                return Err(Diagnostic::new(
                    DiagnosticCode::ProcessSignalsLiteralRequired,
                    "defineProcess.signals must be an object literal",
                    None,
                ));
            }
        };
        let Some(Expr::Function(run)) = field("run") else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessRunLiteralRequired,
                "defineProcess.run must be a function literal",
                None,
            ));
        };
        if !run.is_async {
            return Err(Diagnostic::new(
                DiagnosticCode::AsyncUnsupported,
                "defineProcess.run must be async",
                None,
            ));
        }
        if self
            .declarations
            .iter()
            .any(|declaration| matches!(declaration, Declaration::Process(process) if process.name.as_str() == process_name))
        {
            return Err(Diagnostic::new(
                DiagnosticCode::DuplicateBinding,
                format!("duplicate process name `{process_name}`"),
                None,
            ));
        }

        self.process_depth += 1;
        let function = self.lower_function(run, None)?;
        self.process_depth -= 1;
        let LashExpr::Function(function) = function else {
            unreachable!("run lowering returns a function")
        };
        if !function.captures.is_empty() {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessCaptureUnsupported,
                "defineProcess.run must receive durable inputs as parameters",
                None,
            ));
        }
        let params = function
            .params
            .iter()
            .map(|name| ProcessParam {
                name: name.clone(),
                ty: TypeExpr::Any,
            })
            .collect::<Vec<_>>();
        let call_args = function
            .params
            .iter()
            .map(|name| LashExpr::Variable(name.clone()))
            .collect();
        let failure_name = format!("{GENERATED_BINDING_PREFIX}process_error");
        self.declarations.push(Declaration::Process(ProcessDecl {
            name: process_name.as_str().into(),
            params,
            signals,
            return_ty: Some(TypeExpr::Any),
            label: None,
            body: LashExpr::Try(Box::new(TryExpr {
                body: Box::new(LashExpr::Finish(Box::new(LashExpr::Call {
                    function: Box::new(LashExpr::Function(function)),
                    args: call_args,
                }))),
                catch: Some(CatchClause {
                    binding: failure_name.as_str().into(),
                    body: Box::new(LashExpr::Fail(Box::new(LashExpr::Variable(
                        failure_name.as_str().into(),
                    )))),
                }),
                finally: None,
            })),
        }));
        self.process_bindings
            .insert(binding_name.to_string(), process_name.clone());
        Ok(LashExpr::ProcessRef {
            process: process_name.as_str().into(),
        })
    }

    fn lower_await(&mut self, inner: &Expr) -> Result<LashExpr, Diagnostic> {
        let promise_kind = match inner {
            Expr::Call { callee, args }
                if matches!(
                    callee.as_ref(),
                    Expr::Member {
                        object,
                        property: MemberProperty::Field(method),
                    } if matches!(object.as_ref(), Expr::Ident(name) if name == "Promise" && !self.has_binding(name))
                        && matches!(method.as_str(), "all" | "allSettled")
                ) =>
            {
                let Expr::Member {
                    property: MemberProperty::Field(method),
                    ..
                } = callee.as_ref()
                else {
                    unreachable!()
                };
                let [value] = args.as_slice() else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("Promise.{method} expects one iterable"),
                        None,
                    ));
                };
                if !matches!(value, Expr::Array(_)) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AwaitUnsupported,
                        format!("Promise.{method} currently requires an array iterable"),
                        None,
                    ));
                }
                Some((method.as_str(), value))
            }
            _ => None,
        };
        self.await_depth += 1;
        let (mode, lowered) = if let Some((mode, value)) = promise_kind {
            (Some(mode), self.lower_expr(value))
        } else {
            (None, self.lower_expr(inner))
        };
        self.await_depth -= 1;
        let lowered = lowered?;
        if mode.is_some() && has_unsupported_aggregate_effect(&lowered) {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled currently aggregate tool promises and resolved values; process and timer promises require separate await expressions",
                None,
            ));
        }
        if mode.is_some() && has_nested_aggregate_effect(&lowered) {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled tool promises must be top-level array elements",
                None,
            ));
        }
        if mode.is_some()
            && has_aggregate_effect_leaf(&lowered)
            && has_unbatchable_aggregate_value(&lowered)
        {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled cannot mix tool promises with computed function or assignment values in v1",
                None,
            ));
        }
        if matches!(
            lowered,
            LashExpr::SleepFor(_)
                | LashExpr::SleepUntil(_)
                | LashExpr::WaitSignal { .. }
                | LashExpr::SignalRun { .. }
                | LashExpr::Wake(_)
                | LashExpr::Finish(_)
                | LashExpr::Fail(_)
        ) {
            return Ok(lowered);
        }
        if mode == Some("allSettled") {
            let has_effect = has_aggregate_effect_leaf(&lowered);
            let settled = settle_aggregate_leaves(lowered);
            let values = if has_effect {
                LashExpr::Await(Box::new(settled))
            } else {
                settled
            };
            return Ok(all_settled_results(values));
        }
        if mode == Some("all") {
            if has_aggregate_effect_leaf(&lowered) {
                return Ok(LashExpr::Await(Box::new(unwrap_aggregate_leaves(lowered))));
            }
            return Ok(lowered);
        }
        if matches!(lowered, LashExpr::ReceiverCall { .. }) {
            return Ok(LashExpr::Await(Box::new(LashExpr::ResultUnwrap(Box::new(
                lowered,
            )))));
        }
        if matches!(lowered, LashExpr::StartProcess(_)) {
            return Ok(LashExpr::ResultUnwrap(Box::new(LashExpr::Await(Box::new(
                lowered,
            )))));
        }
        if matches!(
            &lowered,
            LashExpr::Variable(name) if self.process_handle_bindings.contains(name.as_str())
        ) {
            return Ok(LashExpr::ResultUnwrap(Box::new(LashExpr::Await(Box::new(
                lowered,
            )))));
        }
        Err(Diagnostic::new(
            DiagnosticCode::AwaitUnsupported,
            "await supports tools, process handles, sleep, waitSignal, and Promise.all/allSettled",
            None,
        ))
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
        if let Expr::Ident(owner) = object
            && is_known_runtime_global(owner)
            && !self.has_binding(owner)
        {
            let name = match property {
                MemberProperty::Field(field) => field.as_str(),
                MemberProperty::Index(_) => "computed property",
            };
            return Err(Diagnostic::new(
                DiagnosticCode::MethodUnsupported,
                format!("property `{owner}.{name}` is not in the TypeScript runtime surface"),
                None,
            ));
        }
        if let MemberProperty::Field(field) = property
            && let Some(mut path) = module_path(object)
            && path.first().is_some_and(|root| root == "trigger")
            && !self.has_binding("trigger")
        {
            path.push(field.clone());
            return Ok(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                path.into_iter().map(Into::into).collect(),
            )));
        }
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
        if let Expr::Ident(name) = callee
            && !self.has_binding(name)
        {
            return match (name.as_str(), args) {
                ("finish", [_]) if self.process_depth > 0 => Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    "finish is cell-only; return from defineProcess.run so enclosing finally blocks execute",
                    None,
                )),
                ("finish", [value]) => Ok(LashExpr::Finish(Box::new(self.lower_expr(value)?))),
                ("print", [value]) => Ok(LashExpr::Print(Box::new(self.lower_expr(value)?))),
                ("wake", [value]) => Ok(LashExpr::Wake(Box::new(self.lower_expr(value)?))),
                ("wake", [run, Expr::String(signal), payload]) => Ok(LashExpr::SignalRun {
                    run: Box::new(self.lower_expr(run)?),
                    name: signal.as_str().into(),
                    payload: Box::new(self.lower_expr(payload)?),
                }),
                ("sleep", [milliseconds]) if self.await_depth > 0 => {
                    Ok(LashExpr::SleepFor(Box::new(self.lower_expr(milliseconds)?)))
                }
                ("waitSignal", [Expr::String(name)]) if self.await_depth > 0 => {
                    Ok(LashExpr::WaitSignal {
                        name: name.as_str().into(),
                    })
                }
                ("start", [Expr::Ident(target)]) => self.lower_start(target, &[]),
                ("start", [Expr::Ident(target), Expr::Object(entries)]) => {
                    self.lower_start(target, entries)
                }
                ("registerTrigger", [config]) if self.await_depth > 0 => {
                    Ok(LashExpr::ReceiverCall {
                        receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                            vec!["triggers".into()],
                        ))),
                        operation: "register".into(),
                        args: vec![self.lower_expr(config)?],
                    })
                }
                ("defineProcess", _) => Err(Diagnostic::new(
                    DiagnosticCode::ProcessDefinitionNotTopLevel,
                    "defineProcess must initialize a top-level binding",
                    None,
                )),
                ("sleep" | "waitSignal" | "registerTrigger", _) if self.await_depth == 0 => {
                    Err(Diagnostic::new(
                        DiagnosticCode::AwaitRequired,
                        format!("agent primitive `{name}` requires await"),
                        None,
                    ))
                }
                (
                    "finish" | "print" | "wake" | "sleep" | "waitSignal" | "start"
                    | "registerTrigger",
                    _,
                ) => Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    format!("invalid arguments for agent primitive `{name}`"),
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
        if let Expr::Member {
            object,
            property: MemberProperty::Field(method),
        } = callee
        {
            if matches!(object.as_ref(), Expr::Ident(name) if name == "console") && method == "log"
            {
                if !self.has_binding("console") {
                    let mut lowered = args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter();
                    let joined = lowered.next().map_or_else(
                        || LashExpr::String("".into()),
                        |first| js_add(LashExpr::String("".into()), first),
                    );
                    let joined = lowered.fold(joined, |joined, value| {
                        js_add(js_add(joined, LashExpr::String(" ".into())), value)
                    });
                    return Ok(LashExpr::Print(Box::new(joined)));
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
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Date")
                && method == "now"
                && args.is_empty()
                && !self.has_binding("Date")
            {
                return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                    "now",
                ))));
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Math")
                && method == "random"
                && args.is_empty()
                && !self.has_binding("Math")
            {
                return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                    "random",
                ))));
            }
            if let Some(static_owner) = static_stdlib_owner(object)
                && !self.has_binding(static_owner)
                && is_static_stdlib_method(static_owner, method)
            {
                let mut builtin_args =
                    vec![LashExpr::String(format!("{static_owner}.{method}").into())];
                builtin_args.extend(
                    args.iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                );
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: builtin_args,
                });
            }
            // A literal receiver that cannot carry this method is decided here,
            // before any per-method lowering. `map` used to be routed ahead of
            // this check and so skipped it, leaving `"ab".map(f)` to fail at run
            // time with a shaping error instead of being named — one receiver
            // shape short of the classification claim.
            if is_instance_stdlib_method(method)
                && has_literal_stdlib_receiver(object)
                && !literal_supports_instance_method(object, method)
            {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!("method `{method}` is unavailable on this literal receiver"),
                    None,
                ));
            }
            // `map` drives a guest callback, so it cannot go through the
            // stdlib builtin: that exports every argument across the host
            // boundary, which refuses a function value. The VM already owns
            // functions, frames and an in-VM map driver, so lower to that.
            if method == "map" {
                return self.lower_array_map(object, args);
            }
            if is_instance_stdlib_method(method) {
                let mut builtin_args = vec![
                    LashExpr::String(method.as_str().into()),
                    self.lower_expr(object)?,
                ];
                builtin_args.extend(
                    args.iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                );
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: builtin_args,
                });
            }

            if method.starts_with(|character: char| character.is_ascii_uppercase())
                && module_path(object)
                    .and_then(|path| path.first().cloned())
                    .is_some_and(|root| !self.has_binding(&root))
            {
                return Ok(LashExpr::ReceiverCall {
                    receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                        module_path(object)
                            .expect("constructor path checked above")
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                    ))),
                    operation: method.as_str().into(),
                    args: args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<_, _>>()?,
                });
            }

            // Classify by method name before the tool-call branch. A receiver
            // that is not a module authority — a chained call, a local binding,
            // a computed member, a literal — can never dispatch a tool, so an
            // unadvertised method there is a missing method and must say so.
            // Falling through reported it as a tool call needing `await`, and
            // under `await` it lowered and failed at the host untyped.
            let receiver_is_module_authority = module_path(object)
                .and_then(|path| path.first().cloned())
                .is_some_and(|root| !self.has_binding(&root));
            if matches!(object.as_ref(), Expr::Ident(owner) if is_known_runtime_global(owner) && !self.has_binding(owner))
                || has_literal_stdlib_receiver(object)
                || (!receiver_is_module_authority && !is_instance_stdlib_method(method))
            {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!("method `{method}` is not in the TypeScript runtime surface"),
                    None,
                ));
            }

            if self.await_depth == 0 {
                return Err(Diagnostic::new(
                    DiagnosticCode::AwaitRequired,
                    format!(
                        "tool call `{method}` must appear under await or Promise.all/allSettled"
                    ),
                    None,
                ));
            }
            let receiver = if let Some(path) = module_path(object)
                && path
                    .first()
                    .is_some_and(|root| !self.has_binding(root.as_str()))
            {
                LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                    path.into_iter().map(Into::into).collect(),
                ))
            } else {
                self.lower_expr(object)?
            };
            return Ok(LashExpr::ReceiverCall {
                receiver: Box::new(receiver),
                operation: method.as_str().into(),
                args: args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
                    .collect::<Result<_, _>>()?,
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

    fn lower_start(
        &mut self,
        target: &str,
        entries: &[(String, Expr)],
    ) -> Result<LashExpr, Diagnostic> {
        let Some(process) = self.process_bindings.get(target).cloned() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessTargetStaticRequired,
                format!("`{target}` is not a top-level defineProcess binding"),
                None,
            ));
        };
        Ok(LashExpr::StartProcess(ProcessStartExpr {
            process: process.into(),
            args: entries
                .iter()
                .map(|(name, value)| Ok((name.as_str().into(), self.lower_expr(value)?)))
                .collect::<Result<_, Diagnostic>>()?,
        }))
    }
}

fn is_define_process_call(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Call { callee, .. }
            if matches!(callee.as_ref(), Expr::Ident(name) if name == "defineProcess")
    )
}

/// v1 lowers a closure's captures by value, so a cycle of hoisted function
/// declarations has no emission order: each member needs its peers' values
/// before any of them exists. Routing the cycle through a shared mutable frame
/// record would work in memory but builds a heap cycle reachable from a durable
/// root, which the durable encoding rejects — the program would run and then
/// fail to persist. The dialect therefore rejects the shape up front and names
/// the cycle it found.
fn reject_mutual_recursion(
    pending: &[PendingFunction],
    statements: &[Stmt],
    lowerer: &Lowerer,
) -> Result<(), Diagnostic> {
    let index_by_internal = pending
        .iter()
        .enumerate()
        .map(|(index, function)| (function.internal.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let edges = pending
        .iter()
        .map(|function| {
            function
                .captures
                .iter()
                .filter_map(|capture| index_by_internal.get(capture.as_str()).copied())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let Some(component) = strongly_connected_components(&edges)
        .into_iter()
        .find(|component| component.len() > 1)
    else {
        return Ok(());
    };

    // Report the cycle with the names the author wrote, not the mangled ones.
    let source_names = statements
        .iter()
        .filter_map(|statement| match statement {
            Stmt::Function { name, .. } => lowerer
                .binding(name)
                .ok()
                .map(|binding| (binding.internal.clone(), name.clone())),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let display = |member: usize| {
        let internal = &pending[member].internal;
        source_names.get(internal).unwrap_or(internal).clone()
    };
    let cycle = shortest_cycle_through(&edges, component[0])
        .into_iter()
        .map(display)
        .collect::<Vec<_>>()
        .join(" -> ");
    Err(Diagnostic::new(
        DiagnosticCode::MutualRecursionUnsupported,
        format!("mutually recursive function declarations are not supported in v1; cycle: {cycle}"),
        None,
    ))
}

fn reserved_identifier(name: &str) -> Diagnostic {
    Diagnostic::new(
        DiagnosticCode::ReservedIdentifier,
        format!(
            "`{name}` is reserved: identifiers starting with `{GENERATED_BINDING_PREFIX}` name the lowerer's generated bindings"
        ),
        None,
    )
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
